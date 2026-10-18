use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, Statement, params};

use crate::storage::{
    self, HashId, IndexDefinition, IndexTarget, OwnerKind, SchemaState, Snapshot, StandardIndexKind,
};

use super::super::super::{QueryError, QueryResult};
use super::super::equality::property_equality_key;
use super::cache::{
    NODE_OWNER_KIND, RANGE_FAMILY_NUMBER, RANGE_FAMILY_STRING, RELATIONSHIP_OWNER_KIND,
    RangeOrderKey, range_order_key,
};

mod controlled;
use crate::query::resources::{
    ExecutionControl, MemoryReservation, checked_add, checked_mul, map_entry_bytes,
};
pub(crate) use controlled::{
    build_persistent_generation, ensure_standard_indexes_for_commit_controlled,
};
pub(super) use controlled::{definition_workspace, equality_key};

#[cfg(not(test))]
const GENERATION_COPY_PAGE_SIZE: i64 = 4_096;
#[cfg(test)]
const GENERATION_COPY_PAGE_SIZE: i64 = 64;
const INDEX_ENCODING_VERSION: i64 = storage::STANDARD_INDEX_ENCODING_VERSION;
const CACHE_VALUE_INSERT_SQL: &str = "INSERT OR REPLACE INTO temp._lithograph_standard_index_cache_local\
     (snapshot_hash, index_name, owner_kind, owner_id, property_ordinal, token_id, value_blob, equality_blob, text_value,\
      sort_family, sort_number, sort_a, sort_b, sort_c, sort_text, point_crs, point_x, point_y, point_z)\
     VALUES(?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)";
const CACHE_LOOKUP_INSERT_SQL: &str = "INSERT OR REPLACE INTO temp._lithograph_standard_index_cache_local\
     (snapshot_hash, index_name, owner_kind, owner_id, property_ordinal, token_id, value_blob, text_value)\
     VALUES(?1, ?2, ?3, ?4, 0, ?5, NULL, NULL)";

#[derive(Debug, Clone, Copy)]
struct PersistentGeneration {
    id: i64,
    anchor: HashId,
}

pub(super) fn ensure_index_cache(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    control.check()?;
    let _definition = control.reserve(controlled::definition_workspace(index, control)?)?;
    let connection = snapshot.connection_for_query();
    ensure_cache_tables(connection)?;
    if cached_index_binding_ready(snapshot, index, control)? {
        ensure_cache_indexes(connection, index.kind)?;
        return Ok(());
    }
    if let Some(generation) = compatible_persistent_generation(snapshot, index, control)? {
        bind_persistent_generation(snapshot, index, generation, control)?;
        ensure_cache_indexes(connection, index.kind)?;
        return Ok(());
    }
    controlled::rebuild_local(snapshot, index, control)
}

fn cached_index_binding_ready(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let connection = snapshot.connection_for_query();
    let cached = connection
        .query_row(
            "SELECT generation_id, CASE WHEN length(generation_anchor)=32 THEN generation_anchor END FROM temp._lithograph_standard_index_cache_meta \
             WHERE snapshot_hash = ?1 AND index_name = ?2 AND complete = 1",
            params![snapshot.cache_identity().as_bytes().as_slice(), index.name],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
        )
        .optional()?;
    if let Some(binding) = cached.as_ref()
        && cached_index_binding_valid(connection, index, binding, control)?
    {
        return Ok(true);
    }
    if cached.is_some() {
        clear_query_index_cache(snapshot, index)?;
    }
    Ok(false)
}

fn bind_persistent_generation(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    generation: PersistentGeneration,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    prepare_persistent_generation_overlay(snapshot, index, generation, control)?;
    snapshot.connection_for_query().execute(
        "INSERT OR REPLACE INTO temp._lithograph_standard_index_cache_meta \
         (snapshot_hash, index_name, complete, generation_id, generation_anchor) \
         VALUES(?1, ?2, 1, ?3, ?4)",
        params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index.name,
            generation.id,
            generation.anchor.as_bytes().as_slice(),
        ],
    )?;
    Ok(())
}

