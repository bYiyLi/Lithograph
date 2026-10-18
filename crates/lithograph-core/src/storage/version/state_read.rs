//! Budgeted materialized version state, sharing the query's neutral read observer.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::rc::Rc;

use rusqlite::Connection;

use super::state::SnapshotState;
use crate::storage::read_memory::property_value_bytes;
use crate::storage::snapshot_read::{self, ReadLease, SnapshotReadObserver};
use crate::storage::{HashId, Snapshot, StorageResult};

#[derive(Debug)]
struct StateMemory {
    _leases: Vec<ReadLease>,
}

pub(crate) fn load_snapshot_state_observed(
    connection: &Connection,
    commit: HashId,
    observer: Rc<dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<(SnapshotState, Rc<dyn Debug>)> {
    let snapshot = Snapshot::resolve_observed(connection, commit, observer.clone(), check)?;
    let mut leases = Vec::new();
    let mut nodes = BTreeSet::new();
    snapshot.visit_nodes(|node| {
        check()?;
        retain(&mut leases, observer.as_ref(), 256)?;
        nodes.insert(node);
        Ok(())
    })?;
    let mut labels = BTreeSet::new();
    snapshot.visit_labels(|node, label| {
        check()?;
        retain(&mut leases, observer.as_ref(), 256)?;
        labels.insert((node, label));
        Ok(())
    })?;
    let mut relationships = BTreeMap::new();
    snapshot.visit_relationships(|relationship| {
        check()?;
        retain(&mut leases, observer.as_ref(), 256)?;
        relationships.insert(relationship.id, relationship);
        Ok(())
    })?;
    let mut properties = BTreeMap::new();
    snapshot.visit_properties_observed(check, |owner, id, key, value, memory| {
        if let Some(memory) = memory {
            leases.push(memory);
        }
        properties.insert((owner, id, key), value);
        Ok(())
    })?;
    let schema = clone_schema(&snapshot, &mut leases, observer.as_ref(), check)?;
    Ok((
        SnapshotState {
            nodes,
            labels,
            relationships,
            properties,
            schema,
        },
        Rc::new(StateMemory { _leases: leases }),
    ))
}

fn clone_schema(
    snapshot: &Snapshot<'_>,
    leases: &mut Vec<ReadLease>,
    observer: &dyn SnapshotReadObserver,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<crate::storage::SchemaState> {
    let schema = snapshot.query_schema_state().ok_or_else(|| {
        crate::storage::StorageError::corrupt("observed Snapshot is missing Schema state")
    })?;
    retain(
        leases,
        observer,
        crate::storage::schema_state_memory_bytes(schema, check)?,
    )?;
    schema.clone_checked(check)
}

pub(crate) fn snapshot_state_memory_bytes(
    state: &SnapshotState,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<u64> {
    let metadata = state
        .nodes
        .len()
        .checked_add(state.labels.len())
        .and_then(|value| value.checked_add(state.relationships.len()))
        .ok_or_else(|| crate::storage::StorageError::corrupt("Snapshot state size overflow"))?;
    let mut bytes = (metadata as u64)
        .checked_mul(256)
        .ok_or_else(|| crate::storage::StorageError::corrupt("Snapshot state size overflow"))?;
    for value in state.properties.values() {
        check()?;
        bytes = snapshot_read::checked_add(
            bytes,
            snapshot_read::checked_add(property_value_bytes(value, check)?, 256)?,
        )?;
    }
    snapshot_read::checked_add(
        bytes,
        crate::storage::schema_state_memory_bytes(&state.schema, check)?,
    )
}

fn retain(
    leases: &mut Vec<ReadLease>,
    observer: &dyn SnapshotReadObserver,
    bytes: u64,
) -> StorageResult<()> {
    if let Some(lease) = snapshot_read::reserve(Some(observer), bytes)? {
        leases.push(lease);
    }
    Ok(())
}
