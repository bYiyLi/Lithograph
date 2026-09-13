//! Graph identity, dictionary, reference, and checkpoint integrity checks.

use rusqlite::{Connection, OptionalExtension};

use super::integrity::IntegrityIssue;
use super::snapshot::Snapshot;
use super::{HashId, StorageResult};

pub(super) fn graph_integrity_issues(
    connection: &Connection,
) -> StorageResult<Vec<IntegrityIssue>> {
    let mut issues = Vec::new();
    check_branch_refs(connection, &mut issues)?;
    check_sequences(connection, &mut issues)?;
    check_dictionary_refs(connection, &mut issues)?;
    check_identity_history(connection, &mut issues)?;
    check_checkpoint_refs(connection, &mut issues)?;
    check_snapshot_invariants(connection, &mut issues)?;
    Ok(issues)
}

fn check_branch_refs(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    check_required_main(connection, issues)?;
    check_named_branches(connection, issues)?;
    check_tags(connection, issues)?;
    check_merge_session_refs(connection, issues)?;
    check_version_sidecar_refs(connection, issues)
}

fn check_required_main(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let main_exists: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main._lithograph_branches WHERE name = 'main')",
        [],
        |row| row.get(0),
    )?;
    if main_exists != 1 {
        issues.push(IntegrityIssue::new(
            "refs.missing_main",
            "initialized storage is missing the required main branch",
        ));
    }
    Ok(())
}

fn check_named_branches(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    check_named_ref_rows(
        connection,
        issues,
        "SELECT b.name, c.id IS NULL FROM main._lithograph_branches b LEFT JOIN main._lithograph_commits c ON c.id = b.commit_id ORDER BY b.name",
        NamedRefIntegrity {
            kind: "branch",
            invalid_code: "refs.invalid_branch_name",
            dangling_code: "refs.dangling_branch",
        },
    )
}

fn check_tags(connection: &Connection, issues: &mut Vec<IntegrityIssue>) -> StorageResult<()> {
    check_named_ref_rows(
        connection,
        issues,
        "SELECT t.name, c.id IS NULL FROM main._lithograph_tags t LEFT JOIN main._lithograph_commits c ON c.id = t.commit_id ORDER BY t.name",
        NamedRefIntegrity {
            kind: "tag",
            invalid_code: "refs.invalid_tag_name",
            dangling_code: "refs.dangling_tag",
        },
    )
}

struct NamedRefIntegrity<'a> {
    kind: &'a str,
    invalid_code: &'a str,
    dangling_code: &'a str,
}

fn check_named_ref_rows(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
    sql: &str,
    spec: NamedRefIntegrity<'_>,
) -> StorageResult<()> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
    })?;
    for row in rows {
        let (name, dangling) = row?;
        if super::validate_ref_name(&name).is_err() {
            issues.push(IntegrityIssue::new(
                spec.invalid_code,
                format!("{} {name:?} has an invalid ref name", spec.kind),
            ));
        }
        if dangling {
            issues.push(IntegrityIssue::new(
                spec.dangling_code,
                format!("{} {name:?} points to a missing Commit", spec.kind),
            ));
        }
    }
    Ok(())
}

fn check_merge_session_refs(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "SELECT s.id, co.id IS NULL, ct.id IS NULL FROM main._lithograph_merge_sessions s LEFT JOIN main._lithograph_commits co ON co.id = s.ours_commit LEFT JOIN main._lithograph_commits ct ON ct.id = s.theirs_commit ORDER BY s.id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, bool>(1)?,
            row.get::<_, bool>(2)?,
        ))
    })?;
    for row in rows {
        let (session, missing_ours, missing_theirs) = row?;
        if missing_ours || missing_theirs {
            issues.push(IntegrityIssue::new(
                "refs.dangling_merge_session",
                format!("Merge Session {session:?} points to a missing pinned Commit"),
            ));
        }
    }
    Ok(())
}

