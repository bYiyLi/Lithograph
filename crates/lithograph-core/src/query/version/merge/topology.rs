//! Query-owned Commit topology traversal. Parent hashes are borrowed directly
//! from SQLite; traversal never loads graph state or owned Commit metadata.

use std::collections::BTreeSet;
use std::mem::size_of;

use rusqlite::types::ValueRef;

use crate::query::resources::{MemoryReservation, allocation_bytes, checked_add, checked_mul};
use crate::query::version::{VersionExecution, VersionOwned};
use crate::query::{QueryError, QueryErrorKind, QueryResult};
use crate::storage::{self, HashId};

fn set_slot_bytes() -> QueryResult<u64> {
    // Includes a singleton leaf and internal-child pointers. A separate root
    // allowance remains while a BTreeSet is empty after pop/remove operations.
    checked_add(checked_mul(16, size_of::<HashId>() as u64)?, 256)
}

struct CommitSet {
    values: BTreeSet<HashId>,
    memory: MemoryReservation,
}

impl CommitSet {
    fn new(connection: &VersionExecution<'_, '_>) -> QueryResult<Self> {
        let memory = connection.reserve(set_slot_bytes()?)?;
        Ok(Self {
            values: BTreeSet::new(),
            memory,
        })
    }

    fn insert(
        &mut self,
        connection: &VersionExecution<'_, '_>,
        commit: HashId,
    ) -> QueryResult<bool> {
        connection.checkpoint()?;
        if self.values.contains(&commit) {
            return Ok(false);
        }
        self.memory.grow(set_slot_bytes()?)?;
        self.values.insert(commit);
        Ok(true)
    }

    fn pop_first(&mut self) -> QueryResult<Option<HashId>> {
        let result = self.values.pop_first();
        if result.is_some() {
            self.memory
                .resize(self.memory.bytes() - set_slot_bytes()?)?;
        }
        Ok(result)
    }
}

fn parents(
    connection: &VersionExecution<'_, '_>,
    commit: HashId,
) -> QueryResult<[Option<HashId>; 2]> {
    connection.control().check()?;
    let mut statement =
        connection.prepare("SELECT parent1,parent2 FROM main._lithograph_commits WHERE id = ?1")?;
    let mut rows = statement.query([commit.as_bytes().as_slice()])?;
    let row = rows
        .next()?
        .ok_or_else(|| storage::StorageError::not_found(format!("Commit {}", commit.to_hex())))?;
    Ok([parent(row.get_ref(0)?)?, parent(row.get_ref(1)?)?])
}

fn parent(value: ValueRef<'_>) -> QueryResult<Option<HashId>> {
    match value {
        ValueRef::Null => Ok(None),
        ValueRef::Blob(bytes) => HashId::from_slice(bytes).map(Some).map_err(Into::into),
        _ => Err(QueryError::new(
            QueryErrorKind::Storage,
            "Commit parent is not a BLOB",
        )),
    }
}

fn ancestors(connection: &VersionExecution<'_, '_>, start: HashId) -> QueryResult<CommitSet> {
    let mut visited = CommitSet::new(connection)?;
    let mut frontier = CommitSet::new(connection)?;
    frontier.insert(connection, start)?;
    while let Some(commit) = frontier.pop_first()? {
        if !visited.insert(connection, commit)? {
            continue;
        }
        for parent in parents(connection, commit)?.into_iter().flatten() {
            if !visited.values.contains(&parent) {
                frontier.insert(connection, parent)?;
            }
        }
    }
    Ok(visited)
}

pub(super) fn is_ancestor(
    connection: &VersionExecution<'_, '_>,
    ancestor: HashId,
    descendant: HashId,
) -> QueryResult<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    Ok(ancestors(connection, descendant)?
        .values
        .contains(&ancestor))
}

pub(super) fn is_first_parent_descendant(
    connection: &VersionExecution<'_, '_>,
    ancestor: HashId,
    mut descendant: HashId,
) -> QueryResult<bool> {
    if descendant == ancestor {
        return Ok(true);
    }
    while descendant != ancestor {
        connection.checkpoint()?;
        let Some(parent) = parents(connection, descendant)?[0] else {
            return Ok(false);
        };
        descendant = parent;
    }
    Ok(true)
}

pub(super) fn best_common_ancestors(
    connection: &VersionExecution<'_, '_>,
    left: HashId,
    right: HashId,
) -> QueryResult<VersionOwned<Vec<HashId>>> {
    let left = ancestors(connection, left)?;
    let right = ancestors(connection, right)?;
    let dominated = dominated_ancestors(connection, &left.values, &right.values)?;
    collect_best_bases(connection, &left.values, &right.values, &dominated.values)
}

fn dominated_ancestors(
    connection: &VersionExecution<'_, '_>,
    left: &BTreeSet<HashId>,
    right: &BTreeSet<HashId>,
) -> QueryResult<CommitSet> {
    let mut dominated = CommitSet::new(connection)?;
    // The intersection is ancestor-closed. Removing immediate parents of its
    // children is exactly the NOT EXISTS(common child) rule in Storage's SQL.
    for child in left.intersection(right) {
        connection.checkpoint()?;
        for parent in parents(connection, *child)?.into_iter().flatten() {
            if left.contains(&parent) && right.contains(&parent) {
                dominated.insert(connection, parent)?;
            }
        }
    }
    Ok(dominated)
}

