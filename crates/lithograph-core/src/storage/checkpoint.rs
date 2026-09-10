//! Rebuildable snapshot checkpoints.

use rusqlite::{Connection, params};

use super::property::PropertyColumns;
use super::snapshot::Snapshot;
use super::{HashId, StorageResult};

/// Materializes a derived checkpoint for any resolvable Commit.
pub fn create_checkpoint(connection: &Connection, commit: HashId) -> StorageResult<()> {
    if checkpoint_exists(connection, commit)? {
        return Ok(());
    }
    let snapshot = Snapshot::resolve(connection, commit)?;
    with_savepoint(connection, || {
        let created_at: i64 = connection.query_row(
            "SELECT CAST(unixepoch('subsec') * 1000000 AS INTEGER)",
            [],
            |row| row.get(0),
        )?;
        connection.execute(
            "INSERT INTO main._lithograph_checkpoints(commit_id, created_at, metadata) VALUES(?1, ?2, NULL)",
            params![commit.as_bytes().as_slice(), created_at],
        )?;
        write_nodes(connection, commit, &snapshot)?;
        write_labels(connection, commit, &snapshot)?;
        write_relationships(connection, commit, &snapshot)?;
        write_properties(connection, commit, &snapshot)
    })
}

/// Deletes only derived checkpoint rows; canonical history is untouched.
pub fn delete_checkpoint(connection: &Connection, commit: HashId) -> StorageResult<()> {
    with_savepoint(connection, || {
        for table in [
            "_lithograph_cp_properties",
            "_lithograph_cp_relationships",
            "_lithograph_cp_labels",
            "_lithograph_cp_nodes",
            "_lithograph_checkpoints",
        ] {
            let sql = format!("DELETE FROM main.{table} WHERE commit_id = ?1");
            connection.execute(&sql, [commit.as_bytes().as_slice()])?;
        }
        Ok(())
    })
}

fn write_nodes(
    connection: &Connection,
    commit: HashId,
    snapshot: &Snapshot<'_>,
) -> StorageResult<()> {
    let mut statement = connection
        .prepare("INSERT INTO main._lithograph_cp_nodes(commit_id, node_id) VALUES(?1, ?2)")?;
    snapshot.visit_nodes(|node_id| {
        statement.execute(params![commit.as_bytes().as_slice(), node_id])?;
        Ok(())
    })
}

fn write_labels(
    connection: &Connection,
    commit: HashId,
    snapshot: &Snapshot<'_>,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_cp_labels(commit_id, node_id, label_id) VALUES(?1, ?2, ?3)",
    )?;
    snapshot.visit_labels(|node_id, label_id| {
        statement.execute(params![commit.as_bytes().as_slice(), node_id, label_id])?;
        Ok(())
    })
}

fn write_relationships(
    connection: &Connection,
    commit: HashId,
    snapshot: &Snapshot<'_>,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_cp_relationships(commit_id, relationship_id, source_id, type_id, target_id) VALUES(?1, ?2, ?3, ?4, ?5)",
    )?;
    snapshot.visit_relationships(|relationship| {
        statement.execute(params![
            commit.as_bytes().as_slice(),
            relationship.id,
            relationship.source,
            relationship.type_id,
            relationship.target,
        ])?;
        Ok(())
    })
}

fn write_properties(
    connection: &Connection,
    commit: HashId,
    snapshot: &Snapshot<'_>,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_cp_properties(commit_id, owner_kind, owner_id, key_id, type_tag, int_value, real_value, text_value, blob_value, aux_value) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    snapshot.visit_properties(|owner_kind, owner_id, key_id, value| {
        let columns = PropertyColumns::from_value(&value)?;
        statement.execute(params![
            commit.as_bytes().as_slice(),
            owner_kind as i64,
            owner_id,
            key_id,
            columns.type_tag,
            columns.int_value,
            columns.real_value,
            columns.text_value.as_deref(),
            columns.blob_value.as_deref(),
            columns.aux_value.as_deref(),
        ])?;
        Ok(())
    })
}

fn checkpoint_exists(connection: &Connection, commit: HashId) -> StorageResult<bool> {
    let exists: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main._lithograph_checkpoints WHERE commit_id = ?1)",
        [commit.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    Ok(exists == 1)
}

fn with_savepoint<T>(
    connection: &Connection,
    operation: impl FnOnce() -> StorageResult<T>,
) -> StorageResult<T> {
    connection.execute_batch("SAVEPOINT lithograph_checkpoint_write")?;
    match operation() {
        Ok(value) => {
            connection.execute_batch("RELEASE lithograph_checkpoint_write")?;
            Ok(value)
        }
        Err(error) => {
            connection.execute_batch(
                "ROLLBACK TO lithograph_checkpoint_write; RELEASE lithograph_checkpoint_write",
            )?;
            Err(error)
        }
    }
}
