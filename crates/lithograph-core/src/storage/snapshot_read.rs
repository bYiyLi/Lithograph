//! Storage-neutral allocation observation for query-owned Snapshot reads.

use std::collections::btree_map;
use std::fmt::Debug;
use std::iter::Peekable;
use std::rc::Rc;

use rusqlite::{Connection, Row, params, types::ValueRef};
use std::sync::Arc;

use super::layer::{DeltaOp, LayerBuilder, PropertyDelta};
use super::snapshot::{ResolvedSnapshotState, Snapshot, query_local_cache_identity_checked};

use super::property::{PropertyColumns, copy_text_checked};
use super::read_memory::{canonical_value_bytes, property_value_bytes};
use super::{
    HashId, OwnerKind, PropertyKeyId, PropertyValue, SchemaState, StorageError, StorageResult,
};

#[cfg(test)]
mod tests;

/// A lease is opaque to Storage. Its owner supplies the accounting and release
/// behavior, keeping the Snapshot independent of the query implementation.
pub(crate) trait SnapshotReadObserver: Debug {
    fn reserve(&self, bytes: u64) -> StorageResult<Box<dyn Debug>>;

    fn checkpoint(&self) -> StorageResult<()> {
        Ok(())
    }
}

pub(super) type ReadLease = Rc<dyn Debug>;
type OwnerOverlay<'a> =
    Peekable<btree_map::Range<'a, (OwnerKind, i64, PropertyKeyId), PropertyDelta>>;
type LabelOverlay<'a> = Peekable<btree_map::Range<'a, (i64, i64), DeltaOp>>;

#[derive(Debug)]
pub(super) struct ReadProperty {
    pub(super) value: PropertyValue,
    pub(super) memory: Option<ReadLease>,
}

pub(super) fn reserve(
    observer: Option<&dyn SnapshotReadObserver>,
    bytes: u64,
) -> StorageResult<Option<ReadLease>> {
    observer
        .map(|observer| observer.reserve(bytes).map(Rc::from))
        .transpose()
}

pub(super) fn checked_add(left: u64, right: u64) -> StorageResult<u64> {
    left.checked_add(right).ok_or_else(resource_overflow)
}

pub(super) fn resource_overflow() -> StorageError {
    StorageError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_TOOBIG),
        Some("query Snapshot allocation size overflow".to_owned()),
    ))
}

