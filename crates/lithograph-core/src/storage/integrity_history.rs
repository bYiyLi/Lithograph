//! Immutable history/hash/reference integrity checks.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use super::encoding::hash;
use super::integrity::IntegrityIssue;
use super::layer::load_layer;
use super::schema::commit_hash;
use super::{CommitMetadata, HashId, STORAGE_FORMAT, StorageResult};

pub(super) fn history_integrity_issues(
    connection: &Connection,
) -> StorageResult<Vec<IntegrityIssue>> {
    let mut issues = Vec::new();
    let layers = check_layers(connection, &mut issues)?;
    let schemas = check_schemas(connection, &mut issues)?;
    let commits = load_commits(connection, &mut issues)?;
    check_commit_rows(&commits, &layers, &schemas, &mut issues);
    check_commit_dag(&commits, &mut issues);
    check_orphan_rows(connection, &mut issues)?;
    Ok(issues)
}

#[derive(Debug, Clone)]
struct CommitRecord {
    id: HashId,
    parent1: Option<HashId>,
    parent2: Option<HashId>,
    parent1_null: bool,
    parent2_null: bool,
    layer_id: i64,
    schema_hash: Option<HashId>,
    format_version: i64,
    metadata: CommitMetadata,
}

fn check_layers(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<BTreeMap<i64, HashId>> {
    let mut statement =
        connection.prepare("SELECT id, hash FROM main._lithograph_layers ORDER BY id")?;
    let mut rows = statement.query([])?;
    let mut layers = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let layer_id = row.get::<_, i64>(0)?;
        let bytes = row.get::<_, Vec<u8>>(1)?;
        let Ok(stored_hash) = HashId::from_slice(&bytes) else {
            issues.push(IntegrityIssue::new(
                "history.layer_hash_encoding",
                format!("Layer {layer_id} has a non-32-byte hash"),
            ));
            continue;
        };
        check_one_layer(connection, layer_id, stored_hash, issues);
        layers.insert(layer_id, stored_hash);
    }
    Ok(layers)
}

fn check_one_layer(
    connection: &Connection,
    layer_id: i64,
    stored_hash: HashId,
    issues: &mut Vec<IntegrityIssue>,
) {
    match load_layer(connection, layer_id).and_then(|layer| layer.content_hash()) {
        Ok(actual_hash) if actual_hash == stored_hash => {}
        Ok(actual_hash) => issues.push(IntegrityIssue::new(
            "history.layer_hash_mismatch",
            format!(
                "Layer {layer_id} stores {} but recomputes as {}",
                stored_hash.to_hex(),
                actual_hash.to_hex()
            ),
        )),
        Err(error) => issues.push(IntegrityIssue::new(
            "history.layer_payload_invalid",
            format!("Layer {layer_id} cannot be decoded: {error}"),
        )),
    }
}

