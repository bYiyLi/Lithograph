//! Commit and branch-reference persistence.

use rusqlite::{Connection, OptionalExtension, params};

use super::layer::{LayerBuilder, persist_layer_observed};
use super::schema::commit_hash;
use super::snapshot::Snapshot;
use super::snapshot_read::SnapshotReadObserver;
use super::{CommitMetadata, HashId, StorageError, StorageResult};
use std::rc::Rc;

/// Returns the current Commit referenced by a branch.
pub fn branch_head(connection: &Connection, branch: &str) -> StorageResult<HashId> {
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT commit_id FROM main._lithograph_branches WHERE name = ?1",
            [branch],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StorageError::not_found(format!("branch {branch:?}")))?;
    HashId::from_slice(&bytes)
}

/// Returns whether an immutable Commit identity exists without resolving its graph state.
pub fn commit_exists(connection: &Connection, commit: HashId) -> StorageResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_commits WHERE id = ?1)",
            [commit.as_bytes().as_slice()],
            |row| row.get::<_, bool>(0),
        )
        .map_err(StorageError::from)
}

/// Creates a mutable branch reference at an existing immutable Commit.
pub fn create_branch(connection: &Connection, name: &str, from: HashId) -> StorageResult<()> {
    if name.is_empty() {
        return Err(StorageError::corrupt("branch name must not be empty"));
    }
    require_commit(connection, from)?;
    connection.execute(
        "INSERT INTO main._lithograph_branches(name, commit_id) VALUES(?1, ?2)",
        params![name, from.as_bytes().as_slice()],
    )?;
    Ok(())
}

/// Persists one canonical Layer and Commit, then compare-and-moves a branch.
///
/// A merge-shaped Commit supplies `second_parent`; its Layer remains relative
/// to `expected_head`, the first parent.
pub fn commit_layer(
    connection: &Connection,
    branch: &str,
    expected_head: HashId,
    second_parent: Option<HashId>,
    layer: &LayerBuilder,
    metadata: &CommitMetadata,
) -> StorageResult<HashId> {
    commit_layer_checked(
        connection,
        branch,
        expected_head,
        second_parent,
        layer,
        metadata,
        &|| Ok(()),
    )
}

