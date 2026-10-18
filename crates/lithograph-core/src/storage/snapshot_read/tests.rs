use std::cell::Cell;

use super::*;
use crate::storage::{
    CommitMetadata, allocate_node_id, branch_head, commit_layer, create_checkpoint,
    create_storage_schema, initialize_root, intern_label, intern_property_key,
};

#[derive(Debug)]
struct BudgetState {
    limit: u64,
    used: Cell<u64>,
    largest_request: Cell<u64>,
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
        self.0
            .largest_request
            .set(self.0.largest_request.get().max(bytes));
        let next = self
            .0
            .used
            .get()
            .checked_add(bytes)
            .ok_or_else(resource_overflow)?;
        if next > self.0.limit {
            return Err(resource_overflow());
        }
        self.0.used.set(next);
        Ok(Box::new(Lease(self.0.clone(), bytes)))
    }
}

fn budget(limit: u64) -> (Rc<BudgetState>, Rc<dyn SnapshotReadObserver>) {
    let state = Rc::new(BudgetState {
        limit,
        used: Cell::new(0),
        largest_request: Cell::new(0),
    });
    (state.clone(), Rc::new(Budget(state)))
}

fn fixture(value: PropertyValue) -> (Connection, HashId, i64, i64) {
    let connection = Connection::open_in_memory().expect("temporary SQLite");
    connection.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    create_storage_schema(&connection).expect("schema");
    initialize_root(&connection).expect("root");
    let head = branch_head(&connection, "main").expect("head");
    let node = allocate_node_id(&connection).expect("NodeId");
    let label = intern_label(&connection, "Large").expect("Label");
    let key = intern_property_key(&connection, "payload").expect("PropertyKey");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("Node");
    layer.add_label(node, label).expect("Label");
    layer
        .set_property(OwnerKind::Node, node, key, value)
        .expect("property");
    let commit = commit_layer(
        &connection,
        "main",
        head,
        None,
        &layer,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 1,
        },
    )
    .expect("commit");
    (connection, commit, node, key)
}

fn assert_resource(error: StorageError) {
    assert!(
        matches!(error, StorageError::Sqlite(rusqlite::Error::SqliteFailure(error, _)) if error.extended_code == rusqlite::ffi::SQLITE_TOOBIG)
    );
}

#[test]
fn large_property_is_reserved_before_pending_layer_and_checkpoint_decode() {
    const PAYLOAD: usize = 32 * 1024 * 1024;
    let (connection, commit, node, key) = fixture(PropertyValue::String("x".repeat(PAYLOAD)));
    let (small, observer) = budget(1024 * 1024);
    assert_resource(
        Snapshot::resolve_query_state_observed(&connection, commit, observer, &|| Ok(()))
            .expect_err("pending Layer exceeds budget before property decode"),
    );
    assert_eq!(small.used.get(), 0);
    assert!(small.largest_request.get() >= PAYLOAD as u64);

    let (large, observer) = budget(256 * 1024 * 1024);
    let state = Snapshot::resolve_query_state_observed(&connection, commit, observer, &|| Ok(()))
        .expect("large budget");
    assert!(large.used.get() >= PAYLOAD as u64);
    let snapshot = Snapshot::from_resolved_state(&connection, &state);
    snapshot
        .with_property(OwnerKind::Node, node, key, |value| {
            assert!(matches!(value, Some(PropertyValue::String(value)) if value.len() == PAYLOAD));
            Ok::<_, StorageError>(())
        })
        .expect("borrow retained overlay value");
    drop(snapshot);
    drop(state);
    assert_eq!(large.used.get(), 0);

    create_checkpoint(&connection, commit).expect("derived checkpoint");
    let (small, observer) = budget(1024 * 1024);
    let state = Snapshot::resolve_query_state_observed(&connection, commit, observer, &|| Ok(()))
        .expect("checkpoint does not eagerly decode property");
    let snapshot = Snapshot::from_resolved_state(&connection, &state);
    let visited = Cell::new(false);
    assert_resource(
        snapshot
            .with_property(OwnerKind::Node, node, key, |_| {
                visited.set(true);
                Ok::<_, StorageError>(())
            })
            .expect_err("checkpoint value must be reserved before callback"),
    );
    assert!(!visited.get());
    assert!(small.largest_request.get() >= PAYLOAD as u64);
    assert_resource(
        snapshot
            .prefetch_properties(OwnerKind::Node, &[node], &[key])
            .expect_err("cache prefetch is budgeted before decode"),
    );
    drop(snapshot);
    drop(state);
    assert_eq!(small.used.get(), 0);
}

