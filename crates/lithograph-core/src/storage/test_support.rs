//! Test-only large canonical fixture construction.
//!
//! This module is compiled only with the `test-support` Cargo feature. It is
//! intentionally not part of the release Extension runtime API. The Phase 10
//! scale gate uses it to create a canonical immutable Layer without first
//! materializing hundreds of millions of logical slots in Rust memory.

use rusqlite::{Connection, Statement, params};

use super::encoding::RecordHasher;
use super::identity::allocate_layer_id;
use super::layer::DeltaOp;
use super::property::PropertyColumns;
use super::schema::{commit_hash, schema_hash_for_commit};
use super::{
    CommitMetadata, HashId, OwnerKind, PropertyValue, RelationshipRecord, STORAGE_FORMAT,
    StorageError, StorageResult, VectorCoordinateType, VectorValue, allocate_node_id_range,
    allocate_relationship_id_range, branch_head, intern_label, intern_property_key,
    intern_relationship_type, root_commit,
};

/// Deterministic Phase 10 scale fixture dimensions.
#[derive(Debug, Clone, Copy)]
pub struct ScaleFixtureSpec {
    pub node_count: u64,
    pub relationship_count: u64,
    pub sample_document_count: u64,
    pub hub_relationship_count: u64,
    pub progress_interval: u64,
}

/// Canonical identities produced by [`seed_scale_fixture`].
#[derive(Debug, Clone, Copy)]
pub struct ScaleFixture {
    pub root: HashId,
    pub commit: HashId,
    pub first_node: i64,
    pub first_relationship: i64,
    pub scale_label: i64,
    pub document_label: i64,
    pub low_degree_label: i64,
    pub hub_label: i64,
    pub link_type: i64,
    pub hub_outgoing_count: u64,
}

#[derive(Debug, Clone, Copy)]
struct ScaleSeedIds {
    first_node: i64,
    first_relationship: i64,
    scale_label: i64,
    document_label: i64,
    low_degree_label: i64,
    hub_label: i64,
    link_type: i64,
    id_key: i64,
    text_key: i64,
    embedding_key: i64,
    layer_id: i64,
}

/// Creates one large append-only Commit for a disposable benchmark database.
///
/// The rows are written in the same canonical slot order as `LayerBuilder`, and
/// the Layer hash is streamed through LCE1 while the rows are inserted. The
/// caller must still run the normal integrity checker before using the fixture
/// as release evidence.
pub fn seed_scale_fixture(
    connection: &Connection,
    spec: ScaleFixtureSpec,
    mut progress: impl FnMut(&str, u64, u64),
) -> StorageResult<ScaleFixture> {
    if spec.node_count < 7 || spec.relationship_count == 0 {
        return Err(StorageError::corrupt(
            "scale fixture requires at least seven Nodes and one Relationship",
        ));
    }
    let sample_count = spec.sample_document_count.min(spec.node_count);
    let root = root_commit(connection)?;
    if branch_head(connection, "main")? != root {
        return Err(StorageError::corrupt(
            "scale fixture requires main to still reference Root Commit",
        ));
    }
    let commit_count: i64 =
        connection.query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })?;
    if commit_count != 1 {
        return Err(StorageError::corrupt(
            "scale fixture requires a freshly initialized database",
        ));
    }

    connection.execute_batch("SAVEPOINT lithograph_phase10_scale_seed")?;
    let outcome = seed_scale_fixture_inner(connection, spec, sample_count, root, &mut progress);
    match outcome {
        Ok(fixture) => {
            connection.execute_batch("RELEASE lithograph_phase10_scale_seed")?;
            Ok(fixture)
        }
        Err(error) => {
            connection.execute_batch(
                "ROLLBACK TO lithograph_phase10_scale_seed; RELEASE lithograph_phase10_scale_seed",
            )?;
            Err(error)
        }
    }
}

