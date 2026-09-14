use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};

use super::super::{
    CommitMetadata, HashId, LayerBuilder, StorageError, StorageResult, commit_layer,
};
use super::refs::require_commit;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub id: HashId,
    pub format_version: i64,
    pub parent1: Option<HashId>,
    pub parent2: Option<HashId>,
    pub schema_hash: HashId,
    pub metadata: CommitMetadata,
}

pub fn load_commit(connection: &Connection, commit: HashId) -> StorageResult<CommitRecord> {
    let row = connection
        .query_row(
            "SELECT format_version, parent1, parent2, schema_hash, author, message, committed_at FROM main._lithograph_commits WHERE id = ?1",
            [commit.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StorageError::not_found(format!("Commit {}", commit.to_hex())))?;
    Ok(CommitRecord {
        id: commit,
        format_version: row.0,
        parent1: row.1.as_deref().map(HashId::from_slice).transpose()?,
        parent2: row.2.as_deref().map(HashId::from_slice).transpose()?,
        schema_hash: HashId::from_slice(&row.3)?,
        metadata: CommitMetadata {
            author: row.4,
            message: row.5,
            committed_at: row.6,
        },
    })
}

pub fn commit_data(connection: &Connection, commit: HashId) -> StorageResult<Option<String>> {
    require_commit(connection, commit)?;
    connection
        .query_row(
            "SELECT data_json FROM main._lithograph_commit_data WHERE commit_id = ?1",
            [commit.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()
        .map_err(StorageError::from)
}

pub fn set_commit_data(
    connection: &Connection,
    commit: HashId,
    data_json: &str,
) -> StorageResult<()> {
    require_commit(connection, commit)?;
    let _: serde_json::Value = serde_json::from_str(data_json)
        .map_err(|_| StorageError::corrupt("Commit Data must contain valid JSON"))?;
    connection.execute(
        "INSERT INTO main._lithograph_commit_data(commit_id, data_json) VALUES(?1, ?2) ON CONFLICT(commit_id) DO UPDATE SET data_json = excluded.data_json",
        params![commit.as_bytes().as_slice(), data_json],
    )?;
    Ok(())
}

pub fn clear_commit_data(connection: &Connection, commit: HashId) -> StorageResult<()> {
    require_commit(connection, commit)?;
    connection.execute(
        "DELETE FROM main._lithograph_commit_data WHERE commit_id = ?1",
        [commit.as_bytes().as_slice()],
    )?;
    Ok(())
}

pub fn create_empty_commit(
    connection: &Connection,
    branch: &str,
    expected_head: HashId,
    metadata: &CommitMetadata,
) -> StorageResult<HashId> {
    commit_layer(
        connection,
        branch,
        expected_head,
        None,
        &LayerBuilder::default(),
        metadata,
    )
}

pub fn is_ancestor(
    connection: &Connection,
    ancestor: HashId,
    descendant: HashId,
) -> StorageResult<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    Ok(reachable_commits(connection, [descendant])?.contains(&ancestor))
}

const BEST_COMMON_ANCESTORS_SQL: &str = r#"
WITH RECURSIVE
left_anc(id) AS (
  SELECT ?1
  UNION
  SELECT c.parent1
  FROM main._lithograph_commits c JOIN left_anc a ON c.id = a.id
  WHERE c.parent1 IS NOT NULL
  UNION
  SELECT c.parent2
  FROM main._lithograph_commits c JOIN left_anc a ON c.id = a.id
  WHERE c.parent2 IS NOT NULL
),
right_anc(id) AS (
  SELECT ?2
  UNION
  SELECT c.parent1
  FROM main._lithograph_commits c JOIN right_anc a ON c.id = a.id
  WHERE c.parent1 IS NOT NULL
  UNION
  SELECT c.parent2
  FROM main._lithograph_commits c JOIN right_anc a ON c.id = a.id
  WHERE c.parent2 IS NOT NULL
),
common(id) AS (
  SELECT id FROM left_anc
  INTERSECT
  SELECT id FROM right_anc
)
SELECT candidate.id
FROM common candidate
WHERE NOT EXISTS (
  SELECT 1
  FROM main._lithograph_commits child
  JOIN common common_child ON common_child.id = child.id
  WHERE child.parent1 = candidate.id OR child.parent2 = candidate.id
)
ORDER BY candidate.id
"#;

pub fn best_common_ancestors(
    connection: &Connection,
    left: HashId,
    right: HashId,
) -> StorageResult<Vec<HashId>> {
    let mut statement = connection.prepare(BEST_COMMON_ANCESTORS_SQL)?;
    let rows = statement.query_map(
        params![left.as_bytes().as_slice(), right.as_bytes().as_slice()],
        |row| row.get::<_, Vec<u8>>(0),
    )?;
    rows.map(|row| HashId::from_slice(&row?)).collect()
}

pub fn reachable_child_count(
    connection: &Connection,
    start: HashId,
    parent: HashId,
) -> StorageResult<usize> {
    let count: i64 = connection.query_row(
        "WITH RECURSIVE reachable(id) AS (\
             SELECT ?1 \
             UNION \
             SELECT c.parent1 FROM main._lithograph_commits c JOIN reachable r ON c.id = r.id WHERE c.parent1 IS NOT NULL \
             UNION \
             SELECT c.parent2 FROM main._lithograph_commits c JOIN reachable r ON c.id = r.id WHERE c.parent2 IS NOT NULL\
         ) \
         SELECT count(*) FROM main._lithograph_commits child \
         JOIN reachable r ON r.id = child.id \
         WHERE child.parent1 = ?2 OR child.parent2 = ?2",
        params![start.as_bytes().as_slice(), parent.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    usize::try_from(count).map_err(|_| StorageError::corrupt("reachable child count exceeds usize"))
}

/// Removes an uncommitted first-parent chain used only as transaction-local
/// staging. The caller must hold the enclosing SQLite write transaction and
/// must first move public refs away from the staged chain.
pub fn discard_uncommitted_chain(
    connection: &Connection,
    base: HashId,
    head: HashId,
) -> StorageResult<()> {
    if base == head {
        return Ok(());
    }
    let commits = staged_chain(connection, base, head)?;
    delete_staged_commits(connection, &commits)?;
    delete_orphan_staged_layers(connection, &commits)?;
    delete_orphan_staged_schemas(connection, &commits)?;
    Ok(())
}

type StagedCommit = (HashId, i64, HashId);

fn staged_chain(
    connection: &Connection,
    base: HashId,
    head: HashId,
) -> StorageResult<Vec<StagedCommit>> {
    let mut current = head;
    let mut commits = Vec::new();
    while current != base {
        let row: (Option<Vec<u8>>, i64, Vec<u8>) = connection
            .query_row(
                "SELECT parent1, layer_id, schema_hash FROM main._lithograph_commits WHERE id = ?1",
                [current.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| StorageError::not_found(format!("Commit {}", current.to_hex())))?;
        let parent = row
            .0
            .as_deref()
            .map(HashId::from_slice)
            .transpose()?
            .ok_or_else(|| StorageError::corrupt("staged Commit chain reached Root before base"))?;
        commits.push((current, row.1, HashId::from_slice(&row.2)?));
        current = parent;
    }
    Ok(commits)
}

fn delete_staged_commits(connection: &Connection, commits: &[StagedCommit]) -> StorageResult<()> {
    for (commit, _, _) in commits {
        delete_commit_auxiliary(connection, *commit)?;
        let bytes = commit.as_bytes().as_slice();
        if connection.execute(
            "DELETE FROM main._lithograph_commits WHERE id = ?1",
            [bytes],
        )? != 1
        {
            return Err(StorageError::corrupt(format!(
                "failed to delete transaction-local staged Commit {}",
                commit.to_hex()
            )));
        }
    }
    Ok(())
}

pub(super) fn delete_commit_auxiliary(
    connection: &Connection,
    commit: HashId,
) -> StorageResult<(usize, usize)> {
    let bytes = commit.as_bytes().as_slice();
    let commit_data = connection.execute(
        "DELETE FROM main._lithograph_commit_data WHERE commit_id = ?1",
        [bytes],
    )?;
    let checkpoints = connection.execute(
        "DELETE FROM main._lithograph_checkpoints WHERE commit_id = ?1",
        [bytes],
    )?;
    for table in [
        "_lithograph_cp_nodes",
        "_lithograph_cp_labels",
        "_lithograph_cp_relationships",
        "_lithograph_cp_properties",
    ] {
        connection.execute(
            &format!("DELETE FROM main.{table} WHERE commit_id = ?1"),
            [bytes],
        )?;
    }
    Ok((checkpoints, commit_data))
}

fn delete_orphan_staged_layers(
    connection: &Connection,
    commits: &[StagedCommit],
) -> StorageResult<()> {
    let layer_ids = commits
        .iter()
        .map(|(_, layer_id, _)| *layer_id)
        .collect::<BTreeSet<_>>();
    for layer_id in layer_ids {
        let referenced: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_commits WHERE layer_id = ?1)",
            [layer_id],
            |row| row.get(0),
        )?;
        if referenced {
            continue;
        }
        for table in [
            "_lithograph_node_delta",
            "_lithograph_label_delta",
            "_lithograph_rel_delta",
            "_lithograph_property_delta",
        ] {
            connection.execute(
                &format!("DELETE FROM main.{table} WHERE layer_id = ?1"),
                [layer_id],
            )?;
        }
        connection.execute(
            "DELETE FROM main._lithograph_layers WHERE id = ?1",
            [layer_id],
        )?;
    }
    Ok(())
}

fn delete_orphan_staged_schemas(
    connection: &Connection,
    commits: &[StagedCommit],
) -> StorageResult<()> {
    let schema_hashes = commits
        .iter()
        .map(|(_, _, schema_hash)| *schema_hash)
        .collect::<BTreeSet<_>>();
    for schema_hash in schema_hashes {
        connection.execute(
            "DELETE FROM main._lithograph_schema_objects WHERE hash = ?1 AND NOT EXISTS(SELECT 1 FROM main._lithograph_commits WHERE schema_hash = ?1)",
            [schema_hash.as_bytes().as_slice()],
        )?;
    }
    Ok(())
}

pub fn reachable_commits(
    connection: &Connection,
    roots: impl IntoIterator<Item = HashId>,
) -> StorageResult<BTreeSet<HashId>> {
    let mut reachable = BTreeSet::new();
    let mut frontier = roots.into_iter().collect::<Vec<_>>();
    while let Some(commit) = frontier.pop() {
        if !reachable.insert(commit) {
            continue;
        }
        let record = load_commit(connection, commit)?;
        frontier.extend([record.parent1, record.parent2].into_iter().flatten());
    }
    Ok(reachable)
}

/// Returns the complete pinned reachable DAG in deterministic reverse-topological order.
/// Public pagination adds a bounded cursor over this stable order.
pub fn reverse_topological_log(
    connection: &Connection,
    start: HashId,
) -> StorageResult<Vec<CommitRecord>> {
    let reachable = reachable_commits(connection, [start])?;
    let mut records = BTreeMap::new();
    let mut child_count = BTreeMap::<HashId, usize>::new();
    for commit in &reachable {
        let record = load_commit(connection, *commit)?;
        child_count.entry(*commit).or_default();
        for parent in [record.parent1, record.parent2].into_iter().flatten() {
            if reachable.contains(&parent) {
                *child_count.entry(parent).or_default() += 1;
            }
        }
        records.insert(*commit, record);
    }
    let mut ready = BTreeSet::<(std::cmp::Reverse<i64>, HashId)>::new();
    for (commit, count) in &child_count {
        if *count == 0 {
            let record = &records[commit];
            ready.insert((std::cmp::Reverse(record.metadata.committed_at), *commit));
        }
    }
    let mut output = Vec::with_capacity(records.len());
    while let Some(key) = ready.pop_first() {
        let commit = key.1;
        let record = records[&commit].clone();
        for parent in [record.parent1, record.parent2].into_iter().flatten() {
            if !reachable.contains(&parent) {
                continue;
            }
            let count = child_count
                .get_mut(&parent)
                .ok_or_else(|| StorageError::corrupt("history traversal lost a parent"))?;
            *count = count.saturating_sub(1);
            if *count == 0 {
                let parent_record = &records[&parent];
                ready.insert((
                    std::cmp::Reverse(parent_record.metadata.committed_at),
                    parent,
                ));
            }
        }
        output.push(record);
    }
    if output.len() != records.len() {
        return Err(StorageError::corrupt("Commit DAG contains a cycle"));
    }
    Ok(output)
}