#[test]
fn checkpoint_cache_leases_follow_page_and_snapshot_lifetimes() {
    let (connection, commit, node, key) = fixture(PropertyValue::String("x".repeat(128 * 1024)));
    create_checkpoint(&connection, commit).expect("checkpoint");
    let (state, observer) = budget(2 * 1024 * 1024);
    let resolved =
        Snapshot::resolve_query_state_observed(&connection, commit, observer, &|| Ok(()))
            .expect("resolve checkpoint");
    let snapshot = Snapshot::from_resolved_state(&connection, &resolved);
    let schema_memory = state.used.get();
    for _ in 0..4 {
        snapshot
            .prefetch_properties(OwnerKind::Node, &[node], &[key])
            .expect("prefetch one page");
        let retained = state.used.get();
        assert!((schema_memory + 128 * 1024..schema_memory + 256 * 1024).contains(&retained));
        snapshot
            .visit_owner_properties(OwnerKind::Node, node, |found, value| {
                assert_eq!(found, key);
                assert!(matches!(value, PropertyValue::String(value) if value.len() == 128 * 1024));
                Ok::<_, StorageError>(())
            })
            .expect("borrow checkpoint property without whole-owner copy");
        assert_eq!(state.used.get(), retained);
    }
    drop(snapshot);
    assert!(
        state.used.get() > 0,
        "resolved state still owns the shared cache"
    );
    drop(resolved);
    assert_eq!(state.used.get(), 0);
}

#[test]
fn canonical_lists_preflight_decoded_slots_and_observe_scoped_cancellation() {
    let values = (0..8192).map(PropertyValue::Integer).collect();
    let (connection, commit, node, key) = fixture(PropertyValue::List(values));
    create_checkpoint(&connection, commit).expect("checkpoint");
    let (small, observer) = budget(16 * 1024);
    let state = Snapshot::resolve_query_state_observed(&connection, commit, observer, &|| Ok(()))
        .expect("resolve");
    let snapshot = Snapshot::from_resolved_state(&connection, &state);
    assert_resource(
        snapshot
            .with_property(OwnerKind::Node, node, key, |_| Ok::<_, StorageError>(()))
            .expect_err("decoded List slots exceed budget"),
    );
    assert!(small.largest_request.get() > 16 * 1024);
    let checks = Cell::new(0);
    let check = || {
        checks.set(checks.get() + 1);
        if checks.get() >= 256 {
            Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                None,
            )))
        } else {
            Ok(())
        }
    };
    let error = snapshot
        .with_property_checked(OwnerKind::Node, node, key, &check, |_| {
            Ok::<_, StorageError>(())
        })
        .expect_err("canonical scan observes scoped cancellation");
    assert!(
        matches!(error, StorageError::Sqlite(rusqlite::Error::SqliteFailure(error, _)) if error.extended_code == rusqlite::ffi::SQLITE_INTERRUPT)
    );
    assert_eq!(checks.get(), 256);
}

#[test]
fn staged_snapshot_clones_are_reserved_and_replaced_values_release_memory() {
    let (connection, commit, node, key) = fixture(PropertyValue::String("x".repeat(128 * 1024)));
    let (budget, observer) = budget(8 * 1024 * 1024);
    let state = Snapshot::resolve_query_state_observed(&connection, commit, observer, &|| Ok(()))
        .expect("observed snapshot");
    let baseline = budget.used.get();
    let mut snapshot = Snapshot::from_resolved_state(&connection, &state);
    let mut layer = LayerBuilder::default();
    layer
        .set_property(
            OwnerKind::Node,
            node,
            key,
            PropertyValue::String("y".repeat(256 * 1024)),
        )
        .expect("staged value");
    snapshot
        .apply_layer_checked(&layer, &|| Ok(()))
        .expect("checked staged update");
    let larger = budget.used.get();
    layer
        .set_property(
            OwnerKind::Node,
            node,
            key,
            PropertyValue::String("z".repeat(64 * 1024)),
        )
        .expect("replace staged value");
    snapshot
        .apply_layer_checked(&layer, &|| Ok(()))
        .expect("replace checked update");
    assert!(
        budget.used.get() + 256 * 1024 < larger,
        "old overlay and staged value copies were released"
    );
    snapshot
        .with_property(OwnerKind::Node, node, key, |value| {
            assert!(
                matches!(value, Some(PropertyValue::String(value)) if value.len() == 64 * 1024)
            );
            Ok::<_, StorageError>(())
        })
        .expect("current staged value");
    drop(snapshot);
    assert_eq!(budget.used.get(), baseline);
    drop(state);
    assert_eq!(budget.used.get(), 0);
}