pub(crate) fn commit_layer_checked(
    connection: &Connection,
    branch: &str,
    expected_head: HashId,
    second_parent: Option<HashId>,
    layer: &LayerBuilder,
    metadata: &CommitMetadata,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<HashId> {
    commit_layer_observed(
        connection,
        branch,
        expected_head,
        second_parent,
        layer,
        metadata,
        CommitRead {
            observer: None,
            check,
        },
    )
}

pub(crate) fn commit_layer_observed(
    connection: &Connection,
    branch: &str,
    expected_head: HashId,
    second_parent: Option<HashId>,
    layer: &LayerBuilder,
    metadata: &CommitMetadata,
    read: CommitRead<'_>,
) -> StorageResult<HashId> {
    with_savepoint(connection, |connection| {
        (read.check)()?;
        let schema_hash = super::schema::schema_hash_for_commit(connection, expected_head)?;
        commit_change_inner(
            connection,
            branch,
            expected_head,
            second_parent,
            (layer, schema_hash),
            metadata,
            &read,
        )
    })
}

/// Persists a Schema-only Commit with an empty graph Layer and compare-and-moves a Branch.
/// Persists a canonical graph Layer and explicit Schema object, optionally with a second parent.
pub fn commit_layer_with_schema(
    connection: &Connection,
    branch: &str,
    expected_head: HashId,
    second_parent: Option<HashId>,
    layer: &LayerBuilder,
    schema_hash: HashId,
    metadata: &CommitMetadata,
) -> StorageResult<HashId> {
    commit_layer_with_schema_checked(
        connection,
        branch,
        expected_head,
        second_parent,
        (layer, schema_hash),
        metadata,
        &|| Ok(()),
    )
}

pub(crate) fn commit_layer_with_schema_checked(
    connection: &Connection,
    branch: &str,
    expected_head: HashId,
    second_parent: Option<HashId>,
    input: (&LayerBuilder, HashId),
    metadata: &CommitMetadata,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<HashId> {
    commit_layer_with_schema_observed(
        connection,
        branch,
        expected_head,
        second_parent,
        input,
        metadata,
        CommitRead {
            observer: None,
            check,
        },
    )
}

pub(crate) struct CommitRead<'a> {
    observer: Option<Rc<dyn SnapshotReadObserver>>,
    check: &'a dyn Fn() -> StorageResult<()>,
}

impl<'a> CommitRead<'a> {
    pub(crate) fn observed(
        observer: Rc<dyn SnapshotReadObserver>,
        check: &'a dyn Fn() -> StorageResult<()>,
    ) -> Self {
        Self {
            observer: Some(observer),
            check,
        }
    }
}

pub(crate) fn commit_layer_with_schema_observed(
    connection: &Connection,
    branch: &str,
    expected_head: HashId,
    second_parent: Option<HashId>,
    input: (&LayerBuilder, HashId),
    metadata: &CommitMetadata,
    read: CommitRead<'_>,
) -> StorageResult<HashId> {
    let (layer, schema_hash) = input;
    with_savepoint(connection, |connection| {
        (read.check)()?;
        require_schema(connection, schema_hash)?;
        commit_change_inner(
            connection,
            branch,
            expected_head,
            second_parent,
            (layer, schema_hash),
            metadata,
            &read,
        )
    })
}

pub fn commit_schema(
    connection: &Connection,
    branch: &str,
    expected_head: HashId,
    schema_hash: HashId,
    metadata: &CommitMetadata,
) -> StorageResult<HashId> {
    commit_layer_with_schema(
        connection,
        branch,
        expected_head,
        None,
        &LayerBuilder::default(),
        schema_hash,
        metadata,
    )
}

fn commit_change_inner(
    connection: &Connection,
    branch: &str,
    expected_head: HashId,
    second_parent: Option<HashId>,
    input: (&LayerBuilder, HashId),
    metadata: &CommitMetadata,
    read: &CommitRead<'_>,
) -> StorageResult<HashId> {
    let (layer, schema_hash) = input;
    (read.check)()?;
    if branch_head(connection, branch)? != expected_head {
        return Err(StorageError::BranchHeadMoved);
    }
    if let Some(parent) = second_parent {
        require_commit(connection, parent)?;
    }
    require_schema(connection, schema_hash)?;
    validate_candidate_layer(connection, expected_head, layer, read)?;
    let (layer_id, layer_hash) =
        persist_layer_observed(connection, layer, read.observer.as_deref(), read.check)?;
    let commit = commit_hash(
        super::STORAGE_FORMAT,
        Some(expected_head),
        second_parent,
        layer_hash,
        schema_hash,
        metadata,
    );
    let row = CommitRow {
        id: commit,
        parent1: expected_head,
        parent2: second_parent,
        layer_id,
        schema_hash,
        metadata,
    };
    (read.check)()?;
    insert_commit(connection, &row)?;
    (read.check)()?;
    compare_and_move_branch(connection, branch, expected_head, commit)?;
    (read.check)()?;
    Ok(commit)
}

fn validate_candidate_layer(
    connection: &Connection,
    expected_head: HashId,
    layer: &LayerBuilder,
    read: &CommitRead<'_>,
) -> StorageResult<()> {
    if !layer.is_empty() {
        let state = Snapshot::resolve_state_with_checkpoint_skip(
            connection,
            expected_head,
            None,
            read.observer.clone(),
            read.check,
        )?;
        Snapshot::from_resolved_state(connection, &state)
            .validate_layer_invariants_checked(layer, read.check)?;
    }
    Ok(())
}

struct CommitRow<'a> {
    id: HashId,
    parent1: HashId,
    parent2: Option<HashId>,
    layer_id: i64,
    schema_hash: HashId,
    metadata: &'a CommitMetadata,
}

