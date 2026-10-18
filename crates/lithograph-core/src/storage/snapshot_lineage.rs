//! First-parent lineage resolution for Snapshot construction.

use std::collections::BTreeSet;
use std::mem::size_of;

use rusqlite::{Connection, Statement, types::ValueRef};

use super::snapshot_read::{self, ReadLease, SnapshotReadObserver};
use super::{HashId, LayerBuilder, StorageError, StorageResult};

type Check<'a> = &'a dyn Fn() -> StorageResult<()>;

pub(super) fn resolve_lineage(
    connection: &Connection,
    commit: HashId,
    skip_checkpoint: Option<HashId>,
    observer: Option<&dyn SnapshotReadObserver>,
    check: Check<'_>,
) -> StorageResult<(Option<HashId>, Vec<i64>, Vec<ReadLease>)> {
    check()?;
    let mut current = commit;
    let mut layers = Vec::new();
    let mut visited = BTreeSet::new();
    let mut memory = Vec::new();
    let empty_layer_hash = LayerBuilder::default().content_hash()?;
    let mut statement = connection.prepare(
        "SELECT commits.parent1, commits.layer_id, layers.hash, \
                EXISTS(SELECT 1 FROM main._lithograph_checkpoints AS checkpoints \
                       WHERE checkpoints.commit_id = commits.id), \
                (EXISTS(SELECT 1 FROM main._lithograph_node_delta AS node_delta \
                        WHERE node_delta.layer_id = commits.layer_id) \
                 OR EXISTS(SELECT 1 FROM main._lithograph_label_delta AS label_delta \
                           WHERE label_delta.layer_id = commits.layer_id) \
                 OR EXISTS(SELECT 1 FROM main._lithograph_rel_delta AS rel_delta \
                           WHERE rel_delta.layer_id = commits.layer_id) \
                 OR EXISTS(SELECT 1 FROM main._lithograph_property_delta AS property_delta \
                           WHERE property_delta.layer_id = commits.layer_id)) \
         FROM main._lithograph_commits AS commits \
         JOIN main._lithograph_layers AS layers ON layers.id = commits.layer_id \
         WHERE commits.id = ?1",
    )?;
    check()?;
    loop {
        check()?;
        if visited.contains(&current) {
            return Err(StorageError::corrupt(
                "Commit first-parent lineage contains a cycle",
            ));
        }
        reserve_lineage_entry(&mut memory, observer, check)?;
        visited.insert(current);
        check()?;
        let row = load_lineage_row(&mut statement, current)?;
        check()?;
        if Some(current) != skip_checkpoint && row.has_checkpoint {
            return Ok((Some(current), layers, memory));
        }
        record_lineage_layer(&row, empty_layer_hash, &mut layers, check)?;
        let Some(parent) = row.parent else {
            return Ok((None, layers, memory));
        };
        current = parent;
    }
}

fn reserve_lineage_entry(
    memory: &mut Vec<ReadLease>,
    observer: Option<&dyn SnapshotReadObserver>,
    check: Check<'_>,
) -> StorageResult<()> {
    // Each visited key can allocate a whole B-tree node. The two Vecs grow
    // geometrically below; eight slots per visited Commit cover initial spare
    // capacity and simultaneous old/new storage during their reallocation.
    let tree = size_of::<HashId>() as u64 * 16 + 512;
    let arrays = (size_of::<i64>() + size_of::<ReadLease>()) as u64 * 8;
    let lease = snapshot_read::reserve(observer, snapshot_read::checked_add(tree, arrays)?)?;
    if let Some(lease) = lease {
        push_lineage_item(memory, lease, check)?;
    }
    Ok(())
}

fn push_lineage_item<T>(values: &mut Vec<T>, value: T, check: Check<'_>) -> StorageResult<()> {
    if values.len() == values.capacity() {
        let capacity = values
            .capacity()
            .checked_mul(2)
            .ok_or_else(snapshot_read::resource_overflow)?
            .max(4);
        check()?;
        values.reserve_exact(capacity - values.len());
        check()?;
    }
    values.push(value);
    Ok(())
}

struct LineageRow {
    parent: Option<HashId>,
    layer_id: i64,
    layer_hash: HashId,
    has_checkpoint: bool,
    has_delta: bool,
}

fn load_lineage_row(statement: &mut Statement<'_>, current: HashId) -> StorageResult<LineageRow> {
    let mut rows = statement.query([current.as_bytes().as_slice()])?;
    let row = rows
        .next()?
        .ok_or_else(|| StorageError::not_found(format!("Commit {}", current.to_hex())))?;
    let parent = match row.get_ref(0)? {
        ValueRef::Null => None,
        value => Some(hash_column(value)?),
    };
    Ok(LineageRow {
        parent,
        layer_id: row.get(1)?,
        layer_hash: hash_column(row.get_ref(2)?)?,
        has_checkpoint: row.get::<_, i64>(3)? == 1,
        has_delta: row.get::<_, i64>(4)? == 1,
    })
}

fn hash_column(value: ValueRef<'_>) -> StorageResult<HashId> {
    let bytes = value.as_blob().map_err(rusqlite::Error::from)?;
    HashId::from_slice(bytes)
}