#[test]
fn materialized_state_owns_property_and_schema_leases_after_snapshot_drop() {
    let (connection, commit, _, _) = fixture(PropertyValue::String("x".repeat(128 * 1024)));
    create_checkpoint(&connection, commit).expect("checkpoint");
    let (budget, observer) = budget(2 * 1024 * 1024);
    let (state, memory) =
        crate::storage::load_snapshot_state_observed(&connection, commit, observer, &|| Ok(()))
            .expect("materialized state");
    assert_eq!(state.properties.len(), 1);
    assert!(budget.used.get() >= 128 * 1024);
    assert!(
        crate::storage::snapshot_state_memory_bytes(&state, &|| Ok(())).expect("clone sizing")
            >= 128 * 1024
    );
    drop(state);
    drop(memory);
    assert_eq!(budget.used.get(), 0);
}

#[test]
fn sparse_layer_reader_skips_superseded_property_before_owned_decode() {
    let (connection, first, node, key) =
        fixture(PropertyValue::String("x".repeat(2 * 1024 * 1024)));
    let mut latest = LayerBuilder::default();
    latest
        .set_property(
            OwnerKind::Node,
            node,
            key,
            PropertyValue::String("latest".to_owned()),
        )
        .expect("latest value");
    let second = commit_layer(
        &connection,
        "main",
        first,
        None,
        &latest,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 2,
        },
    )
    .expect("new Commit");
    let root = crate::storage::root_commit(&connection).expect("root");
    let (budget, observer) = budget(64 * 1024);
    let (layer, memory) = crate::storage::touched_layer_between_commits_observed(
        &connection,
        root,
        second,
        observer.clone(),
        &|| Ok(()),
    )
    .expect("sparse latest slots fit budget");
    assert_eq!(
        layer
            .properties
            .get(&(OwnerKind::Node, node, key))
            .and_then(|delta| delta.value.as_ref()),
        Some(&PropertyValue::String("latest".to_owned()))
    );
    assert!(
        budget.largest_request.get() < 64 * 1024,
        "superseded 2 MiB payload was never requested"
    );
    drop(layer);
    drop(memory);
    assert_eq!(budget.used.get(), 0);
    let (layer, memory) = crate::storage::layer_between_commits_observed(
        &connection,
        root,
        second,
        observer,
        &|| Ok(()),
    )
    .expect("net sparse Layer fits budget");
    assert_eq!(layer.properties.len(), 1);
    drop(layer);
    drop(memory);
    assert_eq!(budget.used.get(), 0);
}

#[test]
fn candidate_and_full_graph_validation_check_inside_long_node_traversal() {
    let (connection, commit, _, _) = fixture(PropertyValue::Integer(1));
    let mut snapshot = Snapshot::resolve(&connection, commit).expect("parent snapshot");
    let mut layer = LayerBuilder::default();
    for _ in 0..512 {
        let node = allocate_node_id(&connection).expect("allocated NodeId");
        layer.add_node(node).expect("candidate node");
    }
    snapshot
        .validate_layer_invariants_checked(&layer, &|| Ok(()))
        .expect("valid candidate");
    let calls = Cell::new(0);
    let check = || {
        calls.set(calls.get() + 1);
        if calls.get() == 257 {
            Err(StorageError::not_found("cancel candidate validation"))
        } else {
            Ok(())
        }
    };
    let error = snapshot
        .validate_layer_invariants_checked(&layer, &check)
        .expect_err("candidate validation observes cancellation");
    assert!(error.to_string().contains("cancel candidate validation"));
    assert_eq!(
        calls.get(),
        257,
        "remaining candidate entries were not visited"
    );
    snapshot
        .validate_layer_invariants_checked(&layer, &|| Ok(()))
        .expect("retry remains usable");
    snapshot
        .apply_layer_checked(&layer, &|| Ok(()))
        .expect("staged candidate");
    calls.set(0);
    let error = snapshot
        .validate_graph_invariants_checked(&check)
        .expect_err("full validation observes cancellation");
    assert!(error.to_string().contains("cancel candidate validation"));
    assert_eq!(calls.get(), 257);
    snapshot
        .validate_graph_invariants()
        .expect("full validation retry");
}

