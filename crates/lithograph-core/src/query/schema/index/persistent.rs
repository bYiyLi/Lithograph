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

const INDEX_BUILD_PAGE_SIZE: usize = 4_096;
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

pub(crate) fn ensure_standard_indexes_for_commit(
    connection: &Connection,
    commit: HashId,
    previous: &SchemaState,
    schema: &SchemaState,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let snapshot = Snapshot::resolve(connection, commit)?;
    for (name, index) in &schema.indexes {
        if previous.indexes.get(name) == Some(index) {
            continue;
        }
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        if !matches!(
            index.kind,
            StandardIndexKind::FullText | StandardIndexKind::Vector | StandardIndexKind::Semantic
        ) && !matches!(index.target, IndexTarget::NodeLookup)
        {
            if persistent_index_storage_available(connection)? {
                build_persistent_generation(&snapshot, index, is_interrupted)?;
            } else {
                ensure_index_cache(&snapshot, index)?;
            }
        }
    }
    Ok(())
}

pub(super) fn ensure_index_cache(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
) -> QueryResult<()> {
    let connection = snapshot.connection_for_query();
    ensure_cache_tables(connection)?;
    if cached_index_binding_ready(snapshot, index)? {
        ensure_cache_indexes(connection, index.kind)?;
        return Ok(());
    }
    if let Some(generation) = compatible_persistent_generation(snapshot, index)? {
        bind_persistent_generation(snapshot, index, generation)?;
        ensure_cache_indexes(connection, index.kind)?;
        return Ok(());
    }
    rebuild_local_index_cache(snapshot, index, &|| false)
}