fn cached_index_binding_valid(
    connection: &Connection,
    index: &IndexDefinition,
    binding: &(Option<i64>, Option<Vec<u8>>),
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let (Some(generation_id), Some(cached_anchor)) = binding else {
        return Ok(binding.0.is_none() && binding.1.is_none());
    };
    let expected = index.canonical_identity_blob()?;
    let mut statement = connection.prepare(
        "SELECT anchor_commit, definition_blob FROM main._lithograph_index_generations \
         WHERE generation_id = ?1 AND encoding_version = ?2 AND complete = 1",
    )?;
    let mut rows = statement.query(params![generation_id, INDEX_ENCODING_VERSION])?;
    let Some(row) = rows.next()? else {
        return Ok(false);
    };
    let anchor = row.get_ref(0)?.as_blob().map_err(rusqlite::Error::from)?;
    let definition = row.get_ref(1)?.as_blob().map_err(rusqlite::Error::from)?;
    if anchor != cached_anchor || definition.len() != expected.len() {
        return Ok(false);
    }
    for (actual, expected) in definition.chunks(64 * 1024).zip(expected.chunks(64 * 1024)) {
        control.check()?;
        if actual != expected {
            return Ok(false);
        }
    }
    Ok(true)
}

fn clear_query_index_cache(snapshot: &Snapshot<'_>, index: &IndexDefinition) -> QueryResult<()> {
    clear_query_index_tables(
        snapshot,
        index,
        &[
            "_lithograph_standard_index_cache_local",
            "_lithograph_standard_index_cache_meta",
            "_lithograph_standard_index_changed_owners",
        ],
    )
}

fn clear_query_index_tables(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    tables: &[&str],
) -> QueryResult<()> {
    let connection = snapshot.connection_for_query();
    for table in tables {
        connection.execute(
            &format!("DELETE FROM temp.{table} WHERE snapshot_hash = ?1 AND index_name = ?2"),
            params![snapshot.cache_identity().as_bytes().as_slice(), index.name],
        )?;
    }
    Ok(())
}

fn prepare_persistent_generation_overlay(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    generation: PersistentGeneration,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let connection = snapshot.connection_for_query();
    clear_query_index_tables(
        snapshot,
        index,
        &[
            "_lithograph_standard_index_cache_local",
            "_lithograph_standard_index_changed_owners",
        ],
    )?;
    if generation.anchor == snapshot.commit() && snapshot.query_local_layer().is_none() {
        return Ok(());
    }
    let mut changed_memory = control.reserve(0)?;
    let mut changed = BTreeSet::new();
    if generation.anchor != snapshot.commit() {
        let execution = crate::query::version::VersionExecution::new(connection, control);
        let delta =
            execution.touched_layer_between_commits(generation.anchor, snapshot.commit())?;
        relevant_changed_owners(
            snapshot,
            index,
            &delta,
            &mut changed,
            &mut changed_memory,
            control,
        )?;
    }
    if let Some(local) = snapshot.query_local_layer() {
        relevant_changed_owners(
            snapshot,
            index,
            local,
            &mut changed,
            &mut changed_memory,
            control,
        )?;
    }
    #[cfg(feature = "test-support")]
    crate::performance::record_changed_owners(changed.len());
    let mut insert = connection.prepare(
        "INSERT OR IGNORE INTO temp._lithograph_standard_index_changed_owners(\
             snapshot_hash, index_name, owner_kind, owner_id) VALUES(?1, ?2, ?3, ?4)",
    )?;
    for (owner_kind, owner_id) in &changed {
        control.work(1)?;
        insert.execute(params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index.name,
            owner_kind,
            owner_id
        ])?;
    }
    drop(insert);
    materialize_changed_owner_values(snapshot, index, &changed, control)
}

fn relevant_changed_owners(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    delta: &storage::LayerBuilder,
    changed: &mut BTreeSet<(i64, i64)>,
    memory: &mut MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let mut insert = |owner| -> QueryResult<()> {
        control.work(1)?;
        if !changed.contains(&owner) {
            memory.grow(map_entry_bytes::<(i64, i64)>()?)?;
            changed.insert(owner);
        }
        Ok(())
    };
    let connection = snapshot.connection_for_query();
    let (owner_kind, token, properties) = match &index.target {
        IndexTarget::NodeLookup => return Ok(()),
        IndexTarget::RelationshipLookup => {
            return changed_lookup_relationships(delta, &mut insert);
        }
        IndexTarget::NodeProperties { label, properties } => (
            OwnerKind::Node,
            storage::find_label(connection, label)?,
            properties,
        ),
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => (
            OwnerKind::Relationship,
            storage::find_relationship_type(connection, relationship_type)?,
            properties,
        ),
    };
    let _keys_memory = control.reserve(checked_mul(properties.len() as u64, 8)?)?;
    let keys = property_key_ids(snapshot, properties)?.unwrap_or_default();
    if owner_kind == OwnerKind::Node {
        changed_nodes(delta, token, control, &mut insert)?;
    } else {
        changed_relationships(delta, token, control, &mut insert)?;
    }
    changed_properties(delta, owner_kind, &keys, control, &mut insert)
}