#[test]
fn unique_staged_updates_are_linear_and_failed_copy_on_write_is_atomic() {
    const UPDATES: usize = 5000;
    let (connection, commit, _, key) = fixture(PropertyValue::Integer(1));
    let (budget, observer) = budget(256 * 1024 * 1024);
    let resolved =
        Snapshot::resolve_query_state_observed(&connection, commit, observer, &|| Ok(()))
            .expect("observed snapshot");
    let mut snapshot = Snapshot::from_resolved_state(&connection, &resolved);
    let calls = Cell::new(0_usize);
    let check = || {
        calls.set(calls.get() + 1);
        Ok(())
    };
    for index in 0..UPDATES {
        let node = 10000 + index as i64;
        let mut delta = LayerBuilder::default();
        delta.add_node(node).expect("new node");
        delta
            .set_property(
                OwnerKind::Node,
                node,
                key,
                PropertyValue::Integer(index as i64),
            )
            .expect("new property");
        snapshot
            .apply_layer_checked(&delta, &check)
            .expect("incremental update");
    }
    assert!(
        calls.get() < UPDATES * 128,
        "{calls:?}: accumulated Layer was traversed again"
    );
    let local = snapshot.query_local_layer().expect("local Layer");
    assert_eq!(local.nodes.len(), UPDATES);
    assert_eq!(local.properties.len(), UPDATES);
    let shared = snapshot.clone();
    let retained = budget.used.get();
    let mut replacement = LayerBuilder::default();
    replacement
        .set_property(OwnerKind::Node, 10000, key, PropertyValue::Integer(-1))
        .expect("replacement");
    calls.set(0);
    let cancel = || {
        calls.set(calls.get() + 1);
        if calls.get() == 257 {
            Err(StorageError::not_found("cancel shared Layer copy"))
        } else {
            Ok(())
        }
    };
    assert!(snapshot.apply_layer_checked(&replacement, &cancel).is_err());
    assert_eq!(calls.get(), 257);
    assert_eq!(
        budget.used.get(),
        retained,
        "failed COW released all temporary claims"
    );
    let value = snapshot
        .query_local_layer()
        .expect("unchanged Layer")
        .properties
        .get(&(OwnerKind::Node, 10000, key))
        .and_then(|delta| delta.value.as_ref());
    assert_eq!(value, Some(&PropertyValue::Integer(0)));
    drop(shared);
    drop(snapshot);
    drop(resolved);
    assert_eq!(budget.used.get(), 0);
}

#[test]
fn staged_sparse_tree_capacity_is_reserved_before_first_entry() {
    let (connection, commit, node, key) = fixture(PropertyValue::Integer(1));
    let mut snapshot = Snapshot::resolve(&connection, commit).expect("parent snapshot");
    let (budget, observer) = budget(1024);
    snapshot.state.read_observer = Some(observer);
    let mut delta = LayerBuilder::default();
    delta
        .set_property(OwnerKind::Node, node, key, PropertyValue::Integer(2))
        .expect("property");
    assert_resource(
        snapshot
            .apply_layer_checked(&delta, &|| Ok(()))
            .expect_err("sparse trees exceed budget"),
    );
    assert!(
        budget.largest_request.get() > 1024,
        "whole tree nodes and lease table were charged"
    );
    assert_eq!(budget.used.get(), 0);
    assert!(snapshot.query_local_layer().is_none());
}

