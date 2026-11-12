//! Query-owned leases for immutable states still consumed after a write.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Mutex;

use crate::storage::HashId;

use super::{
    MemoryReservation, QueryError, QueryResources, QueryResult, allocation_bytes, map_entry_bytes,
};

type PinKey = (HashId, HashId, Option<HashId>);
pub(super) type SnapshotPins = Mutex<BTreeMap<PinKey, usize>>;

#[derive(Debug)]
struct SnapshotPin {
    resources: QueryResources,
    key: PinKey,
    _memory: MemoryReservation,
}

impl QueryResources {
    pub(crate) fn pin_snapshot(
        &self,
        database: HashId,
        commit: HashId,
        checkpoint: Option<HashId>,
    ) -> QueryResult<Box<dyn Debug>> {
        // Each independently resolved state conservatively pays for an entry
        // and its lease, including when another state already holds that key.
        // The map itself never owns a resource lease/Arc back to its owner.
        let memory = self.reserve(map_entry_bytes::<PinKey>()? + 256)?;
        let key = (database, commit, checkpoint);
        let mut pins = self.state.snapshot_pins.lock().map_err(|_| pin_error())?;
        let count = pins.get(&key).copied().unwrap_or(0);
        let next = count.checked_add(1).ok_or_else(pin_error)?;
        pins.insert(key, next);
        drop(pins);
        Ok(Box::new(SnapshotPin {
            resources: self.clone(),
            key,
            _memory: memory,
        }))
    }

    pub(crate) fn checkpoint_is_pinned(
        &self,
        database: HashId,
        checkpoint: HashId,
    ) -> QueryResult<bool> {
        let pins = self.state.snapshot_pins.lock().map_err(|_| pin_error())?;
        Ok(pins
            .keys()
            .any(|(scope, _, base)| *scope == database && *base == Some(checkpoint)))
    }

    pub(crate) fn pinned_snapshot_commits(
        &self,
        database: HashId,
    ) -> QueryResult<(Vec<HashId>, MemoryReservation)> {
        let pins = self.state.snapshot_pins.lock().map_err(|_| pin_error())?;
        let memory = self.reserve(allocation_bytes::<HashId>(pins.len())?)?;
        let mut commits = Vec::with_capacity(pins.len());
        for (scope, commit, _) in pins.keys() {
            if *scope == database && commits.last() != Some(commit) {
                commits.push(*commit);
            }
        }
        Ok((commits, memory))
    }
}

impl Drop for SnapshotPin {
    fn drop(&mut self) {
        // A panic must not strand a pin or panic again while unwinding. Only
        // this small map is touched while locked; no SQLite callback runs here.
        let mut pins = self
            .resources
            .state
            .snapshot_pins
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(count) = pins.get_mut(&self.key) {
            if *count > 1 {
                *count -= 1;
            } else {
                pins.remove(&self.key);
            }
        }
    }
}

fn pin_error() -> QueryError {
    QueryError::internal("immutable Snapshot pin state is unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn database_identity_distinguishes_file_copies_and_rejects_broken_public_metadata() {
        let first = tempfile::NamedTempFile::new().unwrap();
        let second = tempfile::NamedTempFile::new().unwrap();
        let one = rusqlite::Connection::open(first.path()).unwrap();
        let two = rusqlite::Connection::open(second.path()).unwrap();
        let bare_one = crate::storage::snapshot_pin_identity(&one).unwrap();
        let bare_two = crate::storage::snapshot_pin_identity(&two).unwrap();
        assert_ne!(bare_one, bare_two);
        let bare_reopened = rusqlite::Connection::open(first.path()).unwrap();
        assert_eq!(
            crate::storage::snapshot_pin_identity(&bare_reopened).unwrap(),
            bare_one
        );
        for connection in [&one, &two] {
            connection
                .execute_batch(
                    "CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,database_id TEXT);\
                 INSERT INTO _lithograph_meta VALUES(1,'00000000-0000-4000-8000-000000000001');",
                )
                .unwrap();
        }
        let one_id = crate::storage::snapshot_pin_identity(&one).unwrap();
        let two_id = crate::storage::snapshot_pin_identity(&two).unwrap();
        assert_ne!(
            one_id, bare_one,
            "UUID and bare storage scopes must use different domains"
        );
        assert_ne!(
            one_id, two_id,
            "file copies must not share query-local pin scope"
        );
        let reopened = rusqlite::Connection::open(first.path()).unwrap();
        assert_eq!(
            crate::storage::snapshot_pin_identity(&reopened).unwrap(),
            one_id
        );
        for value in [None, Some(String::new()), Some("x".repeat(257))] {
            one.execute("UPDATE _lithograph_meta SET database_id=?1", [value])
                .unwrap();
            assert!(crate::storage::snapshot_pin_identity(&one).is_err());
        }
        one.execute("UPDATE _lithograph_meta SET database_id=zeroblob(8)", [])
            .unwrap();
        assert!(
            crate::storage::snapshot_pin_identity(&one).is_err(),
            "wrong metadata type was accepted"
        );
        one.execute("DELETE FROM _lithograph_meta", []).unwrap();
        assert_eq!(
            crate::storage::snapshot_pin_identity(&one).unwrap(),
            bare_one
        );
        assert_eq!(
            crate::storage::snapshot_pin_identity(&reopened).unwrap(),
            bare_one
        );
    }

    #[test]
    fn snapshot_pins_follow_shared_state_lifetime_and_database_scope() {
        let resources = QueryResources::default();
        let database = HashId::from_bytes([1; 32]);
        let other_database = HashId::from_bytes([2; 32]);
        let commit = HashId::from_bytes([3; 32]);
        let checkpoint = HashId::from_bytes([4; 32]);
        let pin: Rc<dyn Debug> = Rc::from(
            resources
                .pin_snapshot(database, commit, Some(checkpoint))
                .unwrap(),
        );
        let shared = pin.clone();
        assert!(resources.used() > 0);
        assert!(
            resources
                .checkpoint_is_pinned(database, checkpoint)
                .unwrap()
        );
        assert!(
            !resources
                .checkpoint_is_pinned(other_database, checkpoint)
                .unwrap()
        );
        let roots = resources.pinned_snapshot_commits(database).unwrap();
        assert_eq!(roots.0, vec![commit]);
        drop(roots);
        drop(pin);
        assert!(
            resources
                .checkpoint_is_pinned(database, checkpoint)
                .unwrap()
        );
        drop(shared);
        assert!(
            !resources
                .checkpoint_is_pinned(database, checkpoint)
                .unwrap()
        );
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn uncheckpointed_commits_are_pinned_and_low_quota_failure_leaves_no_pin() {
        let database = HashId::from_bytes([1; 32]);
        let commit = HashId::from_bytes([2; 32]);
        let tiny = QueryResources::new(Some(1));
        assert!(tiny.pin_snapshot(database, commit, None).is_err());
        assert_eq!(tiny.used(), 0);
        assert!(tiny.state.snapshot_pins.lock().unwrap().is_empty());
        let resources = QueryResources::default();
        let first = resources.pin_snapshot(database, commit, None).unwrap();
        let second = resources.pin_snapshot(database, commit, None).unwrap();
        drop(first);
        let roots = resources.pinned_snapshot_commits(database).unwrap();
        assert_eq!(roots.0, vec![commit]);
        drop(roots);
        drop(second);
        assert_eq!(resources.used(), 0);
    }
}