fn changed_lookup_relationships(
    delta: &storage::LayerBuilder,
    insert: &mut impl FnMut((i64, i64)) -> QueryResult<()>,
) -> QueryResult<()> {
    for (relationship, _) in delta.relationship_changes() {
        insert((RELATIONSHIP_OWNER_KIND, relationship.id))?;
    }
    Ok(())
}

fn changed_nodes(
    delta: &storage::LayerBuilder,
    token: Option<i64>,
    control: &ExecutionControl<'_>,
    insert: &mut impl FnMut((i64, i64)) -> QueryResult<()>,
) -> QueryResult<()> {
    for (node, _) in delta.node_changes() {
        insert((NODE_OWNER_KIND, node))?;
    }
    for (node, label, _) in delta.label_changes() {
        control.work(1)?;
        if token == Some(label) {
            insert((NODE_OWNER_KIND, node))?;
        }
    }
    Ok(())
}

fn changed_relationships(
    delta: &storage::LayerBuilder,
    token: Option<i64>,
    control: &ExecutionControl<'_>,
    insert: &mut impl FnMut((i64, i64)) -> QueryResult<()>,
) -> QueryResult<()> {
    for (relationship, _) in delta.relationship_changes() {
        control.work(1)?;
        if token == Some(relationship.type_id) {
            insert((RELATIONSHIP_OWNER_KIND, relationship.id))?;
        }
    }
    Ok(())
}

fn changed_properties(
    delta: &storage::LayerBuilder,
    owner_kind: OwnerKind,
    keys: &[i64],
    control: &ExecutionControl<'_>,
    insert: &mut impl FnMut((i64, i64)) -> QueryResult<()>,
) -> QueryResult<()> {
    for (kind, owner, key, _) in delta.property_changes() {
        control.work(1)?;
        if kind == owner_kind && keys.contains(&key) {
            insert((kind as i64, owner))?;
        }
    }
    Ok(())
}

fn materialize_changed_owner_values(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    changed: &BTreeSet<(i64, i64)>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    match &index.target {
        IndexTarget::NodeLookup => Ok(()),
        IndexTarget::RelationshipLookup => {
            materialize_relationship_lookup_values(snapshot, index, changed, control)
        }
        IndexTarget::NodeProperties { label, properties } => {
            materialize_node_property_values(snapshot, index, label, properties, changed, control)
        }
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => materialize_relationship_property_values(
            snapshot,
            index,
            relationship_type,
            properties,
            changed,
            control,
        ),
    }
}

fn materialize_relationship_lookup_values(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    changed: &BTreeSet<(i64, i64)>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let mut insert = snapshot
        .connection_for_query()
        .prepare(CACHE_LOOKUP_INSERT_SQL)?;
    for (owner_kind, owner_id) in changed {
        control.work(1)?;
        if *owner_kind != RELATIONSHIP_OWNER_KIND {
            continue;
        }
        if let Some(relationship) = snapshot.relationship(*owner_id)? {
            insert.execute(params![
                snapshot.cache_identity().as_bytes().as_slice(),
                index.name,
                RELATIONSHIP_OWNER_KIND,
                relationship.id,
                relationship.type_id,
            ])?;
        }
    }
    Ok(())
}

fn materialize_node_property_values(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    label: &str,
    properties: &[String],
    changed: &BTreeSet<(i64, i64)>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let Some(label_id) = storage::find_label(snapshot.connection_for_query(), label)? else {
        return Ok(());
    };
    let _keys = control.reserve(checked_mul(properties.len() as u64, 8)?)?;
    let Some(keys) = property_key_ids(snapshot, properties)? else {
        return Ok(());
    };
    let mut insert = snapshot
        .connection_for_query()
        .prepare(CACHE_VALUE_INSERT_SQL)?;
    for (owner_kind, owner_id) in changed {
        control.work(1)?;
        if *owner_kind != NODE_OWNER_KIND
            || !snapshot.node_exists(*owner_id)?
            || !has_label(snapshot, *owner_id, label_id, control)?
        {
            continue;
        }
        controlled::insert_owner(
            snapshot,
            index,
            OwnerKind::Node,
            *owner_id,
            &keys,
            &mut insert,
            control,
        )?;
    }
    Ok(())
}