fn check_version_sidecar_refs(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let orphan_resolutions: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_merge_resolutions r LEFT JOIN main._lithograph_merge_sessions s ON s.id = r.session_id WHERE s.id IS NULL",
        [],
        |row| row.get(0),
    )?;
    if orphan_resolutions > 0 {
        issues.push(IntegrityIssue::new(
            "refs.orphan_merge_resolution",
            format!("found {orphan_resolutions} Merge Session resolution row(s) without a Session"),
        ));
    }
    let dangling_commit_data: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_commit_data d LEFT JOIN main._lithograph_commits c ON c.id = d.commit_id WHERE c.id IS NULL",
        [],
        |row| row.get(0),
    )?;
    if dangling_commit_data > 0 {
        issues.push(IntegrityIssue::new(
            "refs.dangling_commit_data",
            format!("found {dangling_commit_data} Commit Data row(s) for missing Commits"),
        ));
    }
    Ok(())
}

fn check_sequences(connection: &Connection, issues: &mut Vec<IntegrityIssue>) -> StorageResult<()> {
    for (kind, name, max_sql) in [
        (
            1_i64,
            "NodeId",
            "SELECT coalesce(max(node_id), 0) FROM main._lithograph_node_delta",
        ),
        (
            2_i64,
            "RelationshipId",
            "SELECT coalesce(max(relationship_id), 0) FROM main._lithograph_rel_delta",
        ),
        (
            3_i64,
            "LabelId",
            "SELECT coalesce(max(id), 0) FROM main._lithograph_labels",
        ),
        (
            4_i64,
            "RelationshipTypeId",
            "SELECT coalesce(max(id), 0) FROM main._lithograph_rel_types",
        ),
        (
            5_i64,
            "PropertyKeyId",
            "SELECT coalesce(max(id), 0) FROM main._lithograph_prop_keys",
        ),
        (
            6_i64,
            "LayerId",
            "SELECT coalesce(max(id), 0) FROM main._lithograph_layers",
        ),
    ] {
        check_sequence(connection, kind, name, max_sql, issues)?;
    }
    let unexpected: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_sequences WHERE kind NOT BETWEEN 1 AND 6",
        [],
        |row| row.get(0),
    )?;
    if unexpected > 0 {
        issues.push(IntegrityIssue::new(
            "identity.sequence_kind",
            format!("found {unexpected} unexpected identity sequence row(s)"),
        ));
    }
    Ok(())
}

