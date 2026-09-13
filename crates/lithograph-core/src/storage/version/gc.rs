use std::collections::BTreeSet;

use rusqlite::{Connection, params};

use super::super::{HashId, StorageResult};
use super::{
    history::delete_commit_auxiliary, list_branches, list_tags, merge_session_roots,
    reachable_commits,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcCounters {
    pub commits: usize,
    pub layers: usize,
    pub schemas: usize,
    pub checkpoints: usize,
    pub commit_data: usize,
}

pub fn collect_garbage(connection: &Connection) -> StorageResult<GcCounters> {
    let unreachable = unreachable_commits(connection)?;
    if unreachable.is_empty() {
        return Ok(GcCounters::default());
    }
    let mut counters = GcCounters::default();
    delete_unreachable_commits(connection, &unreachable, &mut counters)?;
    delete_orphan_layers(connection, &mut counters)?;
    counters.schemas += connection.execute(
        "DELETE FROM main._lithograph_schema_objects WHERE NOT EXISTS(SELECT 1 FROM main._lithograph_commits WHERE schema_hash = _lithograph_schema_objects.hash)",
        [],
    )?;
    Ok(counters)
}

fn unreachable_commits(connection: &Connection) -> StorageResult<Vec<HashId>> {
    let mut roots = Vec::new();
    roots.extend(
        list_branches(connection)?
            .into_iter()
            .map(|item| item.commit),
    );
    roots.extend(list_tags(connection)?.into_iter().map(|item| item.commit));
    roots.extend(merge_session_roots(connection)?);
    let reachable = reachable_commits(connection, roots)?;
    let all = all_commits(connection)?;
    Ok(all.difference(&reachable).copied().collect())
}

fn delete_unreachable_commits(
    connection: &Connection,
    unreachable: &[HashId],
    counters: &mut GcCounters,
) -> StorageResult<()> {
    for commit in unreachable {
        let bytes = commit.as_bytes().as_slice();
        let (checkpoints, commit_data) = delete_commit_auxiliary(connection, *commit)?;
        counters.checkpoints += checkpoints;
        counters.commit_data += commit_data;
        counters.commits += connection.execute(
            "DELETE FROM main._lithograph_commits WHERE id = ?1",
            [bytes],
        )?;
    }
    Ok(())
}

fn delete_orphan_layers(connection: &Connection, counters: &mut GcCounters) -> StorageResult<()> {
    let orphan_layers = orphan_layer_ids(connection)?;
    for layer_id in orphan_layers {
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
        counters.layers += connection.execute(
            "DELETE FROM main._lithograph_layers WHERE id = ?1 AND NOT EXISTS(SELECT 1 FROM main._lithograph_commits WHERE layer_id = ?1)",
            [layer_id],
        )?;
    }
    Ok(())
}

fn all_commits(connection: &Connection) -> StorageResult<BTreeSet<HashId>> {
    let mut statement = connection.prepare("SELECT id FROM main._lithograph_commits")?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut commits = BTreeSet::new();
    for row in rows {
        commits.insert(HashId::from_slice(&row?)?);
    }
    Ok(commits)
}

fn orphan_layer_ids(connection: &Connection) -> StorageResult<Vec<i64>> {
    let mut statement = connection.prepare(
        "SELECT id FROM main._lithograph_layers WHERE NOT EXISTS(SELECT 1 FROM main._lithograph_commits WHERE layer_id = _lithograph_layers.id) ORDER BY id",
    )?;
    let rows = statement.query_map(params![], |row| row.get(0))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}