fn materialize_relationship_property_values(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    relationship_type: &str,
    properties: &[String],
    changed: &BTreeSet<(i64, i64)>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let _keys = control.reserve(checked_mul(properties.len() as u64, 8)?)?;
    let Some((type_id, keys)) =
        relationship_property_domain(snapshot, relationship_type, properties)?
    else {
        return Ok(());
    };
    let mut insert = snapshot
        .connection_for_query()
        .prepare(CACHE_VALUE_INSERT_SQL)?;
    for (owner_kind, owner_id) in changed {
        control.work(1)?;
        if *owner_kind != RELATIONSHIP_OWNER_KIND {
            continue;
        }
        let Some(relationship) = snapshot.relationship(*owner_id)? else {
            continue;
        };
        if relationship.type_id == type_id {
            controlled::insert_owner(
                snapshot,
                index,
                OwnerKind::Relationship,
                *owner_id,
                &keys,
                &mut insert,
                control,
            )?;
        }
    }
    Ok(())
}

fn has_label(
    snapshot: &Snapshot<'_>,
    node: i64,
    label: i64,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let mut found = false;
    let check = || control.check().map_err(QueryError::into_storage_error);
    snapshot.visit_node_labels_checked(node, &check, |candidate| {
        control.work(1)?;
        found |= candidate == label;
        Ok::<_, QueryError>(())
    })?;
    Ok(found)
}

fn require_persistent_index_storage(connection: &Connection) -> QueryResult<()> {
    if persistent_index_storage_available(connection)? {
        Ok(())
    } else {
        Err(QueryError::internal(
            "persistent Standard Index generation requires storage format 3",
        ))
    }
}

fn create_generation_manifest(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
) -> QueryResult<(i64, HashId)> {
    let connection = snapshot.connection_for_query();
    let (definition_blob, definition_hash) = index_definition_identity(index)?;
    if let Some(generation_id) = existing_generation_id(snapshot, definition_hash)? {
        delete_generation(connection, generation_id)?;
    }
    connection.execute(
        "INSERT INTO main._lithograph_index_generations(\
             anchor_commit, definition_hash, definition_blob, encoding_version, complete, \
             indexed_entities, entry_count, created_at) \
         VALUES(?1, ?2, ?3, ?4, 0, 0, 0, ?5)",
        params![
            snapshot.commit().as_bytes().as_slice(),
            definition_hash.as_bytes().as_slice(),
            definition_blob,
            INDEX_ENCODING_VERSION,
            chrono::Utc::now().timestamp_micros()
        ],
    )?;
    let generation_id = connection.last_insert_rowid();
    if generation_id <= 0 {
        return Err(QueryError::internal(
            "persistent Standard Index generation received an invalid identity",
        ));
    }
    Ok((generation_id, definition_hash))
}