pub(super) fn reserve_value(
    value: &PropertyValue,
    observer: Option<&dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<Option<ReadLease>> {
    check()?;
    match observer {
        Some(observer) => reserve(
            Some(observer),
            checked_add(property_value_bytes(value, check)?, 256)?,
        ),
        None => Ok(None),
    }
}

/// Inspect SQLite-owned bytes before `Row::get` creates a String/Vec. The
/// temporary column copies and canonical decoder work coexist with the retained
/// decoded value, so both claims must be acquired before any copy.
pub(super) fn property_from_row(
    row: &Row<'_>,
    offset: usize,
    observer: Option<&dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<ReadProperty> {
    check()?;
    let (retained, temporary) = property_row_bytes(row, offset, check)?;
    let memory = reserve(observer, checked_add(retained, 256)?)?;
    let _temporary = reserve(observer, temporary)?;
    let columns = property_columns(row, offset, check)?;
    let value = columns.to_value_checked(check)?;
    check()?;
    Ok(ReadProperty { value, memory })
}

fn property_row_bytes(
    row: &Row<'_>,
    offset: usize,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<(u64, u64)> {
    let type_tag: i64 = row.get(offset)?;
    let text = row.get_ref(offset + 3)?;
    let blob = row.get_ref(offset + 4)?;
    let aux = row.get_ref(offset + 5)?;
    let raw = checked_add(
        checked_add(column_len(text), column_len(blob))?,
        column_len(aux),
    )?;
    let base = std::mem::size_of::<PropertyValue>() as u64;
    let (retained, decode_temporary) = match (type_tag, blob) {
        (5 | 11..=14, ValueRef::Blob(bytes)) => canonical_value_bytes(bytes, check)?,
        (4, ValueRef::Null) => (checked_add(base, column_len(text))?, 0),
        (10, ValueRef::Null) => (checked_add(base, column_len(aux).saturating_sub(4))?, 0),
        _ => (base, 0),
    };
    let temporary = checked_add(
        checked_add(raw, decode_temporary)?,
        std::mem::size_of::<PropertyColumns>() as u64,
    )?;
    Ok((retained, temporary))
}

fn property_columns(
    row: &Row<'_>,
    offset: usize,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<PropertyColumns> {
    Ok(PropertyColumns {
        type_tag: row.get(offset)?,
        int_value: row.get(offset + 1)?,
        real_value: row.get(offset + 2)?,
        text_value: owned_text_column(row, offset + 3, check)?,
        blob_value: owned_blob_column(row, offset + 4, check)?,
        aux_value: owned_blob_column(row, offset + 5, check)?,
    })
}

fn owned_text_column(
    row: &Row<'_>,
    column: usize,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<Option<String>> {
    match row.get_ref(column)? {
        ValueRef::Text(bytes) => {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| StorageError::corrupt("property text is not UTF-8"))?;
            copy_text_checked(text, check).map(Some)
        }
        _ => row.get(column).map_err(StorageError::from),
    }
}

fn owned_blob_column(
    row: &Row<'_>,
    column: usize,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<Option<Vec<u8>>> {
    match row.get_ref(column)? {
        ValueRef::Blob(bytes) => {
            let mut output = Vec::with_capacity(bytes.len());
            for chunk in bytes.chunks(64 * 1024) {
                check()?;
                output.extend_from_slice(chunk);
            }
            Ok(Some(output))
        }
        _ => row.get(column).map_err(StorageError::from),
    }
}

pub(super) fn layer_memory_bytes(
    layer: &LayerBuilder,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<u64> {
    let entries = layer.nodes.len() + layer.labels.len() + layer.relationships.len();
    let mut bytes = (entries as u64)
        .checked_mul(256)
        .ok_or_else(resource_overflow)?;
    for delta in layer.properties.values() {
        check()?;
        bytes = checked_add(bytes, 256)?;
        if let Some(value) = &delta.value {
            bytes = checked_add(bytes, property_value_bytes(value, check)?)?;
        }
    }
    Ok(bytes)
}

pub(crate) fn clone_value_checked(
    value: &PropertyValue,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<PropertyValue> {
    check()?;
    Ok(match value {
        PropertyValue::String(value) => PropertyValue::String(copy_text_checked(value, check)?),
        PropertyValue::List(values) => {
            let mut output = Vec::with_capacity(values.len());
            for value in values {
                output.push(clone_value_checked(value, check)?);
            }
            PropertyValue::List(output)
        }
        PropertyValue::ZonedDateTime(value) => {
            PropertyValue::ZonedDateTime(super::ZonedDateTimeValue {
                epoch_seconds: value.epoch_seconds,
                nanoseconds: value.nanoseconds,
                zone_id: copy_text_checked(&value.zone_id, check)?,
            })
        }
        PropertyValue::Point(value) => {
            let mut coordinates = Vec::with_capacity(value.coordinates.len());
            for chunk in value.coordinates.chunks(256) {
                check()?;
                coordinates.extend_from_slice(chunk);
            }
            PropertyValue::Point(super::PointValue {
                crs: value.crs,
                coordinates,
            })
        }
        PropertyValue::Vector(value) => {
            let mut packed = Vec::with_capacity(value.packed.len());
            for chunk in value.packed.chunks(64 * 1024) {
                check()?;
                packed.extend_from_slice(chunk);
            }
            PropertyValue::Vector(super::VectorValue {
                coordinate_type: value.coordinate_type,
                dimension: value.dimension,
                packed,
            })
        }
        value => value.clone(),
    })
}

pub(super) fn clone_layer_checked(
    layer: &LayerBuilder,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<LayerBuilder> {
    let mut output = LayerBuilder::default();
    for (key, op) in &layer.nodes {
        check()?;
        output.nodes.insert(*key, *op);
    }
    for (key, op) in &layer.labels {
        check()?;
        output.labels.insert(*key, *op);
    }
    for (key, delta) in &layer.relationships {
        check()?;
        output.relationships.insert(*key, *delta);
    }
    for (key, delta) in &layer.properties {
        check()?;
        output.properties.insert(
            *key,
            PropertyDelta {
                op: delta.op,
                value: delta
                    .value
                    .as_ref()
                    .map(|value| clone_value_checked(value, check))
                    .transpose()?,
            },
        );
    }
    Ok(output)
}

fn column_len(value: ValueRef<'_>) -> u64 {
    match value {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes.len() as u64,
        _ => 0,
    }
}

impl Snapshot<'_> {
    pub(crate) fn resolve_observed<'connection>(
        connection: &'connection Connection,
        commit: HashId,
        observer: Rc<dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<Snapshot<'connection>> {
        let state = Self::resolve_query_state_observed(connection, commit, observer, check)?;
        Ok(Snapshot::from_resolved_state(connection, &state))
    }

    pub(crate) fn resolve_with_layer_and_schema_observed<'connection>(
        connection: &'connection Connection,
        commit: HashId,
        layer: &LayerBuilder,
        schema: SchemaState,
        observer: Rc<dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<Snapshot<'connection>> {
        let state = Self::resolve_query_state_with_layer_and_schema_observed(
            connection, commit, layer, schema, observer, check,
        )?;
        Ok(Snapshot::from_resolved_state(connection, &state))
    }

    pub(super) fn read_checkpoint(&self) -> StorageResult<()> {
        self.state
            .read_observer
            .as_ref()
            .map_or(Ok(()), |observer| observer.checkpoint())
    }

    pub(crate) fn with_property<T, E: From<StorageError>>(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
        key_id: PropertyKeyId,
        visitor: impl FnOnce(Option<&PropertyValue>) -> Result<T, E>,
    ) -> Result<T, E> {
        self.with_property_checked(
            owner_kind,
            owner_id,
            key_id,
            &|| self.read_checkpoint(),
            visitor,
        )
    }

    pub(crate) fn with_property_checked<T, E: From<StorageError>>(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
        key_id: PropertyKeyId,
        check: &dyn Fn() -> StorageResult<()>,
        visitor: impl FnOnce(Option<&PropertyValue>) -> Result<T, E>,
    ) -> Result<T, E> {
        check()?;
        let key = (owner_kind, owner_id, key_id);
        if let Some(delta) = self.state.overlay.properties.get(&key) {
            return visitor(delta.value.as_ref());
        }
        let cache = self.state.property_cache.borrow();
        if let Some(value) = cache.get(&key) {
            return visitor(value.value.as_ref());
        }
        drop(cache);
        let Some(checkpoint) = self.state.checkpoint else {
            return visitor(None);
        };
        let mut statement = self
            .connection
            .prepare(
                "SELECT type_tag, int_value, real_value, text_value, blob_value, aux_value \
             FROM main._lithograph_cp_properties \
             WHERE commit_id=?1 AND owner_kind=?2 AND owner_id=?3 AND key_id=?4",
            )
            .map_err(StorageError::from)?;
        let mut rows = statement
            .query(params![
                checkpoint.as_bytes().as_slice(),
                owner_kind as i64,
                owner_id,
                key_id
            ])
            .map_err(StorageError::from)?;
        let Some(row) = rows.next().map_err(StorageError::from)? else {
            return visitor(None);
        };
        let property = property_from_row(row, 0, self.state.read_observer.as_deref(), check)?;
        visitor(Some(&property.value))
    }

    pub(crate) fn visit_owner_properties<E: From<StorageError>>(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
        visitor: impl FnMut(PropertyKeyId, &PropertyValue) -> Result<(), E>,
    ) -> Result<(), E> {
        self.visit_owner_properties_checked(
            owner_kind,
            owner_id,
            &|| self.read_checkpoint(),
            visitor,
        )
    }

    pub(crate) fn visit_owner_properties_checked<E: From<StorageError>>(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
        check: &dyn Fn() -> StorageResult<()>,
        mut visitor: impl FnMut(PropertyKeyId, &PropertyValue) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut overlay = self
            .state
            .overlay
            .properties
            .range((owner_kind, owner_id, 0)..=(owner_kind, owner_id, i64::MAX))
            .peekable();
        if let Some(checkpoint) = self.state.checkpoint {
            self.visit_owner_base_properties(
                checkpoint,
                owner_kind,
                owner_id,
                &mut overlay,
                check,
                &mut visitor,
            )?;
        }
        for (key, delta) in overlay {
            visit_delta_reference(key.2, delta, check, &mut visitor)?;
        }
        Ok(())
    }

    fn visit_owner_base_properties<E: From<StorageError>>(
        &self,
        checkpoint: HashId,
        owner_kind: OwnerKind,
        owner_id: i64,
        overlay: &mut OwnerOverlay<'_>,
        check: &dyn Fn() -> StorageResult<()>,
        visitor: &mut impl FnMut(PropertyKeyId, &PropertyValue) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut statement = self.connection.prepare(
            "SELECT key_id, type_tag, int_value, real_value, text_value, blob_value, aux_value FROM main._lithograph_cp_properties WHERE commit_id=?1 AND owner_kind=?2 AND owner_id=?3 ORDER BY key_id",
        ).map_err(StorageError::from)?;
        let mut rows = statement
            .query(params![
                checkpoint.as_bytes().as_slice(),
                owner_kind as i64,
                owner_id
            ])
            .map_err(StorageError::from)?;
        while let Some(row) = rows.next().map_err(StorageError::from)? {
            check()?;
            let key = row.get::<_, PropertyKeyId>(0).map_err(StorageError::from)?;
            if owner_overlay_through(key, overlay, check, visitor)? {
                continue;
            }
            let property = property_from_row(row, 1, self.state.read_observer.as_deref(), check)?;
            visitor(key, &property.value)?;
        }
        Ok(())
    }

    pub(crate) fn visit_node_labels<E: From<StorageError>>(
        &self,
        node_id: i64,
        visitor: impl FnMut(i64) -> Result<(), E>,
    ) -> Result<(), E> {
        self.visit_node_labels_checked(node_id, &|| self.read_checkpoint(), visitor)
    }

    pub(crate) fn visit_node_labels_checked<E: From<StorageError>>(
        &self,
        node_id: i64,
        check: &dyn Fn() -> StorageResult<()>,
        mut visitor: impl FnMut(i64) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut overlay = self
            .state
            .overlay
            .labels
            .range((node_id, 0)..=(node_id, i64::MAX))
            .peekable();
        if let Some(checkpoint) = self.state.checkpoint {
            self.visit_base_node_labels(checkpoint, node_id, &mut overlay, check, &mut visitor)?;
        }
        for (key, op) in overlay {
            check()?;
            if *op == DeltaOp::Add {
                visitor(key.1)?;
            }
        }
        Ok(())
    }

    fn visit_base_node_labels<E: From<StorageError>>(
        &self,
        checkpoint: HashId,
        node_id: i64,
        overlay: &mut LabelOverlay<'_>,
        check: &dyn Fn() -> StorageResult<()>,
        visitor: &mut impl FnMut(i64) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut statement = self.connection.prepare("SELECT label_id FROM main._lithograph_cp_labels WHERE commit_id=?1 AND node_id=?2 ORDER BY label_id").map_err(StorageError::from)?;
        let mut rows = statement
            .query(params![checkpoint.as_bytes().as_slice(), node_id])
            .map_err(StorageError::from)?;
        while let Some(row) = rows.next().map_err(StorageError::from)? {
            check()?;
            let label = row.get::<_, i64>(0).map_err(StorageError::from)?;
            if !label_overlay_through(label, overlay, check, visitor)? {
                visitor(label)?;
            }
        }
        Ok(())
    }

    pub(crate) fn resolve_query_state_observed(
        connection: &Connection,
        commit: HashId,
        observer: Rc<dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<ResolvedSnapshotState> {
        let mut state = resolve_observed_base(connection, commit, observer.clone(), check)?;
        let (schema, memory) =
            SchemaState::load_observed(connection, commit, observer.as_ref(), check)?;
        state.schema_override = Some(Arc::new(schema));
        state.schema_memory = Some(memory);
        Ok(state)
    }

    pub(crate) fn resolve_query_state_with_layer_and_schema_observed(
        connection: &Connection,
        commit: HashId,
        layer: &LayerBuilder,
        schema: SchemaState,
        observer: Rc<dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<ResolvedSnapshotState> {
        let mut state = resolve_observed_base(connection, commit, observer, check)?;
        let observer = state.read_observer.as_deref();
        let schema_memory = reserve(observer, super::schema_state_memory_bytes(&schema, check)?)?;
        let (local, local_memory) =
            super::snapshot_staged::prepare_local_layer(layer, observer, check)?;
        Rc::make_mut(&mut state.overlay).apply_layer_observed(layer, observer, check)?;
        state.query_local_layer = Some(Arc::new(local));
        state.local_memory = Some(local_memory);
        let hash_bytes = checked_add(
            layer_memory_bytes(layer, check)?,
            super::schema_state_memory_bytes(&schema, check)?,
        )?
        .checked_mul(4)
        .ok_or_else(resource_overflow)?;
        let _hash_memory = reserve(observer, hash_bytes)?;
        state.cache_identity =
            query_local_cache_identity_checked(commit, layer, Some(&schema), check)?;
        state.schema_override = Some(Arc::new(schema));
        state.schema_memory = schema_memory;
        Ok(state)
    }
}

fn visit_delta_reference<E: From<StorageError>>(
    key: PropertyKeyId,
    delta: &super::layer::PropertyDelta,
    check: &dyn Fn() -> StorageResult<()>,
    visitor: &mut impl FnMut(PropertyKeyId, &PropertyValue) -> Result<(), E>,
) -> Result<(), E> {
    check()?;
    if delta.op == DeltaOp::Add {
        let value = delta
            .value
            .as_ref()
            .ok_or_else(|| StorageError::corrupt("set property delta is missing value"))?;
        visitor(key, value)?;
    }
    Ok(())
}

fn owner_overlay_through<E: From<StorageError>>(
    key: i64,
    overlay: &mut OwnerOverlay<'_>,
    check: &dyn Fn() -> StorageResult<()>,
    visitor: &mut impl FnMut(PropertyKeyId, &PropertyValue) -> Result<(), E>,
) -> Result<bool, E> {
    while let Some((entry, delta)) = overlay.next_if(|(entry, _)| entry.2 <= key) {
        visit_delta_reference(entry.2, delta, check, visitor)?;
        if entry.2 == key {
            return Ok(true);
        }
    }
    Ok(false)
}

fn label_overlay_through<E: From<StorageError>>(
    label: i64,
    overlay: &mut LabelOverlay<'_>,
    check: &dyn Fn() -> StorageResult<()>,
    visitor: &mut impl FnMut(i64) -> Result<(), E>,
) -> Result<bool, E> {
    while let Some((entry, op)) = overlay.next_if(|(entry, _)| entry.1 <= label) {
        check()?;
        if *op == DeltaOp::Add {
            visitor(entry.1)?;
        }
        if entry.1 == label {
            return Ok(true);
        }
    }
    Ok(false)
}

fn resolve_observed_base(
    connection: &Connection,
    commit: HashId,
    observer: Rc<dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<ResolvedSnapshotState> {
    Snapshot::resolve_state_with_checkpoint_skip(connection, commit, None, Some(observer), check)
}
