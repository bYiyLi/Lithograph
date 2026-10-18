//! Allocation-checked Standard Index DDL builds. Values are borrowed one at a
//! time; a composite index is populated only after every property is present.

use std::io::{self, Write};

use super::*;
use crate::query::resources::{ExecutionControl, checked_add, checked_mul};
use crate::storage::{PropertyValue, VectorCoordinateType};

const BUILD_PAGE: usize = 64;
// A scan can retain base/overlay vectors, merge-tree nodes, and its output page.
// This bounds those fixed-size records before entering the existing scan.
const SCAN_RECORD_BYTES: u64 = 1_024;

#[cfg(test)]
mod tests;

pub(crate) fn ensure_standard_indexes_for_commit_controlled(
    connection: &Connection,
    commit: HashId,
    previous: &SchemaState,
    schema: &SchemaState,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let snapshot = crate::query::graph::resolve_snapshot_with_control(connection, commit, control)?;
    for (name, index) in &schema.indexes {
        control.check()?;
        if matches!(
            index.kind,
            StandardIndexKind::FullText | StandardIndexKind::Vector | StandardIndexKind::Semantic
        ) || matches!(index.target, IndexTarget::NodeLookup)
        {
            continue;
        }
        // canonical_eq and the generation manifest both serialize owned Schema
        // objects. Count borrowed JSON first, then hold their workspace claim.
        let _definition = control.reserve(definition_workspace(index, control)?)?;
        if unchanged(previous.indexes.get(name), index, control)? {
            continue;
        }
        rebuild_local(&snapshot, index, control)?;
        if persistent_index_storage_available(connection)? {
            publish_persistent(&snapshot, index, control)?;
        }
    }
    Ok(())
}