fn check_schemas(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<BTreeSet<HashId>> {
    let mut statement = connection.prepare(
        "SELECT hash, canonical_blob FROM main._lithograph_schema_objects ORDER BY hash",
    )?;
    let mut rows = statement.query([])?;
    let mut schemas = BTreeSet::new();
    while let Some(row) = rows.next()? {
        let bytes = row.get::<_, Vec<u8>>(0)?;
        let blob = row.get::<_, Vec<u8>>(1)?;
        let Ok(stored_hash) = HashId::from_slice(&bytes) else {
            issues.push(IntegrityIssue::new(
                "history.schema_hash_encoding",
                "Schema object has a non-32-byte hash",
            ));
            continue;
        };
        let actual_hash = hash(&blob);
        if stored_hash != actual_hash {
            issues.push(IntegrityIssue::new(
                "history.schema_hash_mismatch",
                format!(
                    "Schema {} recomputes as {}",
                    stored_hash.to_hex(),
                    actual_hash.to_hex()
                ),
            ));
        }
        schemas.insert(stored_hash);
    }
    Ok(schemas)
}

fn load_commits(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<BTreeMap<HashId, CommitRecord>> {
    let mut statement = connection.prepare(
        "SELECT id, format_version, parent1, parent2, layer_id, schema_hash, author, message, committed_at FROM main._lithograph_commits ORDER BY id",
    )?;
    let mut rows = statement.query([])?;
    let mut commits = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let id_bytes = row.get::<_, Vec<u8>>(0)?;
        let Ok(id) = HashId::from_slice(&id_bytes) else {
            issues.push(IntegrityIssue::new(
                "history.commit_id_encoding",
                "Commit row has a non-32-byte id",
            ));
            continue;
        };
        let parent1_bytes = row.get::<_, Option<Vec<u8>>>(2)?;
        let parent2_bytes = row.get::<_, Option<Vec<u8>>>(3)?;
        let schema_bytes = row.get::<_, Vec<u8>>(5)?;
        let parent1 = decode_optional_hash(parent1_bytes.as_deref(), "parent1", id, issues);
        let parent2 = decode_optional_hash(parent2_bytes.as_deref(), "parent2", id, issues);
        let schema_hash = match HashId::from_slice(&schema_bytes) {
            Ok(hash) => Some(hash),
            Err(_) => {
                issues.push(IntegrityIssue::new(
                    "history.commit_schema_encoding",
                    format!("Commit {} has a non-32-byte schema hash", id.to_hex()),
                ));
                None
            }
        };
        commits.insert(
            id,
            CommitRecord {
                id,
                parent1,
                parent2,
                parent1_null: parent1_bytes.is_none(),
                parent2_null: parent2_bytes.is_none(),
                layer_id: row.get(4)?,
                schema_hash,
                format_version: row.get(1)?,
                metadata: CommitMetadata {
                    author: row.get(6)?,
                    message: row.get(7)?,
                    committed_at: row.get(8)?,
                },
            },
        );
    }
    Ok(commits)
}

fn decode_optional_hash(
    bytes: Option<&[u8]>,
    field: &str,
    commit: HashId,
    issues: &mut Vec<IntegrityIssue>,
) -> Option<HashId> {
    let bytes = bytes?;
    if let Ok(hash) = HashId::from_slice(bytes) {
        return Some(hash);
    }
    issues.push(IntegrityIssue::new(
        "history.commit_parent_encoding",
        format!("Commit {} has a non-32-byte {field}", commit.to_hex()),
    ));
    None
}

fn check_commit_rows(
    commits: &BTreeMap<HashId, CommitRecord>,
    layers: &BTreeMap<i64, HashId>,
    schemas: &BTreeSet<HashId>,
    issues: &mut Vec<IntegrityIssue>,
) {
    let mut root_count = 0_usize;
    for commit in commits.values() {
        if commit.parent1_null && commit.parent2_null {
            root_count += 1;
        }
        if commit.format_version != STORAGE_FORMAT {
            issues.push(IntegrityIssue::new(
                "history.commit_format",
                format!(
                    "Commit {} uses storage format {} instead of {}",
                    commit.id.to_hex(),
                    commit.format_version,
                    STORAGE_FORMAT
                ),
            ));
        }
        check_commit_references(commit, commits, layers, schemas, issues);
        check_commit_hash(commit, layers, issues);
    }
    if root_count != 1 {
        issues.push(IntegrityIssue::new(
            "history.root_count",
            format!("expected exactly one Root Commit, found {root_count}"),
        ));
    }
}

fn check_commit_references(
    commit: &CommitRecord,
    commits: &BTreeMap<HashId, CommitRecord>,
    layers: &BTreeMap<i64, HashId>,
    schemas: &BTreeSet<HashId>,
    issues: &mut Vec<IntegrityIssue>,
) {
    if commit.parent1_null && !commit.parent2_null {
        issues.push(IntegrityIssue::new(
            "history.parent_shape",
            format!("Commit {} has parent2 without parent1", commit.id.to_hex()),
        ));
    }
    for parent in [commit.parent1, commit.parent2].into_iter().flatten() {
        if !commits.contains_key(&parent) {
            issues.push(IntegrityIssue::new(
                "history.dangling_parent",
                format!(
                    "Commit {} references missing parent {}",
                    commit.id.to_hex(),
                    parent.to_hex()
                ),
            ));
        }
    }
    if !layers.contains_key(&commit.layer_id) {
        issues.push(IntegrityIssue::new(
            "history.dangling_layer",
            format!(
                "Commit {} references missing Layer {}",
                commit.id.to_hex(),
                commit.layer_id
            ),
        ));
    }
    if let Some(schema_hash) = commit.schema_hash
        && !schemas.contains(&schema_hash)
    {
        issues.push(IntegrityIssue::new(
            "history.dangling_schema",
            format!(
                "Commit {} references missing Schema {}",
                commit.id.to_hex(),
                schema_hash.to_hex()
            ),
        ));
    }
}

fn check_commit_hash(
    commit: &CommitRecord,
    layers: &BTreeMap<i64, HashId>,
    issues: &mut Vec<IntegrityIssue>,
) {
    if commit.format_version != STORAGE_FORMAT {
        return;
    }
    if (!commit.parent1_null && commit.parent1.is_none())
        || (!commit.parent2_null && commit.parent2.is_none())
    {
        return;
    }
    let Some(layer_hash) = layers.get(&commit.layer_id).copied() else {
        return;
    };
    let Some(schema_hash) = commit.schema_hash else {
        return;
    };
    let actual = commit_hash(
        commit.parent1,
        commit.parent2,
        layer_hash,
        schema_hash,
        &commit.metadata,
    );
    if actual != commit.id {
        issues.push(IntegrityIssue::new(
            "history.commit_hash_mismatch",
            format!(
                "Commit {} recomputes as {}",
                commit.id.to_hex(),
                actual.to_hex()
            ),
        ));
    }
}

fn check_commit_dag(commits: &BTreeMap<HashId, CommitRecord>, issues: &mut Vec<IntegrityIssue>) {
    let mut indegree = BTreeMap::new();
    let mut children: BTreeMap<HashId, Vec<HashId>> = BTreeMap::new();
    for commit in commits.values() {
        let mut count = 0_usize;
        for parent in [commit.parent1, commit.parent2].into_iter().flatten() {
            if commits.contains_key(&parent) {
                count += 1;
                children.entry(parent).or_default().push(commit.id);
            }
        }
        indegree.insert(commit.id, count);
    }
    let mut ready = BTreeSet::new();
    for (commit, degree) in &indegree {
        if *degree == 0 {
            ready.insert(*commit);
        }
    }
    let mut visited = 0_usize;
    while let Some(commit) = ready.pop_first() {
        visited += 1;
        if let Some(items) = children.get(&commit) {
            for child in items {
                if let Some(degree) = indegree.get_mut(child) {
                    *degree = degree.saturating_sub(1);
                    if *degree == 0 {
                        ready.insert(*child);
                    }
                }
            }
        }
    }
    if visited != commits.len() {
        issues.push(IntegrityIssue::new(
            "history.commit_cycle",
            "Commit graph contains a directed cycle",
        ));
    }
}

fn check_orphan_rows(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    for (name, table) in [
        ("node delta", "_lithograph_node_delta"),
        ("label delta", "_lithograph_label_delta"),
        ("relationship delta", "_lithograph_rel_delta"),
        ("property delta", "_lithograph_property_delta"),
    ] {
        let sql = format!(
            "SELECT count(*) FROM main.{table} d LEFT JOIN main._lithograph_layers l ON l.id = d.layer_id WHERE l.id IS NULL"
        );
        push_orphan_count(connection, name, &sql, issues)?;
    }
    for (name, table) in [
        ("checkpoint nodes", "_lithograph_cp_nodes"),
        ("checkpoint labels", "_lithograph_cp_labels"),
        ("checkpoint relationships", "_lithograph_cp_relationships"),
        ("checkpoint properties", "_lithograph_cp_properties"),
    ] {
        let sql = format!(
            "SELECT count(*) FROM main.{table} c LEFT JOIN main._lithograph_checkpoints p ON p.commit_id = c.commit_id WHERE p.commit_id IS NULL"
        );
        push_orphan_count(connection, name, &sql, issues)?;
    }
    Ok(())
}

fn push_orphan_count(
    connection: &Connection,
    name: &str,
    sql: &str,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let count: i64 = connection.query_row(sql, [], |row| row.get(0))?;
    if count > 0 {
        issues.push(IntegrityIssue::new(
            "history.orphan_rows",
            format!("found {count} orphan {name} row(s)"),
        ));
    }
    Ok(())
}