fn collect_best_bases(
    connection: &VersionExecution<'_, '_>,
    left: &BTreeSet<HashId>,
    right: &BTreeSet<HashId>,
    dominated: &BTreeSet<HashId>,
) -> QueryResult<VersionOwned<Vec<HashId>>> {
    let mut count = 0usize;
    for commit in left.intersection(right) {
        connection.checkpoint()?;
        if !dominated.contains(commit) {
            count = count.checked_add(1).ok_or_else(|| {
                crate::query::resources::memory_error("merge base count overflow")
            })?;
        }
    }
    let memory = connection.reserve(allocation_bytes::<HashId>(count)?)?;
    let mut value = Vec::with_capacity(count);
    for commit in left.intersection(right) {
        connection.checkpoint()?;
        if !dominated.contains(commit) {
            value.push(*commit);
        }
    }
    Ok(VersionOwned { value, memory })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{ExecutionControl, QueryResources};

    fn hash(value: u8) -> HashId {
        HashId::from_bytes([value; 32])
    }

    fn fixture() -> rusqlite::Connection {
        let raw = rusqlite::Connection::open_in_memory().unwrap();
        raw.execute_batch("CREATE TABLE _lithograph_commits(id BLOB PRIMARY KEY,parent1 BLOB,parent2 BLOB,author TEXT,message TEXT);").unwrap();
        raw
    }

    fn append(raw: &rusqlite::Connection, value: u8, first: Option<u8>, second: Option<u8>) {
        let first = first.map(|value| hash(value).as_bytes().to_vec());
        let second = second.map(|value| hash(value).as_bytes().to_vec());
        raw.execute(
            "INSERT INTO _lithograph_commits VALUES(?1,?2,?3,'metadata','message')",
            rusqlite::params![hash(value).as_bytes().as_slice(), first, second],
        )
        .unwrap();
    }

    #[test]
    fn criss_cross_bases_match_storage_sql_and_release_temporary_sets() {
        let raw = fixture();
        for (value, first, second) in [
            (1, None, None),
            (2, Some(1), None),
            (3, Some(1), None),
            (4, Some(2), Some(3)),
            (5, Some(3), Some(2)),
            (6, Some(4), Some(5)),
        ] {
            append(&raw, value, first, second);
        }
        let resources = QueryResources::new(Some(1024 * 1024));
        let never = || false;
        let control = ExecutionControl::new(&resources, &never);
        let connection = VersionExecution::new(&raw, &control);
        let actual = best_common_ancestors(&connection, hash(4), hash(5)).unwrap();
        assert_eq!(
            actual.value,
            storage::best_common_ancestors(&raw, hash(4), hash(5)).unwrap()
        );
        assert_eq!(actual.value, vec![hash(2), hash(3)]);
        assert_eq!(
            resources.used(),
            allocation_bytes::<HashId>(actual.value.capacity()).unwrap()
        );
        drop(actual);
        assert_eq!(resources.used(), 0);
        assert!(is_ancestor(&connection, hash(3), hash(4)).unwrap());
        assert!(!is_first_parent_descendant(&connection, hash(3), hash(4)).unwrap());
        assert!(is_first_parent_descendant(&connection, hash(3), hash(5)).unwrap());
    }

    #[test]
    fn large_metadata_is_not_copied_and_low_budget_rejects_before_tree_growth() {
        let raw = fixture();
        append(&raw, 1, None, None);
        for value in 2..80 {
            append(&raw, value, Some(value - 1), None);
        }
        raw.execute(
            "UPDATE _lithograph_commits SET author = ?1 WHERE id = ?2",
            rusqlite::params!["x".repeat(1024 * 1024), hash(79).as_bytes().as_slice()],
        )
        .unwrap();
        let resources = QueryResources::new(Some(8192));
        let never = || false;
        let control = ExecutionControl::new(&resources, &never);
        let connection = VersionExecution::new(&raw, &control);
        assert_eq!(
            best_common_ancestors(&connection, hash(78), hash(79))
                .unwrap_err()
                .kind,
            QueryErrorKind::Resource
        );
        assert!(resources.peak() <= 8192);
        assert_eq!(resources.used(), 0);
        let resources = QueryResources::new(Some(256 * 1024));
        let control = ExecutionControl::new(&resources, &never);
        let connection = VersionExecution::new(&raw, &control);
        assert!(is_ancestor(&connection, hash(78), hash(79)).unwrap());
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn cancellation_releases_both_traversal_sets() {
        let raw = fixture();
        append(&raw, 1, None, None);
        for value in 2..80 {
            append(&raw, value, Some(value - 1), None);
        }
        let resources = QueryResources::default();
        let checks = std::cell::Cell::new(0);
        let interrupt = || {
            checks.set(checks.get() + 1);
            checks.get() > 4
        };
        let control = ExecutionControl::new(&resources, &interrupt);
        let connection = VersionExecution::new(&raw, &control);
        assert_eq!(
            best_common_ancestors(&connection, hash(78), hash(79))
                .unwrap_err()
                .kind,
            QueryErrorKind::Interrupted
        );
        assert_eq!(resources.used(), 0);
    }
}