fn existing_generation_id(
    snapshot: &Snapshot<'_>,
    definition_hash: HashId,
) -> QueryResult<Option<i64>> {
    snapshot
        .connection_for_query()
        .query_row(
            "SELECT generation_id FROM main._lithograph_index_generations \
             WHERE anchor_commit = ?1 AND definition_hash = ?2 AND encoding_version = ?3",
            params![
                snapshot.commit().as_bytes().as_slice(),
                definition_hash.as_bytes().as_slice(),
                INDEX_ENCODING_VERSION
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

fn delete_generation(connection: &Connection, generation_id: i64) -> QueryResult<()> {
    connection.execute(
        "DELETE FROM main._lithograph_index_entries WHERE generation_id = ?1",
        [generation_id],
    )?;
    connection.execute(
        "DELETE FROM main._lithograph_index_generations WHERE generation_id = ?1",
        [generation_id],
    )?;
    Ok(())
}

fn generation_counts(connection: &Connection, generation_id: i64) -> QueryResult<(i64, i64)> {
    let entry_count = connection.query_row(
        "SELECT count(*) FROM main._lithograph_index_entries WHERE generation_id = ?1",
        [generation_id],
        |row| row.get(0),
    )?;
    let indexed_entities = connection.query_row(
        "SELECT count(*) FROM (\
             SELECT owner_kind, owner_id FROM main._lithograph_index_entries \
             WHERE generation_id = ?1 GROUP BY owner_kind, owner_id\
         )",
        [generation_id],
        |row| row.get(0),
    )?;
    Ok((entry_count, indexed_entities))
}

fn publish_generation(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    generation_id: i64,
    indexed_entities: i64,
    entry_count: i64,
) -> QueryResult<()> {
    let connection = snapshot.connection_for_query();
    connection.execute(
        "UPDATE main._lithograph_index_generations \
         SET complete = 1, indexed_entities = ?2, entry_count = ?3 \
         WHERE generation_id = ?1 AND complete = 0",
        params![generation_id, indexed_entities, entry_count],
    )?;
    connection.execute(
        "UPDATE temp._lithograph_standard_index_cache_meta \
         SET generation_id = ?3, generation_anchor = ?4 \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND complete = 1",
        params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index.name,
            generation_id,
            snapshot.commit().as_bytes().as_slice(),
        ],
    )?;
    connection.execute(
        "DELETE FROM temp._lithograph_standard_index_cache_local \
         WHERE snapshot_hash = ?1 AND index_name = ?2",
        params![snapshot.cache_identity().as_bytes().as_slice(), index.name],
    )?;
    Ok(())
}

fn copy_local_generation_pages(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    generation_id: i64,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let connection = snapshot.connection_for_query();
    let mut cursor = (-1_i64, 0_i64, -1_i64);
    loop {
        check_index_build_interrupted(is_interrupted)?;
        let last = {
            let mut statement = connection.prepare(
                "SELECT owner_kind, owner_id, property_ordinal \
                 FROM temp._lithograph_standard_index_cache_local \
                 WHERE snapshot_hash = ?1 AND index_name = ?2 \
                   AND (owner_kind, owner_id, property_ordinal) > (?3, ?4, ?5) \
                 ORDER BY owner_kind, owner_id, property_ordinal LIMIT ?6",
            )?;
            let mut rows = statement.query(params![
                snapshot.cache_identity().as_bytes().as_slice(),
                index.name,
                cursor.0,
                cursor.1,
                cursor.2,
                GENERATION_COPY_PAGE_SIZE,
            ])?;
            let mut last = None;
            while let Some(row) = rows.next()? {
                last = Some((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ));
            }
            last
        };
        let Some(last) = last else {
            break;
        };
        connection.execute(
            "INSERT INTO main._lithograph_index_entries(\
                 generation_id, owner_kind, owner_id, property_ordinal, token_id, value_blob, \
                 equality_blob, text_value, sort_family, sort_number, sort_a, sort_b, sort_c, \
                 sort_text, point_crs, point_x, point_y, point_z) \
             SELECT ?6, owner_kind, owner_id, property_ordinal, token_id, value_blob, \
                    equality_blob, text_value, sort_family, sort_number, sort_a, sort_b, sort_c, \
                    sort_text, point_crs, point_x, point_y, point_z \
             FROM temp._lithograph_standard_index_cache_local \
             WHERE snapshot_hash = ?1 AND index_name = ?2 \
               AND (owner_kind, owner_id, property_ordinal) > (?3, ?4, ?5) \
               AND (owner_kind, owner_id, property_ordinal) <= (?7, ?8, ?9) \
             ORDER BY owner_kind, owner_id, property_ordinal",
            params![
                snapshot.cache_identity().as_bytes().as_slice(),
                index.name,
                cursor.0,
                cursor.1,
                cursor.2,
                generation_id,
                last.0,
                last.1,
                last.2,
            ],
        )?;
        cursor = last;
        check_index_build_interrupted(is_interrupted)?;
    }
    Ok(())
}

fn evict_old_generations(
    connection: &Connection,
    definition_hash: HashId,
    current_generation: i64,
) -> QueryResult<()> {
    let mut statement = connection.prepare(
        "SELECT generation_id FROM main._lithograph_index_generations \
         WHERE definition_hash = ?1 AND encoding_version = ?2 AND complete = 1 \
         ORDER BY (generation_id = ?3) DESC, created_at DESC, generation_id DESC",
    )?;
    let ids = statement
        .query_map(
            params![
                definition_hash.as_bytes().as_slice(),
                INDEX_ENCODING_VERSION,
                current_generation
            ],
            |row| row.get::<_, i64>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    for generation_id in ids.into_iter().skip(2) {
        connection.execute(
            "DELETE FROM main._lithograph_index_entries WHERE generation_id = ?1",
            [generation_id],
        )?;
        connection.execute(
            "DELETE FROM main._lithograph_index_generations WHERE generation_id = ?1",
            [generation_id],
        )?;
    }
    Ok(())
}

fn compatible_persistent_generation(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<PersistentGeneration>> {
    let connection = snapshot.connection_for_query();
    if !persistent_index_storage_available(connection)? {
        return Ok(None);
    }
    let (definition_blob, definition_hash) = index_definition_identity(index)?;
    let (candidates, _candidates_memory) =
        load_generation_candidates(connection, definition_hash, control)?;
    let mut current = snapshot.commit();
    loop {
        control.work(1)?;
        if let Some(generation) =
            valid_generation_candidate(current, &definition_blob, candidates.get(&current))
        {
            return Ok(Some(generation));
        }
        let parent: Option<Vec<u8>> = connection.query_row(
            "SELECT parent1 FROM main._lithograph_commits WHERE id=?1",
            [current.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        let Some(parent) = parent else {
            return Ok(None);
        };
        current = HashId::from_slice(&parent)?;
    }
}

type GenerationCandidate = (i64, Vec<u8>);

fn load_generation_candidates(
    connection: &Connection,
    definition_hash: HashId,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BTreeMap<HashId, GenerationCandidate>, MemoryReservation)> {
    let mut memory = control.reserve(0)?;
    let mut statement = connection.prepare(
        "SELECT generation_id, anchor_commit, definition_blob \
         FROM main._lithograph_index_generations \
         WHERE definition_hash = ?1 AND encoding_version = ?2 AND complete = 1",
    )?;
    let mut rows = statement.query(params![
        definition_hash.as_bytes().as_slice(),
        INDEX_ENCODING_VERSION
    ])?;
    let mut candidates = BTreeMap::new();
    while let Some(row) = rows.next()? {
        control.work(1)?;
        let (anchor, candidate) = read_generation_candidate(row, &mut memory, control)?;
        candidates.insert(anchor, candidate);
    }
    Ok((candidates, memory))
}

fn read_generation_candidate(
    row: &rusqlite::Row<'_>,
    memory: &mut MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<(HashId, GenerationCandidate)> {
    let anchor = HashId::from_slice(row.get_ref(1)?.as_blob().map_err(rusqlite::Error::from)?)?;
    let definition = row.get_ref(2)?.as_blob().map_err(rusqlite::Error::from)?;
    memory.grow(checked_add(
        map_entry_bytes::<GenerationCandidate>()?,
        definition.len() as u64,
    )?)?;
    let mut blob = Vec::with_capacity(definition.len());
    for chunk in definition.chunks(64 * 1024) {
        control.check()?;
        blob.extend_from_slice(chunk);
    }
    Ok((anchor, (row.get(0)?, blob)))
}

fn valid_generation_candidate(
    anchor: HashId,
    definition_blob: &[u8],
    candidate: Option<&GenerationCandidate>,
) -> Option<PersistentGeneration> {
    let (generation_id, stored_definition) = candidate?;
    if stored_definition.as_slice() != definition_blob {
        return None;
    }
    Some(PersistentGeneration {
        id: *generation_id,
        anchor,
    })
}

fn index_definition_identity(index: &IndexDefinition) -> QueryResult<(Vec<u8>, HashId)> {
    let definition_blob = index.canonical_identity_blob()?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"LITHOGRAPH_INDEX_DEFINITION_V1");
    hasher.update(&definition_blob);
    Ok((
        definition_blob,
        HashId::from_bytes(*hasher.finalize().as_bytes()),
    ))
}

fn persistent_index_storage_available(connection: &Connection) -> QueryResult<bool> {
    let storage_format: Option<i64> = connection
        .query_row(
            "SELECT storage_format FROM main._lithograph_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(storage_format) = storage_format {
        return Ok(storage_format >= 3);
    }
    let exists: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema \
         WHERE type = 'table' AND name = '_lithograph_index_generations')",
        [],
        |row| row.get(0),
    )?;
    Ok(exists == 1)
}

fn ensure_cache_tables(connection: &Connection) -> QueryResult<()> {
    connection.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _lithograph_standard_index_cache_meta(\
             snapshot_hash BLOB NOT NULL, index_name TEXT NOT NULL, complete INTEGER NOT NULL, \
             generation_id INTEGER, generation_anchor BLOB,\
             PRIMARY KEY(snapshot_hash, index_name)) WITHOUT ROWID;\
         CREATE TEMP TABLE IF NOT EXISTS _lithograph_standard_index_cache_local(\
             snapshot_hash BLOB NOT NULL, index_name TEXT NOT NULL, owner_kind INTEGER NOT NULL,\
             owner_id INTEGER NOT NULL, property_ordinal INTEGER NOT NULL, token_id INTEGER,\
             value_blob BLOB, equality_blob BLOB, text_value TEXT, sort_family INTEGER, sort_number NUMERIC,\
             sort_a INTEGER, sort_b INTEGER, sort_c INTEGER, sort_text TEXT,\
             point_crs INTEGER, point_x REAL, point_y REAL, point_z REAL,\
             PRIMARY KEY(snapshot_hash, index_name, owner_kind, owner_id, property_ordinal)) WITHOUT ROWID;\
         CREATE TEMP TABLE IF NOT EXISTS _lithograph_standard_index_cache_config(\
             id INTEGER PRIMARY KEY CHECK(id = 1), storage_format INTEGER NOT NULL);\
         CREATE TEMP TABLE IF NOT EXISTS _lithograph_standard_index_changed_owners(\
             snapshot_hash BLOB NOT NULL, index_name TEXT NOT NULL, owner_kind INTEGER NOT NULL, owner_id INTEGER NOT NULL,\
             PRIMARY KEY(snapshot_hash, index_name, owner_kind, owner_id)) WITHOUT ROWID;",
    )?;
    let storage_format = connection
        .query_row(
            "SELECT storage_format FROM main._lithograph_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(if persistent_index_storage_available(connection)? {
            3
        } else {
            2
        });
    let configured: Option<i64> = connection
        .query_row(
            "SELECT storage_format FROM temp._lithograph_standard_index_cache_config WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if configured != Some(storage_format) {
        connection.execute_batch("DROP VIEW IF EXISTS temp._lithograph_standard_index_cache")?;
        if storage_format >= 3 {
            connection.execute_batch(
                "CREATE TEMP VIEW _lithograph_standard_index_cache AS \
                 SELECT local.snapshot_hash, local.index_name, local.owner_kind, local.owner_id, \
                        local.property_ordinal, local.token_id, local.value_blob, local.equality_blob, \
                        local.text_value, local.sort_family, local.sort_number, local.sort_a, local.sort_b, \
                        local.sort_c, local.sort_text, local.point_crs, local.point_x, local.point_y, local.point_z \
                 FROM temp._lithograph_standard_index_cache_local AS local \
                 UNION ALL \
                 SELECT meta.snapshot_hash, meta.index_name, entries.owner_kind, entries.owner_id, \
                        entries.property_ordinal, entries.token_id, entries.value_blob, entries.equality_blob, \
                        entries.text_value, entries.sort_family, entries.sort_number, entries.sort_a, entries.sort_b, \
                        entries.sort_c, entries.sort_text, entries.point_crs, entries.point_x, entries.point_y, entries.point_z \
                 FROM temp._lithograph_standard_index_cache_meta AS meta \
                 JOIN main._lithograph_index_entries AS entries ON entries.generation_id = meta.generation_id \
                 WHERE meta.complete = 1 AND meta.generation_id IS NOT NULL \
                   AND NOT EXISTS (\
                     SELECT 1 FROM temp._lithograph_standard_index_changed_owners AS changed \
                     WHERE changed.snapshot_hash = meta.snapshot_hash AND changed.index_name = meta.index_name \
                       AND changed.owner_kind = entries.owner_kind AND changed.owner_id = entries.owner_id\
                   );",
            )?;
        } else {
            connection.execute_batch(
                "CREATE TEMP VIEW _lithograph_standard_index_cache AS \
                 SELECT snapshot_hash, index_name, owner_kind, owner_id, property_ordinal, token_id, \
                        value_blob, equality_blob, text_value, sort_family, sort_number, sort_a, sort_b, \
                        sort_c, sort_text, point_crs, point_x, point_y, point_z \
                 FROM temp._lithograph_standard_index_cache_local;",
            )?;
        }
        connection.execute(
            "INSERT OR REPLACE INTO temp._lithograph_standard_index_cache_config(id, storage_format) VALUES(1, ?1)",
            [storage_format],
        )?;
    }
    Ok(())
}

fn ensure_cache_indexes(connection: &Connection, kind: StandardIndexKind) -> QueryResult<()> {
    let sql = match kind {
        StandardIndexKind::Lookup => {
            "CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_token \
             ON _lithograph_standard_index_cache_local(snapshot_hash, index_name, owner_kind, token_id, owner_id);"
        }
        StandardIndexKind::Range => {
            "CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_equality ON _lithograph_standard_index_cache_local(snapshot_hash, index_name, owner_kind, property_ordinal, equality_blob, owner_id);\
             CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_range_number ON _lithograph_standard_index_cache_local(snapshot_hash, index_name, owner_kind, property_ordinal, sort_family, sort_number, owner_id);\
             CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_range_text ON _lithograph_standard_index_cache_local(snapshot_hash, index_name, owner_kind, property_ordinal, sort_family, sort_text, owner_id);\
             CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_range_tuple \
             ON _lithograph_standard_index_cache_local(snapshot_hash, index_name, owner_kind, property_ordinal, sort_family, sort_a, sort_b, sort_c, sort_text, owner_id);"
        }
        StandardIndexKind::Text => {
            "CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_equality \
             ON _lithograph_standard_index_cache_local(snapshot_hash, index_name, owner_kind, property_ordinal, equality_blob, owner_id);\
             CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_text \
             ON _lithograph_standard_index_cache_local(snapshot_hash, index_name, owner_kind, property_ordinal, text_value, owner_id);"
        }
        StandardIndexKind::Point => {
            "CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_equality \
             ON _lithograph_standard_index_cache_local(snapshot_hash, index_name, owner_kind, property_ordinal, equality_blob, owner_id);\
             CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_point_x \
             ON _lithograph_standard_index_cache_local(snapshot_hash, index_name, owner_kind, property_ordinal, point_crs, point_x, point_y, point_z, owner_id);\
             CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_point_y \
             ON _lithograph_standard_index_cache_local(snapshot_hash, index_name, owner_kind, property_ordinal, point_crs, point_y, point_x, point_z, owner_id);"
        }
        StandardIndexKind::FullText | StandardIndexKind::Vector | StandardIndexKind::Semantic => {
            return Ok(());
        }
    };
    connection.execute_batch(sql)?;
    Ok(())
}

fn relationship_property_domain(
    snapshot: &Snapshot<'_>,
    relationship_type: &str,
    properties: &[String],
) -> QueryResult<Option<(i64, Vec<i64>)>> {
    let Some(type_id) =
        storage::find_relationship_type(snapshot.connection_for_query(), relationship_type)?
    else {
        return Ok(None);
    };
    let Some(keys) = property_key_ids(snapshot, properties)? else {
        return Ok(None);
    };
    Ok(Some((type_id, keys)))
}

fn check_index_build_interrupted(is_interrupted: &dyn Fn() -> bool) -> QueryResult<()> {
    if is_interrupted() {
        Err(QueryError::interrupted())
    } else {
        Ok(())
    }
}

fn property_key_ids(
    snapshot: &Snapshot<'_>,
    properties: &[String],
) -> QueryResult<Option<Vec<i64>>> {
    let mut keys = Vec::with_capacity(properties.len());
    for property in properties {
        let Some(key) = storage::find_property_key(snapshot.connection_for_query(), property)?
        else {
            return Ok(None);
        };
        keys.push(key);
    }
    Ok(Some(keys))
}

fn cache_value_supported(kind: StandardIndexKind, value: &storage::PropertyValue) -> bool {
    match kind {
        StandardIndexKind::Lookup
        | StandardIndexKind::FullText
        | StandardIndexKind::Vector
        | StandardIndexKind::Semantic => false,
        StandardIndexKind::Text => matches!(value, storage::PropertyValue::String(_)),
        StandardIndexKind::Point => matches!(value, storage::PropertyValue::Point(_)),
        StandardIndexKind::Range => true,
    }
}