fn record_lineage_layer(
    row: &LineageRow,
    empty_layer_hash: HashId,
    layers: &mut Vec<i64>,
    check: Check<'_>,
) -> StorageResult<()> {
    if row.has_delta {
        push_lineage_item(layers, row.layer_id, check)?;
        return Ok(());
    }
    if row.layer_hash != empty_layer_hash {
        return Err(StorageError::corrupt(format!(
            "empty Layer {} has a non-canonical content hash",
            row.layer_id
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fmt::Debug;
    use std::rc::Rc;

    use super::*;
    use crate::storage::{
        CommitMetadata, allocate_node_id, branch_head, commit_layer, create_checkpoint,
        create_storage_schema, initialize_root,
    };

    #[derive(Debug)]
    struct BudgetState {
        used: Cell<u64>,
        limit: u64,
    }

    #[derive(Debug)]
    struct Budget(Rc<BudgetState>);

    #[derive(Debug)]
    struct Lease(Rc<BudgetState>, u64);

    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.used.set(self.0.used.get() - self.1);
        }
    }

    impl SnapshotReadObserver for Budget {
        fn reserve(&self, bytes: u64) -> StorageResult<Box<dyn Debug>> {
            let used = snapshot_read::checked_add(self.0.used.get(), bytes)?;
            if used > self.0.limit {
                return Err(snapshot_read::resource_overflow());
            }
            self.0.used.set(used);
            Ok(Box::new(Lease(self.0.clone(), bytes)))
        }
    }

    fn budget(limit: u64) -> (Rc<BudgetState>, Budget) {
        let state = Rc::new(BudgetState {
            used: Cell::new(0),
            limit,
        });
        (state.clone(), Budget(state))
    }

    fn history(count: usize, graph_writes: bool) -> (Connection, HashId) {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").unwrap();
        create_storage_schema(&connection).unwrap();
        let mut head = initialize_root(&connection).unwrap().root;
        for index in 0..count {
            let mut layer = LayerBuilder::default();
            if graph_writes {
                layer
                    .add_node(allocate_node_id(&connection).unwrap())
                    .unwrap();
            }
            head = commit_layer(
                &connection,
                "main",
                head,
                None,
                &layer,
                &CommitMetadata {
                    author: None,
                    message: None,
                    committed_at: index as i64 + 1,
                },
            )
            .unwrap();
        }
        (connection, head)
    }

    #[test]
    fn empty_commit_history_is_budgeted_and_failure_releases_lineage_memory() {
        let (connection, head) = history(256, false);
        let (state, observer) = budget(4096);
        let error =
            resolve_lineage(&connection, head, None, Some(&observer), &|| Ok(())).unwrap_err();
        assert!(
            matches!(error, StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) if code.extended_code == rusqlite::ffi::SQLITE_TOOBIG)
        );
        assert_eq!(state.used.get(), 0);
        assert_eq!(branch_head(&connection, "main").unwrap(), head);
        let (checkpoint, layers, memory) =
            resolve_lineage(&connection, head, None, None, &|| Ok(())).unwrap();
        assert!(checkpoint.is_none());
        assert!(layers.is_empty());
        assert!(memory.is_empty());
    }

    #[test]
    fn empty_commit_history_checks_cancellation_and_releases_lineage_memory() {
        let (connection, head) = history(256, false);
        let (state, observer) = budget(u64::MAX);
        let calls = Cell::new(0);
        let error = resolve_lineage(&connection, head, None, Some(&observer), &|| {
            calls.set(calls.get() + 1);
            if calls.get() == 257 {
                Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                    None,
                )))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(
            matches!(error, StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) if code.extended_code == rusqlite::ffi::SQLITE_INTERRUPT)
        );
        assert_eq!(calls.get(), 257);
        assert_eq!(state.used.get(), 0);
        assert_eq!(branch_head(&connection, "main").unwrap(), head);
    }

    #[test]
    fn lineage_layer_capacity_stays_charged_until_the_returned_leases_drop() {
        let (connection, head) = history(40, true);
        let (state, observer) = budget(u64::MAX);
        let (checkpoint, layers, memory) =
            resolve_lineage(&connection, head, None, Some(&observer), &|| Ok(())).unwrap();
        assert!(checkpoint.is_none());
        assert_eq!(layers.len(), 40);
        assert_eq!(memory.len(), 41);
        let arrays =
            layers.capacity() * size_of::<i64>() + memory.capacity() * size_of::<ReadLease>();
        assert!(state.used.get() >= arrays as u64);
        drop(layers);
        assert!(state.used.get() > 0);
        drop(memory);
        assert_eq!(state.used.get(), 0);
    }

    #[test]
    fn skip_checkpoint_keeps_the_first_parent_fallback_semantics() {
        let (connection, head) = history(2, true);
        create_checkpoint(&connection, head).unwrap();
        let (state, observer) = budget(u64::MAX);
        let (checkpoint, layers, memory) =
            resolve_lineage(&connection, head, None, Some(&observer), &|| Ok(())).unwrap();
        assert_eq!(checkpoint, Some(head));
        assert!(layers.is_empty());
        drop(memory);
        assert_eq!(state.used.get(), 0);
        let (checkpoint, layers, memory) =
            resolve_lineage(&connection, head, Some(head), Some(&observer), &|| Ok(())).unwrap();
        assert!(checkpoint.is_none());
        assert_eq!(layers.len(), 2);
        drop(memory);
        assert_eq!(state.used.get(), 0);
    }
}