fn check_sequence(
    connection: &Connection,
    kind: i64,
    name: &str,
    max_sql: &str,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let next_id = connection
        .query_row(
            "SELECT next_id FROM main._lithograph_sequences WHERE kind = ?1",
            [kind],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let Some(next_id) = next_id else {
        issues.push(IntegrityIssue::new(
            "identity.sequence_missing",
            format!("missing sequence for {name}"),
        ));
        return Ok(());
    };
    let max_id: i64 = connection.query_row(max_sql, [], |row| row.get(0))?;
    if next_id <= 0 || next_id <= max_id {
        issues.push(IntegrityIssue::new(
            "identity.sequence_regressed",
            format!("{name} sequence next_id {next_id} does not exceed committed max {max_id}"),
        ));
    }
    Ok(())
}

fn check_dictionary_refs(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    for (name, sql) in [
        (
            "label delta",
            "SELECT count(*) FROM main._lithograph_label_delta d LEFT JOIN main._lithograph_labels x ON x.id = d.label_id WHERE x.id IS NULL",
        ),
        (
            "relationship delta",
            "SELECT count(*) FROM main._lithograph_rel_delta d LEFT JOIN main._lithograph_rel_types x ON x.id = d.type_id WHERE x.id IS NULL",
        ),
        (
            "property delta",
            "SELECT count(*) FROM main._lithograph_property_delta d LEFT JOIN main._lithograph_prop_keys x ON x.id = d.key_id WHERE x.id IS NULL",
        ),
        (
            "checkpoint label",
            "SELECT count(*) FROM main._lithograph_cp_labels d LEFT JOIN main._lithograph_labels x ON x.id = d.label_id WHERE x.id IS NULL",
        ),
        (
            "checkpoint relationship",
            "SELECT count(*) FROM main._lithograph_cp_relationships d LEFT JOIN main._lithograph_rel_types x ON x.id = d.type_id WHERE x.id IS NULL",
        ),
        (
            "checkpoint property",
            "SELECT count(*) FROM main._lithograph_cp_properties d LEFT JOIN main._lithograph_prop_keys x ON x.id = d.key_id WHERE x.id IS NULL",
        ),
    ] {
        let count: i64 = connection.query_row(sql, [], |row| row.get(0))?;
        if count > 0 {
            issues.push(IntegrityIssue::new(
                "dictionary.dangling_id",
                format!("found {count} {name} row(s) with missing dictionary ids"),
            ));
        }
    }
    Ok(())
}

fn check_identity_history(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    // Repeated add operations for the same logical identity can be legitimate
    // when a later Layer restores or merges an existing historical element.
    // Non-reuse is guaranteed by the monotonic allocator; the invariant that
    // remains globally checkable from history is Relationship tuple stability.
    check_relationship_identity_stability(connection, issues)
}

fn check_relationship_identity_stability(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let changed: i64 = connection.query_row(
        "SELECT count(*) FROM (SELECT relationship_id FROM main._lithograph_rel_delta GROUP BY relationship_id HAVING min(source_id) != max(source_id) OR min(type_id) != max(type_id) OR min(target_id) != max(target_id))",
        [],
        |row| row.get(0),
    )?;
    if changed > 0 {
        issues.push(IntegrityIssue::new(
            "identity.relationship_mutated",
            format!("found {changed} RelationshipId value(s) with changing endpoints/type"),
        ));
    }
    Ok(())
}

fn check_checkpoint_refs(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "SELECT p.commit_id FROM main._lithograph_checkpoints p LEFT JOIN main._lithograph_commits c ON c.id = p.commit_id WHERE c.id IS NULL ORDER BY p.commit_id",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    for row in rows {
        issues.push(IntegrityIssue::new(
            "checkpoint.dangling_commit",
            format!("checkpoint {} refers to a missing Commit", hex_bytes(&row?)),
        ));
    }
    check_checkpoint_semantics(connection, issues)
}

fn check_checkpoint_semantics(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let mut statement = connection
        .prepare("SELECT commit_id FROM main._lithograph_checkpoints ORDER BY commit_id")?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    for row in rows {
        let bytes = row?;
        let Ok(commit) = HashId::from_slice(&bytes) else {
            continue;
        };
        if commit_exists(connection, commit)? {
            compare_checkpoint_semantics(connection, commit, issues);
        }
    }
    Ok(())
}

fn compare_checkpoint_semantics(
    connection: &Connection,
    commit: HashId,
    issues: &mut Vec<IntegrityIssue>,
) {
    let with_checkpoint =
        Snapshot::resolve(connection, commit).and_then(|snapshot| snapshot.semantic_hash());
    let rebuilt = Snapshot::resolve_without_target_checkpoint(connection, commit)
        .and_then(|snapshot| snapshot.semantic_hash());
    match (with_checkpoint, rebuilt) {
        (Ok(left), Ok(right)) if left == right => {}
        (Ok(left), Ok(right)) => issues.push(IntegrityIssue::new(
            "checkpoint.snapshot_mismatch",
            format!(
                "checkpoint for Commit {} hashes as {} but canonical replay hashes as {}",
                commit.to_hex(),
                left.to_hex(),
                right.to_hex()
            ),
        )),
        (Err(error), _) | (_, Err(error)) => issues.push(IntegrityIssue::new(
            "checkpoint.snapshot_invalid",
            format!(
                "checkpoint for Commit {} cannot be verified: {error}",
                commit.to_hex()
            ),
        )),
    }
}

fn commit_exists(connection: &Connection, commit: HashId) -> StorageResult<bool> {
    let exists: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main._lithograph_commits WHERE id = ?1)",
        [commit.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    Ok(exists == 1)
}

fn check_snapshot_invariants(
    connection: &Connection,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let mut statement =
        connection.prepare("SELECT id FROM main._lithograph_commits ORDER BY id")?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    for row in rows {
        let bytes = row?;
        let Ok(commit) = HashId::from_slice(&bytes) else {
            continue;
        };
        if let Err(error) = validate_snapshot(connection, commit) {
            issues.push(IntegrityIssue::new(
                "graph.snapshot_invalid",
                format!(
                    "Commit {} cannot resolve a valid graph: {error}",
                    commit.to_hex()
                ),
            ));
        }
    }
    Ok(())
}

fn validate_snapshot(connection: &Connection, commit: HashId) -> StorageResult<()> {
    Snapshot::resolve(connection, commit)?.validate_graph_invariants()
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}