fn insert_commit(connection: &Connection, row: &CommitRow<'_>) -> StorageResult<()> {
    let parent2 = row
        .parent2
        .as_ref()
        .map(|commit| commit.as_bytes().as_slice());
    connection.execute(
        "INSERT OR IGNORE INTO main._lithograph_commits(id, format_version, parent1, parent2, layer_id, schema_hash, author, message, committed_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            row.id.as_bytes().as_slice(),
            super::STORAGE_FORMAT,
            row.parent1.as_bytes().as_slice(),
            parent2,
            row.layer_id,
            row.schema_hash.as_bytes().as_slice(),
            row.metadata.author.as_deref(),
            row.metadata.message.as_deref(),
            row.metadata.committed_at,
        ],
    )?;
    verify_commit_row(connection, row)
}

fn verify_commit_row(connection: &Connection, expected: &CommitRow<'_>) -> StorageResult<()> {
    let actual = read_persisted_commit_row(connection, expected.id)?;
    let expected_row = persisted_commit_row(expected);
    if actual == expected_row {
        Ok(())
    } else {
        Err(StorageError::corrupt(format!(
            "Commit {} does not match its content-addressed fields",
            expected.id.to_hex()
        )))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PersistedCommitRow {
    format_version: i64,
    parent1: Vec<u8>,
    parent2: Option<Vec<u8>>,
    layer_id: i64,
    schema_hash: Vec<u8>,
    author: Option<String>,
    message: Option<String>,
    committed_at: i64,
}

fn read_persisted_commit_row(
    connection: &Connection,
    id: HashId,
) -> StorageResult<PersistedCommitRow> {
    connection
        .query_row(
        "SELECT format_version, parent1, parent2, layer_id, schema_hash, author, message, committed_at FROM main._lithograph_commits WHERE id = ?1",
        [id.as_bytes().as_slice()],
        |row| {
            Ok(PersistedCommitRow {
                format_version: row.get(0)?,
                parent1: row.get(1)?,
                parent2: row.get(2)?,
                layer_id: row.get(3)?,
                schema_hash: row.get(4)?,
                author: row.get(5)?,
                message: row.get(6)?,
                committed_at: row.get(7)?,
            })
        },
    )
        .map_err(StorageError::from)
}

fn persisted_commit_row(row: &CommitRow<'_>) -> PersistedCommitRow {
    PersistedCommitRow {
        format_version: super::STORAGE_FORMAT,
        parent1: row.parent1.as_bytes().to_vec(),
        parent2: row.parent2.map(|value| value.as_bytes().to_vec()),
        layer_id: row.layer_id,
        schema_hash: row.schema_hash.as_bytes().to_vec(),
        author: row.metadata.author.clone(),
        message: row.metadata.message.clone(),
        committed_at: row.metadata.committed_at,
    }
}

fn require_commit(connection: &Connection, commit: HashId) -> StorageResult<()> {
    let exists: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main._lithograph_commits WHERE id = ?1)",
        [commit.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    if exists == 1 {
        Ok(())
    } else {
        Err(StorageError::not_found(format!(
            "Commit {}",
            commit.to_hex()
        )))
    }
}

fn require_schema(connection: &Connection, schema_hash: HashId) -> StorageResult<()> {
    let exists: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main._lithograph_schema_objects WHERE hash = ?1)",
        [schema_hash.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    if exists == 1 {
        Ok(())
    } else {
        Err(StorageError::not_found(format!(
            "Schema {}",
            schema_hash.to_hex()
        )))
    }
}

fn compare_and_move_branch(
    connection: &Connection,
    branch: &str,
    expected_head: HashId,
    commit: HashId,
) -> StorageResult<()> {
    let changed = connection.execute(
        "UPDATE main._lithograph_branches SET commit_id = ?3 WHERE name = ?1 AND commit_id = ?2",
        params![
            branch,
            expected_head.as_bytes().as_slice(),
            commit.as_bytes().as_slice()
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StorageError::BranchHeadMoved)
    }
}

fn with_savepoint<T>(
    connection: &Connection,
    operation: impl FnOnce(&Connection) -> StorageResult<T>,
) -> StorageResult<T> {
    connection.execute_batch("SAVEPOINT lithograph_storage_write")?;
    match operation(connection) {
        Ok(value) => {
            connection.execute_batch("RELEASE lithograph_storage_write")?;
            Ok(value)
        }
        Err(error) => {
            rollback_savepoint(connection)?;
            Err(error)
        }
    }
}

fn rollback_savepoint(connection: &Connection) -> StorageResult<()> {
    connection
        .execute_batch("ROLLBACK TO lithograph_storage_write; RELEASE lithograph_storage_write")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        OwnerKind, PropertyValue, allocate_node_id, create_storage_schema, initialize_root,
        intern_property_key,
    };

    fn fixture() -> (Connection, HashId) {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").unwrap();
        create_storage_schema(&connection).unwrap();
        let root = initialize_root(&connection).unwrap().root;
        (connection, root)
    }

    fn property_layer(connection: &Connection) -> LayerBuilder {
        let node = allocate_node_id(connection).unwrap();
        let key = intern_property_key(connection, "value").unwrap();
        let mut layer = LayerBuilder::default();
        layer.add_node(node).unwrap();
        layer
            .set_property(
                OwnerKind::Node,
                node,
                key,
                PropertyValue::String("人物".repeat(25_000)),
            )
            .unwrap();
        layer
    }

    fn metadata() -> CommitMetadata {
        CommitMetadata {
            author: None,
            message: None,
            committed_at: 123_456,
        }
    }

    fn counts(connection: &Connection) -> (i64, i64, i64, i64) {
        connection.query_row("SELECT (SELECT count(*) FROM _lithograph_commits), (SELECT count(*) FROM _lithograph_layers), (SELECT count(*) FROM _lithograph_node_delta), (SELECT next_id FROM _lithograph_sequences WHERE kind=6)", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).unwrap()
    }

    #[test]
    fn checked_commit_interrupt_after_first_persisted_slot_rolls_back_atomically() {
        let (connection, root) = fixture();
        let layer = property_layer(&connection);
        let baseline = counts(&connection);
        let error = commit_layer_checked(
            &connection,
            "main",
            root,
            None,
            &layer,
            &metadata(),
            &|| {
                let nodes: i64 = connection.query_row(
                    "SELECT count(*) FROM _lithograph_node_delta",
                    [],
                    |row| row.get(0),
                )?;
                if nodes != 0 {
                    Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                        None,
                    )))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert!(
            matches!(error, StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) if code.extended_code == rusqlite::ffi::SQLITE_INTERRUPT)
        );
        assert_eq!(branch_head(&connection, "main").unwrap(), root);
        assert_eq!(counts(&connection), baseline);
        assert!(connection.is_autocommit());
    }

    #[derive(Debug)]
    struct SmallReadBudget;
    impl SnapshotReadObserver for SmallReadBudget {
        fn reserve(&self, bytes: u64) -> StorageResult<Box<dyn std::fmt::Debug>> {
            if bytes > 64 * 1024 {
                return Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_TOOBIG),
                    None,
                )));
            }
            Ok(Box::new(()))
        }
    }

    #[test]
    fn observed_commit_preflights_parent_values_before_validation_decode() {
        let (connection, root) = fixture();
        let first = commit_layer(
            &connection,
            "main",
            root,
            None,
            &property_layer(&connection),
            &metadata(),
        )
        .unwrap();
        let mut candidate = LayerBuilder::default();
        candidate
            .add_node(allocate_node_id(&connection).unwrap())
            .unwrap();
        let baseline = counts(&connection);
        let error = commit_layer_observed(
            &connection,
            "main",
            first,
            None,
            &candidate,
            &metadata(),
            CommitRead::observed(Rc::new(SmallReadBudget), &|| Ok(())),
        )
        .unwrap_err();
        assert!(
            matches!(error, StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) if code.extended_code == rusqlite::ffi::SQLITE_TOOBIG)
        );
        assert_eq!(branch_head(&connection, "main").unwrap(), first);
        assert_eq!(counts(&connection), baseline);
    }
}