#[test]
fn observed_overlay_reserves_sparse_tree_nodes_before_insertion() {
    let mut node = LayerBuilder::default();
    node.add_node(1).unwrap();
    let mut label = LayerBuilder::default();
    label.add_label(1, 1).unwrap();
    let mut relationship = LayerBuilder::default();
    relationship
        .add_relationship(crate::storage::RelationshipRecord {
            id: 1,
            source: 1,
            type_id: 1,
            target: 2,
        })
        .unwrap();
    let mut property = LayerBuilder::default();
    property.remove_property(OwnerKind::Node, 1, 1).unwrap();
    for layer in [node, label, relationship, property] {
        let (budget, observer) = budget(512);
        let mut overlay = crate::storage::snapshot::Overlay::default();
        assert_resource(
            overlay
                .apply_layer_observed(&layer, Some(observer.as_ref()), &|| Ok(()))
                .expect_err("the first B-tree node must be reserved before insertion"),
        );
        assert!(overlay.nodes.is_empty());
        assert!(overlay.labels.is_empty());
        assert!(overlay.relationships.is_empty());
        assert!(overlay.properties.is_empty());
        assert_eq!(budget.used.get(), 0);
    }
}

#[test]
fn sparse_checkpoint_cache_slot_is_reserved_before_insertion() {
    let (connection, commit, node, key) = fixture(PropertyValue::Integer(1));
    create_checkpoint(&connection, commit).unwrap();
    let mut snapshot = Snapshot::resolve(&connection, commit).unwrap();
    let (budget, observer) = budget(512);
    snapshot.state.read_observer = Some(observer);
    assert_resource(
        snapshot
            .prefetch_properties(OwnerKind::Node, &[node], &[key + 1])
            .expect_err("a missing property's cache slot still allocates a B-tree node"),
    );
    assert!(snapshot.state.property_cache.borrow().is_empty());
    assert_eq!(budget.used.get(), 0);
}

#[test]
fn materialized_property_visitors_transfer_all_value_leases_after_snapshot_drop() {
    const PAYLOAD: usize = 128 * 1024;
    let (connection, first, _, key) = fixture(PropertyValue::String("x".repeat(PAYLOAD)));
    let node = allocate_node_id(&connection).unwrap();
    let label = intern_label(&connection, "Large").unwrap();
    let mut layer = LayerBuilder::default();
    layer.add_node(node).unwrap();
    layer.add_label(node, label).unwrap();
    layer
        .set_property(
            OwnerKind::Node,
            node,
            key,
            PropertyValue::String("y".repeat(PAYLOAD)),
        )
        .unwrap();
    let commit = commit_layer(
        &connection,
        "main",
        first,
        None,
        &layer,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 2,
        },
    )
    .unwrap();
    create_checkpoint(&connection, commit).unwrap();
    for staged in [false, true] {
        let (budget, observer) = budget(4 * 1024 * 1024);
        let state =
            Snapshot::resolve_query_state_observed(&connection, commit, observer, &|| Ok(()))
                .unwrap();
        let mut snapshot = Snapshot::from_resolved_state(&connection, &state);
        if staged {
            let mut delta = LayerBuilder::default();
            delta
                .add_node(allocate_node_id(&connection).unwrap())
                .unwrap();
            snapshot.apply_layer_checked(&delta, &|| Ok(())).unwrap();
        }
        let mut values = Vec::new();
        let mut memory = Vec::new();
        snapshot
            .visit_properties_observed(&|| Ok(()), |_, _, _, value, lease| {
                values.push(value);
                memory.extend(lease);
                Ok(())
            })
            .unwrap();
        assert_eq!(values.len(), 2);
        drop(snapshot);
        drop(state);
        assert!(budget.used.get() >= (2 * PAYLOAD) as u64);
        assert!(matches!(&values[0], PropertyValue::String(value) if value.len() == PAYLOAD));
        drop(values);
        drop(memory);
        assert_eq!(budget.used.get(), 0);

        let (small, observer) = self::budget(320 * 1024);
        let state =
            Snapshot::resolve_query_state_observed(&connection, commit, observer, &|| Ok(()))
                .unwrap();
        let mut snapshot = Snapshot::from_resolved_state(&connection, &state);
        if staged {
            let mut delta = LayerBuilder::default();
            delta
                .add_node(allocate_node_id(&connection).unwrap())
                .unwrap();
            snapshot.apply_layer_checked(&delta, &|| Ok(())).unwrap();
        }
        let mut values = Vec::new();
        let mut memory = Vec::new();
        let error = snapshot
            .visit_properties_observed(&|| Ok(()), |_, _, _, value, lease| {
                values.push(value);
                memory.extend(lease);
                Ok(())
            })
            .expect_err("the prior row stays charged while decoding the next row");
        assert_resource(error);
        drop(values);
        drop(memory);
        drop(snapshot);
        drop(state);
        assert_eq!(small.used.get(), 0);
    }
}
