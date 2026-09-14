//! Graph identity, dictionary, reference, and checkpoint integrity checks.

use rusqlite::{Connection, OptionalExtension};

use super::integrity::{IntegrityIssue, hex_bytes};
use super::layer::StoredLayerReferenceMaxima;
use super::snapshot::Snapshot;
use super::{HashId, StorageResult};

pub(super) fn graph_integrity_issues(
    connection: &Connection,
    references: &StoredLayerReferenceMaxima,
) -> StorageResult<Vec<IntegrityIssue>> {
    let mut issues = Vec::new();
    check_branch_refs(connection, &mut issues)?;
    check_sequences(connection, references, &mut issues)?;
    check_dictionary_refs(connection, references, &mut issues)?;
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

fn check_sequences(
    connection: &Connection,
    references: &StoredLayerReferenceMaxima,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    check_sequence_max(connection, 1, "NodeId", references.node_id, issues)?;
    check_sequence_max(
        connection,
        2,
        "RelationshipId",
        references.relationship_id,
        issues,
    )?;
    for (kind, name, max_sql) in [
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
        let max_id: i64 = connection.query_row(max_sql, [], |row| row.get(0))?;
        check_sequence_max(connection, kind, name, max_id, issues)?;
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

fn check_sequence_max(
    connection: &Connection,
    kind: i64,
    name: &str,
    max_id: i64,
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
    references: &StoredLayerReferenceMaxima,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    for (name, table, max_reference) in [
        ("LabelId", "_lithograph_labels", references.label_id),
        (
            "RelationshipTypeId",
            "_lithograph_rel_types",
            references.relationship_type_id,
        ),
        (
            "PropertyKeyId",
            "_lithograph_prop_keys",
            references.property_key_id,
        ),
    ] {
        let sql = format!(
            "SELECT count(*), coalesce(min(id), 0), coalesce(max(id), 0) FROM main.{table}"
        );
        let (count, min_id, max_id): (i64, i64, i64) =
            connection.query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        if (count == 0 && (min_id != 0 || max_id != 0))
            || (count > 0 && (min_id != 1 || count != max_id))
        {
            issues.push(IntegrityIssue::new(
                "dictionary.id_gap",
                format!("{name} dictionary is not a contiguous append-only 1..={max_id} id range"),
            ));
        }
        if max_reference > max_id {
            issues.push(IntegrityIssue::new(
                "dictionary.dangling_id",
                format!(
                    "canonical history references {name} {max_reference} but dictionary max id is {max_id}"
                ),
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
    visit_commit_ids(
        connection,
        "SELECT commit_id FROM main._lithograph_checkpoints ORDER BY commit_id",
        |commit| {
            if commit_exists(connection, commit)? {
                compare_checkpoint_semantics(connection, commit, issues);
            }
            Ok(())
        },
    )
}

fn visit_commit_ids(
    connection: &Connection,
    sql: &str,
    mut visit: impl FnMut(HashId) -> StorageResult<()>,
) -> StorageResult<()> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    for row in rows {
        let bytes = row?;
        if let Ok(commit) = HashId::from_slice(&bytes) {
            visit(commit)?;
        }
    }
    Ok(())
}

fn compare_checkpoint_semantics(
    connection: &Connection,
    commit: HashId,
    issues: &mut Vec<IntegrityIssue>,
) {
    match checkpoint_semantics_match(connection, commit) {
        Ok(true) => {}
        Ok(false) => issues.push(IntegrityIssue::new(
            "checkpoint.snapshot_mismatch",
            format!(
                "checkpoint for Commit {} differs from canonical first-parent replay",
                commit.to_hex()
            ),
        )),
        Err(error) => issues.push(IntegrityIssue::new(
            "checkpoint.snapshot_invalid",
            format!(
                "checkpoint for Commit {} cannot be verified: {error}",
                commit.to_hex()
            ),
        )),
    }
}

fn checkpoint_semantics_match(connection: &Connection, commit: HashId) -> StorageResult<bool> {
    let commit = commit.as_bytes().as_slice();
    for sql in [
        checkpoint_match_sql(
            "_lithograph_node_delta",
            "d.node_id",
            "d.node_id",
            "node_id",
            "_lithograph_cp_nodes",
        ),
        checkpoint_match_sql(
            "_lithograph_label_delta",
            "d.node_id, d.label_id",
            "d.node_id, d.label_id",
            "node_id, label_id",
            "_lithograph_cp_labels",
        ),
        checkpoint_match_sql(
            "_lithograph_rel_delta",
            "d.relationship_id, d.source_id, d.type_id, d.target_id",
            "d.relationship_id",
            "relationship_id, source_id, type_id, target_id",
            "_lithograph_cp_relationships",
        ),
        checkpoint_match_sql(
            "_lithograph_property_delta",
            "d.owner_kind, d.owner_id, d.key_id, d.type_tag, d.int_value, d.real_value, d.text_value, d.blob_value, d.aux_value",
            "d.owner_kind, d.owner_id, d.key_id",
            "owner_kind, owner_id, key_id, type_tag, int_value, real_value, text_value, blob_value, aux_value",
            "_lithograph_cp_properties",
        ),
    ] {
        let matches: bool = connection.query_row(&sql, [commit], |row| row.get(0))?;
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}

fn checkpoint_match_sql(
    delta_table: &str,
    ranked_columns: &str,
    partition_columns: &str,
    expected_columns: &str,
    checkpoint_table: &str,
) -> String {
    format!(
        r#"
WITH RECURSIVE lineage(commit_id, depth) AS (
  SELECT ?1, 0
  UNION ALL
  SELECT c.parent1, lineage.depth + 1
  FROM lineage
  JOIN main._lithograph_commits c ON c.id = lineage.commit_id
  WHERE c.parent1 IS NOT NULL
), ranked AS (
  SELECT {ranked_columns}, d.op,
         row_number() OVER (PARTITION BY {partition_columns} ORDER BY lineage.depth) AS rn
  FROM lineage
  JOIN main._lithograph_commits c ON c.id = lineage.commit_id
  JOIN main.{delta_table} d ON d.layer_id = c.layer_id
), expected AS (
  SELECT {expected_columns} FROM ranked WHERE rn = 1 AND op = 1
)
SELECT
  NOT EXISTS(SELECT 1 FROM (
    SELECT {expected_columns} FROM expected
    EXCEPT
    SELECT {expected_columns} FROM main.{checkpoint_table} WHERE commit_id = ?1
  ) LIMIT 1)
  AND
  NOT EXISTS(SELECT 1 FROM (
    SELECT {expected_columns} FROM main.{checkpoint_table} WHERE commit_id = ?1
    EXCEPT
    SELECT {expected_columns} FROM expected
  ) LIMIT 1)
"#
    )
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
    visit_commit_ids(
        connection,
        "SELECT id FROM main._lithograph_commits ORDER BY id",
        |commit| {
            let validation = if checkpoint_exists(connection, commit)? {
                validate_checkpoint_graph(connection, commit)
            } else {
                validate_snapshot(connection, commit)
            };
            if let Err(error) = validation {
                issues.push(IntegrityIssue::new(
                    "graph.snapshot_invalid",
                    format!(
                        "Commit {} cannot resolve a valid graph: {error}",
                        commit.to_hex()
                    ),
                ));
            }
            Ok(())
        },
    )
}

fn checkpoint_exists(connection: &Connection, commit: HashId) -> StorageResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_checkpoints WHERE commit_id = ?1)",
            [commit.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn validate_checkpoint_graph(connection: &Connection, commit: HashId) -> StorageResult<()> {
    let commit = commit.as_bytes().as_slice();
    let missing_endpoint: Option<i64> = connection
        .query_row(
            "SELECT r.relationship_id \
             FROM main._lithograph_cp_relationships r \
             LEFT JOIN main._lithograph_cp_nodes s ON s.commit_id = r.commit_id AND s.node_id = r.source_id \
             LEFT JOIN main._lithograph_cp_nodes t ON t.commit_id = r.commit_id AND t.node_id = r.target_id \
             WHERE r.commit_id = ?1 AND (s.node_id IS NULL OR t.node_id IS NULL) LIMIT 1",
            [commit],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(relationship_id) = missing_endpoint {
        return Err(super::StorageError::corrupt(format!(
            "RelationshipId {relationship_id} has a missing endpoint"
        )));
    }

    let missing_label_owner: Option<(i64, i64)> = connection
        .query_row(
            "SELECT l.node_id, l.label_id \
             FROM main._lithograph_cp_labels l \
             LEFT JOIN main._lithograph_cp_nodes n ON n.commit_id = l.commit_id AND n.node_id = l.node_id \
             WHERE l.commit_id = ?1 AND n.node_id IS NULL LIMIT 1",
            [commit],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((node_id, label_id)) = missing_label_owner {
        return Err(super::StorageError::corrupt(format!(
            "LabelId {label_id} refers to missing NodeId {node_id}"
        )));
    }

    let missing_property_owner: Option<(i64, i64, i64)> = connection
        .query_row(
            "SELECT p.owner_kind, p.owner_id, p.key_id \
             FROM main._lithograph_cp_properties p \
             LEFT JOIN main._lithograph_cp_nodes n ON p.owner_kind = 1 AND n.commit_id = p.commit_id AND n.node_id = p.owner_id \
             LEFT JOIN main._lithograph_cp_relationships r ON p.owner_kind = 2 AND r.commit_id = p.commit_id AND r.relationship_id = p.owner_id \
             WHERE p.commit_id = ?1 AND (\
                 (p.owner_kind = 1 AND n.node_id IS NULL) OR \
                 (p.owner_kind = 2 AND r.relationship_id IS NULL) OR \
                 p.owner_kind NOT IN (1,2)\
             ) LIMIT 1",
            [commit],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((_owner_kind, owner_id, key_id)) = missing_property_owner {
        return Err(super::StorageError::corrupt(format!(
            "PropertyKeyId {key_id} refers to missing owner {owner_id}"
        )));
    }
    Ok(())
}

fn validate_snapshot(connection: &Connection, commit: HashId) -> StorageResult<()> {
    Snapshot::resolve(connection, commit)?.validate_graph_invariants()
}
