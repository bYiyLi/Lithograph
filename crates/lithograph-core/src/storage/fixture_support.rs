//! Shared primitives for test-support scale fixture builders.

use rusqlite::{Connection, Statement, params};

use super::encoding::RecordHasher;
use super::layer::DeltaOp;
use super::property::PropertyColumns;
use super::schema::{commit_hash, schema_hash_for_commit};
use super::{
    CommitMetadata, HashId, OwnerKind, PropertyValue, STORAGE_FORMAT, StorageError, StorageResult,
};

pub(super) fn insert_fixture_label(
    statement: &mut Statement<'_>,
    hasher: &mut RecordHasher,
    layer_id: i64,
    node_id: i64,
    label_id: i64,
) -> StorageResult<()> {
    statement.execute(params![layer_id, node_id, label_id])?;
    hash_fixture_label(hasher, node_id, label_id);
    Ok(())
}

pub(super) fn insert_fixture_property(
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
    hash_fixture_property(hasher, owner_id, key_id, &value)
}

pub(super) fn hash_fixture_property(
    hasher: &mut RecordHasher,
    owner_id: i64,
    key_id: i64,
    value: &PropertyValue,
) -> StorageResult<()> {
    let owner_kind = (OwnerKind::Node as i64).to_le_bytes();
    let owner = owner_id.to_le_bytes();
    let key = key_id.to_le_bytes();
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
            &owner,
            &key,
            &op,
            &optional_type,
            &optional_value,
        ],
    );
    Ok(())
}

pub(super) fn hash_fixture_node(hasher: &mut RecordHasher, node_id: i64) {
    let node = node_id.to_le_bytes();
    let op = (DeltaOp::Add as i64).to_le_bytes();
    hasher.record_field("NODE", &[&node, &op]);
}

pub(super) fn hash_fixture_label(hasher: &mut RecordHasher, node_id: i64, label_id: i64) {
    let node = node_id.to_le_bytes();
    let label = label_id.to_le_bytes();
    let op = (DeltaOp::Add as i64).to_le_bytes();
    hasher.record_field("LABEL", &[&node, &label, &op]);
}

pub(super) fn checked_fixture_identity(first: i64, offset: u64, name: &str) -> StorageResult<i64> {
    let offset = i64::try_from(offset)
        .map_err(|_| StorageError::corrupt(format!("{name} offset exceeds INTEGER64")))?;
    first
        .checked_add(offset)
        .ok_or_else(|| StorageError::corrupt(format!("{name} exceeds INTEGER64")))
}

pub(super) fn report_fixture_progress(
    progress: &mut impl FnMut(&str, u64, u64),
    phase: &str,
    offset: u64,
    total: u64,
    interval: u64,
) {
    let current = offset + 1;
    if current == total || (interval > 0 && current.is_multiple_of(interval)) {
        progress(phase, current, total);
    }
}

pub(super) fn finalize_fixture_commit(
    connection: &Connection,
    layer_id: i64,
    root: HashId,
    layer_hash: HashId,
    metadata: &CommitMetadata,
) -> StorageResult<HashId> {
    connection.execute(
        "INSERT INTO main._lithograph_layers(id, hash) VALUES(?1, ?2)",
        params![layer_id, layer_hash.as_bytes().as_slice()],
    )?;
    let schema_hash = schema_hash_for_commit(connection, root)?;
    let commit = commit_hash(
        STORAGE_FORMAT,
        Some(root),
        None,
        layer_hash,
        schema_hash,
        metadata,
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