fn seed_scale_fixture_inner(
    connection: &Connection,
    spec: ScaleFixtureSpec,
    sample_count: u64,
    root: HashId,
    progress: &mut impl FnMut(&str, u64, u64),
) -> StorageResult<ScaleFixture> {
    let ids = prepare_scale_seed_ids(connection, spec)?;
    let mut hasher = scale_layer_hasher(spec, sample_count)?;
    seed_nodes(connection, ids, spec, &mut hasher, progress)?;
    seed_labels(connection, ids, spec, sample_count, &mut hasher, progress)?;
    let hub_extra = seed_relationships(connection, ids, spec, &mut hasher, progress)?;
    seed_properties(connection, ids, spec, sample_count, &mut hasher, progress)?;
    let layer_hash = hasher.finish();
    let commit = finalize_scale_commit(connection, ids.layer_id, root, spec, layer_hash)?;
    Ok(ScaleFixture {
        root,
        commit,
        first_node: ids.first_node,
        first_relationship: ids.first_relationship,
        scale_label: ids.scale_label,
        document_label: ids.document_label,
        low_degree_label: ids.low_degree_label,
        hub_label: ids.hub_label,
        link_type: ids.link_type,
        hub_outgoing_count: hub_extra,
    })
}

fn prepare_scale_seed_ids(
    connection: &Connection,
    spec: ScaleFixtureSpec,
) -> StorageResult<ScaleSeedIds> {
    Ok(ScaleSeedIds {
        first_node: allocate_node_id_range(connection, spec.node_count)?,
        first_relationship: allocate_relationship_id_range(connection, spec.relationship_count)?,
        scale_label: intern_label(connection, "ScaleNode")?,
        document_label: intern_label(connection, "ScaleDocument")?,
        low_degree_label: intern_label(connection, "ScaleLowDegree")?,
        hub_label: intern_label(connection, "ScaleHub")?,
        link_type: intern_relationship_type(connection, "SCALE_LINK")?,
        id_key: intern_property_key(connection, "scaleId")?,
        text_key: intern_property_key(connection, "text")?,
        embedding_key: intern_property_key(connection, "embedding")?,
        layer_id: allocate_layer_id(connection)?,
    })
}

fn scale_layer_hasher(spec: ScaleFixtureSpec, sample_count: u64) -> StorageResult<RecordHasher> {
    let field_count = spec
        .node_count
        .checked_mul(3)
        .and_then(|count| count.checked_add(spec.relationship_count))
        .and_then(|count| count.checked_add(sample_count.checked_mul(3)?))
        .and_then(|count| count.checked_add(2))
        .ok_or_else(|| StorageError::corrupt("scale fixture Layer field count overflow"))?;
    Ok(RecordHasher::new(
        "LAYER",
        usize::try_from(field_count)
            .map_err(|_| StorageError::corrupt("scale fixture exceeds addressable memory size"))?,
    ))
}

fn seed_nodes(
    connection: &Connection,
    ids: ScaleSeedIds,
    spec: ScaleFixtureSpec,
    hasher: &mut RecordHasher,
    progress: &mut impl FnMut(&str, u64, u64),
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_node_delta(layer_id, node_id, op) VALUES(?1, ?2, 1)",
    )?;
    for offset in 0..spec.node_count {
        let node_id = checked_identity(ids.first_node, offset, "NodeId")?;
        statement.execute(params![ids.layer_id, node_id])?;
        hash_node_record(hasher, node_id);
        report_seed_progress(
            progress,
            "nodes",
            offset,
            spec.node_count,
            spec.progress_interval,
        );
    }
    Ok(())
}

fn seed_labels(
    connection: &Connection,
    ids: ScaleSeedIds,
    spec: ScaleFixtureSpec,
    sample_count: u64,
    hasher: &mut RecordHasher,
    progress: &mut impl FnMut(&str, u64, u64),
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_label_delta(layer_id, node_id, label_id, op) VALUES(?1, ?2, ?3, 1)",
    )?;
    for offset in 0..spec.node_count {
        let node_id = checked_identity(ids.first_node, offset, "NodeId")?;
        statement.execute(params![ids.layer_id, node_id, ids.scale_label])?;
        hash_label_record(hasher, node_id, ids.scale_label);
        if offset < sample_count {
            statement.execute(params![ids.layer_id, node_id, ids.document_label])?;
            hash_label_record(hasher, node_id, ids.document_label);
        }
        if offset == 0 {
            statement.execute(params![ids.layer_id, node_id, ids.hub_label])?;
            hash_label_record(hasher, node_id, ids.hub_label);
        } else if offset == 1 {
            statement.execute(params![ids.layer_id, node_id, ids.low_degree_label])?;
            hash_label_record(hasher, node_id, ids.low_degree_label);
        }
        report_seed_progress(
            progress,
            "labels",
            offset,
            spec.node_count,
            spec.progress_interval,
        );
    }
    Ok(())
}