pub(crate) fn build_persistent_generation(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<(i64, i64)> {
    require_persistent_index_storage(snapshot.connection_for_query())?;
    let _definition = control.reserve(definition_workspace(index, control)?)?;
    rebuild_local(snapshot, index, control)?;
    publish_persistent(snapshot, index, control)
}

fn unchanged(
    previous: Option<&IndexDefinition>,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let Some(previous) = previous else {
        return Ok(false);
    };
    let _memory = control.reserve(definition_workspace(previous, control)?)?;
    let check = || control.check().map_err(QueryError::into_storage_error);
    let same = previous.canonical_eq_checked(index, &check)?;
    control.check()?;
    Ok(same)
}

pub(in crate::query::schema::index) fn definition_workspace(
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let mut counter = JsonCounter {
        bytes: 0,
        writes: 0,
        control,
        error: None,
    };
    if let Err(error) = serde_json::to_writer(&mut counter, index) {
        return Err(counter
            .error
            .unwrap_or_else(|| QueryError::internal(error.to_string())));
    }
    // Account for cloned definition strings/Vecs, JSON tree nodes, canonical
    // ordering, and two simultaneously alive JSON outputs during canonical_eq.
    checked_add(
        checked_add(
            checked_mul(counter.bytes, 32)?,
            checked_mul(counter.writes, 128)?,
        )?,
        4_096,
    )
}

struct JsonCounter<'a, 'b> {
    bytes: u64,
    writes: u64,
    control: &'a ExecutionControl<'b>,
    error: Option<QueryError>,
}

impl Write for JsonCounter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = self
            .control
            .work(1)
            .and_then(|()| self.control.bytes(bytes.len() as u64))
            .and_then(|()| {
                Ok((
                    checked_add(self.bytes, bytes.len() as u64)?,
                    checked_add(self.writes, 1)?,
                ))
            });
        match result {
            Ok((total, writes)) => {
                self.bytes = total;
                self.writes = writes;
                Ok(bytes.len())
            }
            Err(error) => {
                self.error = Some(error);
                Err(io::Error::other("index preflight failed"))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn rebuild_local(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let connection = snapshot.connection_for_query();
    ensure_cache_tables(connection)?;
    clear_query_index_cache(snapshot, index)?;
    #[cfg(feature = "test-support")]
    crate::performance::record_standard_index_build();
    match &index.target {
        IndexTarget::NodeLookup => {}
        IndexTarget::RelationshipLookup => {
            build_relationships(snapshot, index, None, &[], control)?
        }
        IndexTarget::NodeProperties { label, properties } => {
            build_node_properties(snapshot, index, label, properties, control)?;
        }
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => {
            build_relationship_properties(snapshot, index, relationship_type, properties, control)?;
        }
    }
    control.check()?;
    ensure_cache_indexes(connection, index.kind)?;
    connection.execute(
        "INSERT INTO temp._lithograph_standard_index_cache_meta \
         (snapshot_hash, index_name, complete, generation_id, generation_anchor) \
         VALUES(?1, ?2, 1, NULL, NULL)",
        params![snapshot.cache_identity().as_bytes().as_slice(), index.name],
    )?;
    Ok(())
}

fn build_node_properties(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    label: &str,
    properties: &[String],
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let Some(label) = storage::find_label(snapshot.connection_for_query(), label)? else {
        return Ok(());
    };
    let _keys = control.reserve(checked_mul(properties.len() as u64, 8)?)?;
    if let Some(keys) = property_key_ids(snapshot, properties)? {
        build_nodes(snapshot, index, label, &keys, control)?;
    }
    Ok(())
}

fn build_relationship_properties(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    relationship_type: &str,
    properties: &[String],
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let _keys = control.reserve(checked_mul(properties.len() as u64, 8)?)?;
    if let Some((type_id, keys)) =
        relationship_property_domain(snapshot, relationship_type, properties)?
    {
        build_relationships(snapshot, index, Some(type_id), &keys, control)?;
    }
    Ok(())
}

fn publish_persistent(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<(i64, i64)> {
    let connection = snapshot.connection_for_query();
    control.check()?;
    let (generation, definition) = create_generation_manifest(snapshot, index)?;
    copy_local_generation_pages(snapshot, index, generation, &|| control.check().is_err())?;
    control.check()?;
    let (entries, entities) = generation_counts(connection, generation)?;
    publish_generation(snapshot, index, generation, entities, entries)?;
    evict_old_generations(connection, definition, generation)?;
    control.check()?;
    Ok((generation, entities))
}

fn build_nodes(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    label: i64,
    keys: &[i64],
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let mut insert = snapshot
        .connection_for_query()
        .prepare(CACHE_VALUE_INSERT_SQL)?;
    let mut after = 0;
    loop {
        let _page_memory = control.reserve(checked_mul(BUILD_PAGE as u64, SCAN_RECORD_BYTES)?)?;
        let page = snapshot.scan_label_after(label, after, BUILD_PAGE)?;
        for owner in page.items {
            control.work(1)?;
            insert_owner(
                snapshot,
                index,
                OwnerKind::Node,
                owner,
                keys,
                &mut insert,
                control,
            )?;
        }
        let Some(next) = page.next_after else {
            return Ok(());
        };
        after = next;
    }
}

fn build_relationships(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    type_id: Option<i64>,
    keys: &[i64],
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let sql = if type_id.is_some() {
        CACHE_VALUE_INSERT_SQL
    } else {
        CACHE_LOOKUP_INSERT_SQL
    };
    let mut insert = snapshot.connection_for_query().prepare(sql)?;
    let mut after = 0;
    loop {
        let _page_memory = control.reserve(checked_mul(BUILD_PAGE as u64, SCAN_RECORD_BYTES)?)?;
        // Identity pages keep cancellation bounded when most overlay records
        // have another type: the typed scan filters before applying its limit.
        let page = snapshot.scan_relationships_after(after, BUILD_PAGE)?;
        for record in page.items {
            control.work(1)?;
            if type_id.is_some_and(|type_id| type_id != record.type_id) {
                continue;
            }
            if type_id.is_some() {
                insert_owner(
                    snapshot,
                    index,
                    OwnerKind::Relationship,
                    record.id,
                    keys,
                    &mut insert,
                    control,
                )?;
            } else {
                insert.execute(params![
                    snapshot.cache_identity().as_bytes().as_slice(),
                    index.name,
                    RELATIONSHIP_OWNER_KIND,
                    record.id,
                    record.type_id
                ])?;
            }
        }
        let Some(next) = page.next_after else {
            return Ok(());
        };
        after = next;
    }
}

pub(super) fn insert_owner(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    kind: OwnerKind,
    owner: i64,
    keys: &[i64],
    insert: &mut Statement<'_>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let check = || control.check().map_err(QueryError::into_storage_error);
    // Keep composite-index completeness without retaining a Vec of all values.
    for key in keys {
        let supported = snapshot.with_property_checked(kind, owner, *key, &check, |value| {
            Ok::<_, QueryError>(value.is_some_and(|value| cache_value_supported(index.kind, value)))
        })?;
        if !supported {
            return Ok(());
        }
    }
    for (ordinal, key) in keys.iter().enumerate() {
        snapshot.with_property_checked(kind, owner, *key, &check, |value| {
            let value =
                value.ok_or_else(|| QueryError::internal("index property changed during build"))?;
            insert_value(
                snapshot,
                index,
                [kind as i64, owner],
                ordinal,
                value,
                insert,
                control,
            )
        })?;
    }
    Ok(())
}

fn value_workspace(value: &PropertyValue, control: &ExecutionControl<'_>) -> QueryResult<u64> {
    let check = || control.check().map_err(QueryError::into_storage_error);
    let bytes = storage::property_value_memory_bytes(value, &check)?;
    // Includes canonical child/field/output buffers, equality-key growth and
    // conversion/range-key copies while the borrowed input remains live.
    checked_add(checked_mul(bytes, 16)?, 4_096)
}

fn order_key(
    value: &PropertyValue,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<RangeOrderKey>> {
    if index.kind != StandardIndexKind::Range {
        return Ok(None);
    }
    match value {
        PropertyValue::String(value) => Ok(Some(RangeOrderKey::Text(
            crate::query::expression::clone_string_controlled(value, Some(control))?,
        ))),
        PropertyValue::List(_)
        | PropertyValue::Vector(_)
        | PropertyValue::Point(_)
        | PropertyValue::Uuid(_)
        | PropertyValue::Duration { .. } => Ok(None),
        _ => {
            let (value, _memory) =
                crate::query::graph::borrowed_property_with_control(value, control)?;
            Ok(range_order_key(&value))
        }
    }
}

fn insert_value(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    owner: [i64; 2],
    ordinal: usize,
    value: &PropertyValue,
    insert: &mut Statement<'_>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let [owner_kind, owner_id] = owner;
    let _memory = control.reserve(value_workspace(value, control)?)?;
    let check = || control.check().map_err(QueryError::into_storage_error);
    let value_blob = value.canonical_bytes_checked(&check)?;
    let equality_blob = equality_key(value, control)?;
    let text_value = match value {
        PropertyValue::String(value) => Some(value.as_str()),
        _ => None,
    };
    let order = order_key(value, index, control)?;
    let (family, number, a, b, c, text) = match order {
        Some(RangeOrderKey::Number(number)) => (
            Some(RANGE_FAMILY_NUMBER),
            Some(number),
            None,
            None,
            None,
            None,
        ),
        Some(RangeOrderKey::Text(text)) => (
            Some(RANGE_FAMILY_STRING),
            None,
            None,
            None,
            None,
            Some(text),
        ),
        Some(RangeOrderKey::Tuple {
            family,
            a,
            b,
            c,
            text,
        }) => (Some(family), None, Some(a), Some(b), Some(c), Some(text)),
        None => (None, None, None, None, None, None),
    };
    let (crs, x, y, z) = match value {
        PropertyValue::Point(point) => (
            Some(point.crs),
            point.coordinates.first().copied(),
            point.coordinates.get(1).copied(),
            point.coordinates.get(2).copied(),
        ),
        _ => (None, None, None, None),
    };
    control.check()?;
    insert.execute(params![
        snapshot.cache_identity().as_bytes().as_slice(),
        index.name,
        owner_kind,
        owner_id,
        i64::try_from(ordinal).unwrap_or(i64::MAX),
        value_blob,
        equality_blob,
        text_value,
        family,
        number,
        a,
        b,
        c,
        text,
        crs,
        x,
        y,
        z
    ])?;
    Ok(())
}

fn append(output: &mut Vec<u8>, bytes: &[u8], control: &ExecutionControl<'_>) -> QueryResult<()> {
    for chunk in bytes.chunks(64 * 1024) {
        control.check()?;
        output.extend_from_slice(chunk);
    }
    Ok(())
}

pub(in crate::query::schema::index) fn equality_key(
    value: &PropertyValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<Vec<u8>>> {
    control.work(1)?;
    match value {
        PropertyValue::Integer(_) | PropertyValue::Float(_) | PropertyValue::Point(_) => {
            property_equality_key(value)
        }
        PropertyValue::List(values) => {
            let mut key = b"list".to_vec();
            for value in values {
                let Some(child) = equality_key(value, control)? else {
                    return Ok(None);
                };
                key.extend_from_slice(&(child.len() as u64).to_le_bytes());
                append(&mut key, &child, control)?;
            }
            Ok(Some(key))
        }
        PropertyValue::Vector(vector)
            if matches!(
                vector.coordinate_type,
                VectorCoordinateType::F32 | VectorCoordinateType::F64
            ) =>
        {
            vector_key(value, vector, control)
        }
        _ => typed_key(value, control).map(Some),
    }
}

fn typed_key(value: &PropertyValue, control: &ExecutionControl<'_>) -> QueryResult<Vec<u8>> {
    let check = || control.check().map_err(QueryError::into_storage_error);
    let canonical = value.canonical_bytes_checked(&check)?;
    let mut key = b"typed".to_vec();
    append(&mut key, &canonical, control)?;
    Ok(key)
}

fn vector_key(
    value: &PropertyValue,
    vector: &storage::VectorValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<Vec<u8>>> {
    let check = || control.check().map_err(QueryError::into_storage_error);
    let _ = value.canonical_bytes_checked(&check)?;
    let mut key = b"vector".to_vec();
    key.push(vector.coordinate_type as u8);
    key.extend_from_slice(&vector.dimension.to_le_bytes());
    let width = if vector.coordinate_type == VectorCoordinateType::F32 {
        4
    } else {
        8
    };
    for chunk in vector.packed.chunks_exact(width) {
        control.work(1)?;
        let number = if width == 4 {
            f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64
        } else {
            f64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ])
        };
        if number.is_nan() {
            return Ok(None);
        }
        if width == 4 {
            let bits = if number == 0.0 {
                0
            } else {
                (number as f32).to_bits()
            };
            key.extend_from_slice(&bits.to_le_bytes());
        } else {
            let bits = if number == 0.0 { 0 } else { number.to_bits() };
            key.extend_from_slice(&bits.to_le_bytes());
        }
    }
    Ok(Some(key))
}