fn cached_index_binding_ready(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
) -> QueryResult<bool> {
    let connection = snapshot.connection_for_query();
    let cached = connection
        .query_row(
            "SELECT generation_id, generation_anchor FROM temp._lithograph_standard_index_cache_meta \
             WHERE snapshot_hash = ?1 AND index_name = ?2 AND complete = 1",
            params![snapshot.cache_identity().as_bytes().as_slice(), index.name],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
        )
        .optional()?;
    if let Some(binding) = cached.as_ref()
        && cached_index_binding_valid(connection, index, binding)?
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
) -> QueryResult<()> {
    prepare_persistent_generation_overlay(snapshot, index, generation)?;
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
) -> QueryResult<bool> {
    let (Some(generation_id), Some(cached_anchor)) = binding else {
        return Ok(binding.0.is_none() && binding.1.is_none());
    };
    let manifest = connection
        .query_row(
            "SELECT anchor_commit, definition_blob FROM main._lithograph_index_generations \
             WHERE generation_id = ?1 AND encoding_version = ?2 AND complete = 1",
            params![generation_id, INDEX_ENCODING_VERSION],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let Some((anchor, definition_blob)) = manifest else {
        return Ok(false);
    };
    Ok(anchor == *cached_anchor && definition_blob == index.canonical_identity_blob()?)
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

fn rebuild_local_index_cache(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let connection = snapshot.connection_for_query();
    ensure_cache_tables(connection)?;
    clear_query_index_cache(snapshot, index)?;
    #[cfg(feature = "test-support")]
    crate::performance::record_standard_index_build();
    build_index_cache(snapshot, index, is_interrupted)?;
    ensure_cache_indexes(connection, index.kind)?;
    connection.execute(
        "INSERT INTO temp._lithograph_standard_index_cache_meta \
         (snapshot_hash, index_name, complete, generation_id, generation_anchor) \
         VALUES(?1, ?2, 1, NULL, NULL)",
        params![snapshot.cache_identity().as_bytes().as_slice(), index.name],
    )?;
    Ok(())
}

fn prepare_persistent_generation_overlay(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    generation: PersistentGeneration,
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
    let mut changed = if generation.anchor == snapshot.commit() {
        BTreeSet::new()
    } else {
        let delta = storage::touched_layer_between_commits(
            connection,
            generation.anchor,
            snapshot.commit(),
        )?;
        relevant_changed_owners(snapshot, index, &delta)?
    };
    if let Some(local) = snapshot.query_local_layer() {
        changed.extend(relevant_changed_owners(snapshot, index, local)?);
    }
    #[cfg(feature = "test-support")]
    crate::performance::record_changed_owners(changed.len());
    let mut insert = connection.prepare(
        "INSERT OR IGNORE INTO temp._lithograph_standard_index_changed_owners(\
             snapshot_hash, index_name, owner_kind, owner_id) VALUES(?1, ?2, ?3, ?4)",
    )?;
    for (owner_kind, owner_id) in &changed {
        insert.execute(params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index.name,
            owner_kind,
            owner_id
        ])?;
    }
    drop(insert);
    materialize_changed_owner_values(snapshot, index, &changed)
}

fn relevant_changed_owners(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    delta: &storage::LayerBuilder,
) -> QueryResult<BTreeSet<(i64, i64)>> {
    let mut changed = BTreeSet::new();
    match &index.target {
        IndexTarget::NodeLookup => {}
        IndexTarget::RelationshipLookup => {
            changed.extend(
                delta
                    .relationships
                    .keys()
                    .map(|relationship_id| (RELATIONSHIP_OWNER_KIND, *relationship_id)),
            );
        }
        IndexTarget::NodeProperties { label, properties } => {
            let label_id = storage::find_label(snapshot.connection_for_query(), label)?;
            let keys = property_key_ids(snapshot, properties)?.unwrap_or_default();
            changed.extend(
                delta
                    .nodes
                    .keys()
                    .map(|node_id| (NODE_OWNER_KIND, *node_id)),
            );
            if let Some(label_id) = label_id {
                changed.extend(
                    delta
                        .labels
                        .keys()
                        .filter(|(_, changed_label)| *changed_label == label_id)
                        .map(|(node_id, _)| (NODE_OWNER_KIND, *node_id)),
                );
            }
            changed.extend(
                delta
                    .properties
                    .keys()
                    .filter_map(|(kind, owner_id, key_id)| {
                        (*kind == OwnerKind::Node && keys.contains(key_id))
                            .then_some((NODE_OWNER_KIND, *owner_id))
                    }),
            );
        }
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => {
            let type_id = storage::find_relationship_type(
                snapshot.connection_for_query(),
                relationship_type,
            )?;
            let keys = property_key_ids(snapshot, properties)?.unwrap_or_default();
            if let Some(type_id) = type_id {
                changed.extend(delta.relationships.iter().filter_map(
                    |(relationship_id, relationship)| {
                        (relationship.record.type_id == type_id)
                            .then_some((RELATIONSHIP_OWNER_KIND, *relationship_id))
                    },
                ));
            }
            changed.extend(
                delta
                    .properties
                    .keys()
                    .filter_map(|(kind, owner_id, key_id)| {
                        (*kind == OwnerKind::Relationship && keys.contains(key_id))
                            .then_some((RELATIONSHIP_OWNER_KIND, *owner_id))
                    }),
            );
        }
    }
    Ok(changed)
}

fn materialize_changed_owner_values(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    changed: &BTreeSet<(i64, i64)>,
) -> QueryResult<()> {
    match &index.target {
        IndexTarget::NodeLookup => Ok(()),
        IndexTarget::RelationshipLookup => {
            materialize_relationship_lookup_values(snapshot, index, changed)
        }
        IndexTarget::NodeProperties { label, properties } => {
            materialize_node_property_values(snapshot, index, label, properties, changed)
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
        ),
    }
}

fn materialize_relationship_lookup_values(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    changed: &BTreeSet<(i64, i64)>,
) -> QueryResult<()> {
    let mut insert = snapshot
        .connection_for_query()
        .prepare(CACHE_LOOKUP_INSERT_SQL)?;
    for (owner_kind, owner_id) in changed {
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
) -> QueryResult<()> {
    let Some(label_id) = storage::find_label(snapshot.connection_for_query(), label)? else {
        return Ok(());
    };
    let Some(keys) = property_key_ids(snapshot, properties)? else {
        return Ok(());
    };
    let mut insert = snapshot
        .connection_for_query()
        .prepare(CACHE_VALUE_INSERT_SQL)?;
    for (owner_kind, owner_id) in changed {
        if *owner_kind != NODE_OWNER_KIND
            || !snapshot.node_exists(*owner_id)?
            || !snapshot.labels(*owner_id)?.contains(&label_id)
        {
            continue;
        }
        insert_owner_values(
            snapshot,
            index,
            NODE_OWNER_KIND,
            *owner_id,
            &keys,
            &mut insert,
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
) -> QueryResult<()> {
    let Some((type_id, keys)) =
        relationship_property_domain(snapshot, relationship_type, properties)?
    else {
        return Ok(());
    };
    let mut insert = snapshot
        .connection_for_query()
        .prepare(CACHE_VALUE_INSERT_SQL)?;
    for (owner_kind, owner_id) in changed {
        if *owner_kind != RELATIONSHIP_OWNER_KIND {
            continue;
        }
        let Some(relationship) = snapshot.relationship(*owner_id)? else {
            continue;
        };
        if relationship.type_id == type_id {
            insert_owner_values(
                snapshot,
                index,
                RELATIONSHIP_OWNER_KIND,
                *owner_id,
                &keys,
                &mut insert,
            )?;
        }
    }
    Ok(())
}

pub(crate) fn build_persistent_generation(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<(i64, i64)> {
    let connection = snapshot.connection_for_query();
    require_persistent_index_storage(connection)?;
    check_index_build_interrupted(is_interrupted)?;
    rebuild_local_index_cache(snapshot, index, is_interrupted)?;
    check_index_build_interrupted(is_interrupted)?;
    let (generation_id, definition_hash) = create_generation_manifest(snapshot, index)?;
    copy_local_generation_pages(snapshot, index, generation_id, is_interrupted)?;
    check_index_build_interrupted(is_interrupted)?;
    let (entry_count, indexed_entities) = generation_counts(connection, generation_id)?;
    publish_generation(
        snapshot,
        index,
        generation_id,
        indexed_entities,
        entry_count,
    )?;
    evict_old_generations(connection, definition_hash, generation_id)?;
    Ok((generation_id, indexed_entities))
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
) -> QueryResult<Option<PersistentGeneration>> {
    let connection = snapshot.connection_for_query();
    if !persistent_index_storage_available(connection)? {
        return Ok(None);
    }
    let (definition_blob, definition_hash) = index_definition_identity(index)?;
    let candidates = load_generation_candidates(connection, definition_hash)?;
    let mut current = snapshot.commit();
    loop {
        if let Some(generation) =
            valid_generation_candidate(current, &definition_blob, candidates.get(&current))
        {
            return Ok(Some(generation));
        }
        let record = storage::load_commit(connection, current)?;
        let Some(parent) = record.parent1 else {
            return Ok(None);
        };
        current = parent;
    }
}

type GenerationCandidate = (i64, Vec<u8>);

fn load_generation_candidates(
    connection: &Connection,
    definition_hash: HashId,
) -> QueryResult<BTreeMap<HashId, GenerationCandidate>> {
    let mut statement = connection.prepare(
        "SELECT generation_id, anchor_commit, definition_blob \
         FROM main._lithograph_index_generations \
         WHERE definition_hash = ?1 AND encoding_version = ?2 AND complete = 1",
    )?;
    let rows = statement.query_map(
        params![
            definition_hash.as_bytes().as_slice(),
            INDEX_ENCODING_VERSION
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        },
    )?;
    let mut candidates = BTreeMap::new();
    for row in rows {
        let (generation_id, anchor, stored_definition) = row?;
        let anchor = HashId::from_slice(&anchor)?;
        candidates.insert(anchor, (generation_id, stored_definition));
    }
    Ok(candidates)
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

fn build_index_cache(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    match &index.target {
        IndexTarget::NodeLookup => Ok(()),
        IndexTarget::RelationshipLookup => {
            build_relationship_lookup_cache(snapshot, index, is_interrupted)
        }
        IndexTarget::NodeProperties { label, properties } => {
            build_node_property_cache(snapshot, index, label, properties, is_interrupted)
        }
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => build_relationship_property_cache(
            snapshot,
            index,
            relationship_type,
            properties,
            is_interrupted,
        ),
    }
}

fn build_relationship_lookup_cache(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let mut insert = snapshot
        .connection_for_query()
        .prepare(CACHE_LOOKUP_INSERT_SQL)?;
    let mut after = 0_i64;
    loop {
        check_index_build_interrupted(is_interrupted)?;
        let page = snapshot.scan_relationships_after(after, INDEX_BUILD_PAGE_SIZE)?;
        for relationship in page.items {
            insert.execute(params![
                snapshot.cache_identity().as_bytes().as_slice(),
                index.name,
                RELATIONSHIP_OWNER_KIND,
                relationship.id,
                relationship.type_id,
            ])?;
        }
        let Some(next_after) = page.next_after else {
            break;
        };
        after = next_after;
    }
    Ok(())
}

fn build_node_property_cache(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    label: &str,
    properties: &[String],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let Some(label_id) = storage::find_label(snapshot.connection_for_query(), label)? else {
        return Ok(());
    };
    let Some(keys) = property_key_ids(snapshot, properties)? else {
        return Ok(());
    };
    let mut insert = snapshot
        .connection_for_query()
        .prepare(CACHE_VALUE_INSERT_SQL)?;
    let mut after = 0_i64;
    loop {
        check_index_build_interrupted(is_interrupted)?;
        let page =
            snapshot.label_property_values_after(label_id, &keys, after, INDEX_BUILD_PAGE_SIZE)?;
        for (node, values) in page.items {
            let Some(values) = values.into_iter().collect::<Option<Vec<_>>>() else {
                continue;
            };
            insert_owner_cache_values(snapshot, index, NODE_OWNER_KIND, node, values, &mut insert)?;
        }
        let Some(next_after) = page.next_after else {
            break;
        };
        after = next_after;
    }
    Ok(())
}

fn build_relationship_property_cache(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    relationship_type: &str,
    properties: &[String],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let Some((type_id, keys)) =
        relationship_property_domain(snapshot, relationship_type, properties)?
    else {
        return Ok(());
    };
    let mut insert = snapshot
        .connection_for_query()
        .prepare(CACHE_VALUE_INSERT_SQL)?;
    let mut after = 0_i64;
    loop {
        check_index_build_interrupted(is_interrupted)?;
        let page = snapshot.scan_relationships_after(after, INDEX_BUILD_PAGE_SIZE)?;
        for relationship in page.items {
            if relationship.type_id == type_id {
                insert_owner_values(
                    snapshot,
                    index,
                    RELATIONSHIP_OWNER_KIND,
                    relationship.id,
                    &keys,
                    &mut insert,
                )?;
            }
        }
        let Some(next_after) = page.next_after else {
            break;
        };
        after = next_after;
    }
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

fn insert_owner_values(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    owner_kind: i64,
    owner_id: i64,
    keys: &[i64],
    insert: &mut Statement<'_>,
) -> QueryResult<()> {
    let storage_owner = if owner_kind == NODE_OWNER_KIND {
        OwnerKind::Node
    } else {
        OwnerKind::Relationship
    };
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(value) = snapshot.property(storage_owner, owner_id, *key)? else {
            return Ok(());
        };
        values.push(value);
    }
    insert_owner_cache_values(snapshot, index, owner_kind, owner_id, values, insert)
}

fn insert_owner_cache_values(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    owner_kind: i64,
    owner_id: i64,
    values: Vec<storage::PropertyValue>,
    insert: &mut Statement<'_>,
) -> QueryResult<()> {
    if values
        .iter()
        .any(|value| !cache_value_supported(index.kind, value))
    {
        return Ok(());
    }
    for (ordinal, value) in values.into_iter().enumerate() {
        insert_cache_value(
            snapshot, index, owner_kind, owner_id, ordinal, value, insert,
        )?;
    }
    Ok(())
}

fn insert_cache_value(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    owner_kind: i64,
    owner_id: i64,
    ordinal: usize,
    value: storage::PropertyValue,
    insert: &mut Statement<'_>,
) -> QueryResult<()> {
    let value_blob = value.canonical_bytes()?;
    let equality_blob = property_equality_key(&value)?;
    let text_value = match &value {
        storage::PropertyValue::String(value) => Some(value.as_str()),
        _ => None,
    };
    let order_key = if index.kind == StandardIndexKind::Range {
        Some(super::super::super::graph::property_value(value.clone())?)
            .as_ref()
            .and_then(range_order_key)
    } else {
        None
    };
    let (point_crs, point_x, point_y, point_z) = match &value {
        storage::PropertyValue::Point(point) => (
            Some(point.crs),
            point.coordinates.first().copied(),
            point.coordinates.get(1).copied(),
            point.coordinates.get(2).copied(),
        ),
        _ => (None, None, None, None),
    };
    let (sort_family, sort_number, sort_a, sort_b, sort_c, sort_text) = match order_key {
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
    insert.execute(params![
        snapshot.cache_identity().as_bytes().as_slice(),
        index.name,
        owner_kind,
        owner_id,
        i64::try_from(ordinal).unwrap_or(i64::MAX),
        value_blob,
        equality_blob,
        text_value,
        sort_family,
        sort_number,
        sort_a,
        sort_b,
        sort_c,
        sort_text,
        point_crs,
        point_x,
        point_y,
        point_z,
    ])?;
    Ok(())
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