fn seed_relationships(
    connection: &Connection,
    ids: ScaleSeedIds,
    spec: ScaleFixtureSpec,
    hasher: &mut RecordHasher,
    progress: &mut impl FnMut(&str, u64, u64),
) -> StorageResult<u64> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_rel_delta(layer_id, relationship_id, source_id, type_id, target_id, op) VALUES(?1, ?2, ?3, ?4, ?5, 1)",
    )?;
    let chain_count = spec.relationship_count.min(4);
    let extra_count = spec.relationship_count.saturating_sub(chain_count);
    let hub_extra = spec.hub_relationship_count.min(extra_count);
    let distributed_start = chain_count.saturating_add(hub_extra);
    for offset in 0..spec.relationship_count {
        let relationship_id = checked_identity(ids.first_relationship, offset, "RelationshipId")?;
        let (source, target) = scale_relationship_endpoints(
            ids.first_node,
            spec,
            offset,
            chain_count,
            distributed_start,
        )?;
        statement.execute(params![
            ids.layer_id,
            relationship_id,
            source,
            ids.link_type,
            target
        ])?;
        hash_relationship_record(
            hasher,
            RelationshipRecord {
                id: relationship_id,
                source,
                type_id: ids.link_type,
                target,
            },
        );
        report_seed_progress(
            progress,
            "relationships",
            offset,
            spec.relationship_count,
            spec.progress_interval,
        );
    }
    Ok(hub_extra)
}

fn scale_relationship_endpoints(
    first_node: i64,
    spec: ScaleFixtureSpec,
    offset: u64,
    chain_count: u64,
    distributed_start: u64,
) -> StorageResult<(i64, i64)> {
    if offset < chain_count {
        return Ok((
            checked_identity(first_node, offset + 1, "source NodeId")?,
            checked_identity(first_node, offset + 2, "target NodeId")?,
        ));
    }
    if offset < distributed_start {
        return Ok((
            first_node,
            checked_identity(
                first_node,
                6 + (offset - chain_count) % (spec.node_count - 6),
                "target NodeId",
            )?,
        ));
    }
    let distributed = offset - distributed_start;
    let source_offset = 6 + distributed % (spec.node_count - 6);
    let target_offset = 6 + (source_offset - 5) % (spec.node_count - 6);
    Ok((
        checked_identity(first_node, source_offset, "source NodeId")?,
        checked_identity(first_node, target_offset, "target NodeId")?,
    ))
}

fn seed_properties(
    connection: &Connection,
    ids: ScaleSeedIds,
    spec: ScaleFixtureSpec,
    sample_count: u64,
    hasher: &mut RecordHasher,
    progress: &mut impl FnMut(&str, u64, u64),
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_property_delta(layer_id, owner_kind, owner_id, key_id, op, type_tag, int_value, real_value, text_value, blob_value, aux_value) VALUES(?1, 1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for offset in 0..spec.node_count {
        let node_id = checked_identity(ids.first_node, offset, "NodeId")?;
        insert_property(
            &mut statement,
            hasher,
            ids.layer_id,
            node_id,
            ids.id_key,
            PropertyValue::Integer(
                i64::try_from(offset + 1)
                    .map_err(|_| StorageError::corrupt("scaleId exceeds INTEGER64"))?,
            ),
        )?;
        if offset < sample_count {
            insert_document_properties(&mut statement, hasher, ids, node_id, offset)?;
        }
        report_seed_progress(
            progress,
            "properties",
            offset,
            spec.node_count,
            spec.progress_interval,
        );
    }
    Ok(())
}

fn insert_document_properties(
    statement: &mut Statement<'_>,
    hasher: &mut RecordHasher,
    ids: ScaleSeedIds,
    node_id: i64,
    offset: u64,
) -> StorageResult<()> {
    insert_property(
        statement,
        hasher,
        ids.layer_id,
        node_id,
        ids.text_key,
        PropertyValue::String(format!("phase10 scale document {offset} graph")),
    )?;
    let second: f64 = if offset.is_multiple_of(2) { 0.0 } else { 0.25 };
    insert_property(
        statement,
        hasher,
        ids.layer_id,
        node_id,
        ids.embedding_key,
        PropertyValue::Vector(VectorValue {
            coordinate_type: VectorCoordinateType::F64,
            dimension: 2,
            packed: [1.0_f64.to_le_bytes(), second.to_le_bytes()].concat(),
        }),
    )
}

fn finalize_scale_commit(
    connection: &Connection,
    layer_id: i64,
    root: HashId,
    spec: ScaleFixtureSpec,
    layer_hash: HashId,
) -> StorageResult<HashId> {
    connection.execute(
        "INSERT INTO main._lithograph_layers(id, hash) VALUES(?1, ?2)",
        params![layer_id, layer_hash.as_bytes().as_slice()],
    )?;
    let schema_hash = schema_hash_for_commit(connection, root)?;
    let metadata = CommitMetadata {
        author: Some("phase10-scale".to_owned()),
        message: Some(format!(
            "seed {} nodes / {} relationships",
            spec.node_count, spec.relationship_count
        )),
        committed_at: 1,
    };
    let commit = commit_hash(
        STORAGE_FORMAT,
        Some(root),
        None,
        layer_hash,
        schema_hash,
        &metadata,
    );
    connection.execute(
        "INSERT INTO main._lithograph_commits(id, format_version, parent1, parent2, layer_id, schema_hash, author, message, committed_at) VALUES(?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8)",
        params![
            commit.as_bytes().as_slice(),
            STORAGE_FORMAT,
            root.as_bytes().as_slice(),
            layer_id,
            schema_hash.as_bytes().as_slice(),
            metadata.author,
            metadata.message,
            metadata.committed_at,
        ],
    )?;
    let changed = connection.execute(
        "UPDATE main._lithograph_branches SET commit_id = ?2 WHERE name = 'main' AND commit_id = ?1",
        params![root.as_bytes().as_slice(), commit.as_bytes().as_slice()],
    )?;
    if changed != 1 {
        return Err(StorageError::BranchHeadMoved);
    }
    Ok(commit)
}

fn insert_property(
    statement: &mut Statement<'_>,
    hasher: &mut RecordHasher,
    layer_id: i64,
    owner_id: i64,
    key_id: i64,
    value: PropertyValue,
) -> StorageResult<()> {
    let columns = PropertyColumns::from_value(&value)?;
    statement.execute(params![
        layer_id,
        owner_id,
        key_id,
        columns.type_tag,
        columns.int_value,
        columns.real_value,
        columns.text_value.as_deref(),
        columns.blob_value.as_deref(),
        columns.aux_value.as_deref(),
    ])?;
    let owner_kind = (OwnerKind::Node as i64).to_le_bytes();
    let owner_id = owner_id.to_le_bytes();
    let key_id = key_id.to_le_bytes();
    let op = (DeltaOp::Add as i64).to_le_bytes();
    let type_tag = value.type_tag().to_le_bytes();
    let mut optional_type = [0_u8; 9];
    optional_type[0] = 1;
    optional_type[1..].copy_from_slice(&type_tag);
    let canonical = value.canonical_bytes()?;
    let mut optional_value = Vec::with_capacity(canonical.len() + 1);
    optional_value.push(1);
    optional_value.extend_from_slice(&canonical);
    hasher.record_field(
        "PROPERTY",
        &[
            &owner_kind,
            &owner_id,
            &key_id,
            &op,
            &optional_type,
            &optional_value,
        ],
    );
    Ok(())
}

fn hash_node_record(hasher: &mut RecordHasher, node_id: i64) {
    let node_id = node_id.to_le_bytes();
    let op = (DeltaOp::Add as i64).to_le_bytes();
    hasher.record_field("NODE", &[&node_id, &op]);
}

fn hash_label_record(hasher: &mut RecordHasher, node_id: i64, label_id: i64) {
    let node_id = node_id.to_le_bytes();
    let label_id = label_id.to_le_bytes();
    let op = (DeltaOp::Add as i64).to_le_bytes();
    hasher.record_field("LABEL", &[&node_id, &label_id, &op]);
}

fn hash_relationship_record(hasher: &mut RecordHasher, record: RelationshipRecord) {
    let id = record.id.to_le_bytes();
    let source = record.source.to_le_bytes();
    let type_id = record.type_id.to_le_bytes();
    let target = record.target.to_le_bytes();
    let op = (DeltaOp::Add as i64).to_le_bytes();
    hasher.record_field("REL", &[&id, &source, &type_id, &target, &op]);
}

fn checked_identity(first: i64, offset: u64, name: &str) -> StorageResult<i64> {
    let offset = i64::try_from(offset)
        .map_err(|_| StorageError::corrupt(format!("{name} offset exceeds INTEGER64")))?;
    first
        .checked_add(offset)
        .ok_or_else(|| StorageError::corrupt(format!("{name} exceeds INTEGER64")))
}

fn report_progress(
    progress: &mut impl FnMut(&str, u64, u64),
    phase: &str,
    current: u64,
    total: u64,
    interval: u64,
) {
    if current == total || (interval > 0 && current.is_multiple_of(interval)) {
        progress(phase, current, total);
    }
}

fn report_seed_progress(
    progress: &mut impl FnMut(&str, u64, u64),
    phase: &str,
    offset: u64,
    total: u64,
    interval: u64,
) {
    report_progress(progress, phase, offset + 1, total, interval);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rusqlite::Connection;

    use super::*;
    use crate::storage::{
        Snapshot, branch_head, create_storage_schema, initialize_connection_state, initialize_root,
        integrity_check, load_commit,
    };

    fn fresh_scale_storage() -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory SQLite must open");
        connection
            .execute_batch(
                "CREATE TABLE main._lithograph_meta(\
                     id INTEGER PRIMARY KEY CHECK(id=1),\
                     magic TEXT NOT NULL,\
                     database_id TEXT NOT NULL,\
                     storage_format INTEGER NOT NULL\
                 );\
                 INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format)\
                 VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000010', 2);",
            )
            .expect("metadata");
        create_storage_schema(&connection).expect("storage schema");
        initialize_root(&connection).expect("root");
        initialize_connection_state(&connection).expect("connection state");
        connection
    }

    #[test]
    fn small_scale_fixture_seeds_canonical_history_and_all_workload_shapes() {
        let connection = fresh_scale_storage();
        let mut progress = Vec::new();
        let fixture = seed_scale_fixture(
            &connection,
            ScaleFixtureSpec {
                node_count: 10,
                relationship_count: 20,
                sample_document_count: 3,
                hub_relationship_count: 5,
                progress_interval: 3,
            },
            |phase, current, total| progress.push((phase.to_owned(), current, total)),
        )
        .expect("scale fixture");

        assert_eq!(
            branch_head(&connection, "main").expect("main head"),
            fixture.commit
        );
        assert_eq!(fixture.hub_outgoing_count, 5);
        assert_eq!(
            load_commit(&connection, fixture.commit)
                .expect("commit")
                .parent1,
            Some(fixture.root)
        );

        let snapshot = Snapshot::resolve(&connection, fixture.commit).expect("snapshot");
        let mut node_count = 0_u64;
        snapshot
            .visit_nodes(|_| {
                node_count += 1;
                Ok(())
            })
            .expect("visit nodes");
        let mut relationship_count = 0_u64;
        snapshot
            .visit_relationships(|_| {
                relationship_count += 1;
                Ok(())
            })
            .expect("visit relationships");
        assert_eq!(node_count, 10);
        assert_eq!(relationship_count, 20);

        let phases = progress
            .iter()
            .map(|(phase, _, _)| phase.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            phases,
            BTreeSet::from(["labels", "nodes", "properties", "relationships"])
        );
        assert!(progress.iter().all(|(_, current, total)| current <= total));
        assert!(integrity_check(&connection).expect("integrity").is_empty());
    }

    #[test]
    fn scale_fixture_rejects_invalid_dimensions_without_mutating_root() {
        let connection = fresh_scale_storage();
        let root = branch_head(&connection, "main").expect("root head");
        let error = seed_scale_fixture(
            &connection,
            ScaleFixtureSpec {
                node_count: 6,
                relationship_count: 0,
                sample_document_count: 0,
                hub_relationship_count: 0,
                progress_interval: 0,
            },
            |_, _, _| {},
        )
        .expect_err("invalid dimensions must fail");
        assert!(error.to_string().contains("at least seven Nodes"));
        assert_eq!(branch_head(&connection, "main").expect("main head"), root);
    }
}
