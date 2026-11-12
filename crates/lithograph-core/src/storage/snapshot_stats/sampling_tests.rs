use std::cell::Cell;

use super::tests::{metadata, storage};
use super::*;
use crate::storage::{
    HashId, LayerBuilder, RelationshipRecord, SchemaState, allocate_node_id,
    allocate_relationship_id, branch_head, commit_layer, create_checkpoint, delete_checkpoint,
    intern_label, intern_relationship_type,
};
use rusqlite::Connection;

struct Fixture {
    connection: Connection,
    commit: HashId,
    nodes: Vec<NodeId>,
    labels: [LabelId; 2],
    types: [RelationshipTypeId; 2],
    relationships: Vec<RelationshipRecord>,
}

fn fixture() -> Fixture {
    let connection = storage();
    let labels = [
        intern_label(&connection, "A").unwrap(),
        intern_label(&connection, "B").unwrap(),
    ];
    let types = [
        intern_relationship_type(&connection, "A").unwrap(),
        intern_relationship_type(&connection, "B").unwrap(),
    ];
    let mut layer = LayerBuilder::default();
    let mut nodes = Vec::new();
    for index in 0..6 {
        let node = allocate_node_id(&connection).unwrap();
        layer.add_node(node).unwrap();
        if index < 4 {
            layer.add_label(node, labels[0]).unwrap();
        }
        if index == 0 || index == 2 {
            layer.add_label(node, labels[1]).unwrap();
        }
        nodes.push(node);
    }
    let mut relationships = Vec::new();
    for (source, target, type_id) in [
        (0, 1, types[0]),
        (0, 1, types[0]),
        (1, 0, types[0]),
        (0, 0, types[0]),
        (2, 3, types[0]),
        (0, 2, types[1]),
    ] {
        let record = RelationshipRecord {
            id: allocate_relationship_id(&connection).unwrap(),
            source: nodes[source],
            target: nodes[target],
            type_id,
        };
        layer.add_relationship(record).unwrap();
        relationships.push(record);
    }
    let head = branch_head(&connection, "main").unwrap();
    let commit = commit_layer(&connection, "main", head, None, &layer, &metadata(1)).unwrap();
    create_checkpoint(&connection, commit).unwrap();
    Fixture {
        connection,
        commit,
        nodes,
        labels,
        types,
        relationships,
    }
}

#[test]
fn bounded_totals_include_requested_relationship_types_and_reject_tight_limits() {
    let f = fixture();
    let snapshot = Snapshot::resolve(&f.connection, f.commit).unwrap();
    let labels = BTreeSet::from(f.labels);
    let types = BTreeSet::from(f.types);
    let counts = snapshot
        .bounded_statistics(&labels, &types, true, 4096, 65536)
        .unwrap()
        .unwrap();
    assert_eq!(counts.node_count, 6);
    assert_eq!(counts.relationship_count, 6);
    assert_eq!(counts.type_counts[&f.types[0]], 5);
    assert_eq!(counts.type_counts[&f.types[1]], 1);
    assert!(
        snapshot
            .bounded_statistics(&labels, &types, true, 0, 65536)
            .unwrap()
            .is_none()
    );
    assert!(
        snapshot
            .bounded_statistics(&labels, &types, true, 4096, 1)
            .unwrap()
            .is_none()
    );
}

#[test]
fn bounded_totals_ignore_non_blob_derived_metadata_without_decoding_it() {
    let f = fixture();
    let snapshot = Snapshot::resolve(&f.connection, f.commit).unwrap();
    f.connection.execute(
        "UPDATE main._lithograph_checkpoints SET metadata=CAST(zeroblob(65536) AS TEXT) WHERE commit_id=?1",
        [f.commit.as_bytes().as_slice()],
    ).unwrap();
    assert_eq!(snapshot.bounded_statistics_decode_work().unwrap(), None);
    assert!(
        snapshot
            .bounded_statistics(
                &BTreeSet::from(f.labels),
                &BTreeSet::from(f.types),
                true,
                4096,
                65536,
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn totals_work_admission_matches_overlay_cost_and_saturates_without_traversal() {
    let f = fixture();
    let node = allocate_node_id(&f.connection).unwrap();
    let mut layer = LayerBuilder::default();
    layer.add_node(node).unwrap();
    for label in f.labels {
        layer.add_label(node, label).unwrap();
    }
    layer
        .add_relationship(RelationshipRecord {
            id: allocate_relationship_id(&f.connection).unwrap(),
            source: node,
            target: f.nodes[0],
            type_id: f.types[0],
        })
        .unwrap();
    let snapshot = Snapshot::resolve_with_layer_and_schema(
        &f.connection,
        f.commit,
        &layer,
        SchemaState::load(&f.connection, f.commit).unwrap(),
    )
    .unwrap();
    assert_eq!(snapshot.bounded_statistics_work(2, 2, true), 14);
    assert_eq!(snapshot.bounded_statistics_work(2, 0, false), 11);
    assert_eq!(snapshot.bounded_statistics_work(2, 0, true), 12);
    assert_eq!(snapshot.bounded_statistics_work(0, 1, false), 7);
    assert_eq!(
        snapshot.bounded_statistics_work(usize::MAX, usize::MAX, true),
        usize::MAX
    );
    let labels = BTreeSet::from(f.labels);
    let types = BTreeSet::from(f.types);
    let decode = snapshot.bounded_statistics_decode_work().unwrap().unwrap();
    assert!(decode > 0);
    let encoded: i64 = f
        .connection
        .query_row(
            "SELECT length(metadata) FROM main._lithograph_checkpoints WHERE commit_id=?1",
            [f.commit.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decode, usize::try_from(encoded).unwrap());
    let work = snapshot.bounded_statistics_work(labels.len(), types.len(), true) + decode;
    assert!(
        snapshot
            .bounded_statistics(&labels, &types, true, work, 65536)
            .unwrap()
            .is_some()
    );
    assert!(
        snapshot
            .bounded_statistics(&labels, &types, true, work - 1, 65536)
            .unwrap()
            .is_none()
    );
}

#[test]
fn decode_work_is_zero_without_checkpoint_and_unknown_for_a_missing_derived_row() {
    let connection = storage();
    let root = branch_head(&connection, "main").unwrap();
    let snapshot = Snapshot::resolve(&connection, root).unwrap();
    assert_eq!(snapshot.bounded_statistics_decode_work().unwrap(), Some(0));
    let labels = BTreeSet::new();
    let types = BTreeSet::new();
    let base = snapshot.bounded_statistics_work(0, 0, false);
    assert_eq!(base, 4);
    assert!(
        snapshot
            .bounded_statistics(&labels, &types, false, base, 0)
            .unwrap()
            .is_some()
    );

    let f = fixture();
    let snapshot = Snapshot::resolve(&f.connection, f.commit).unwrap();
    delete_checkpoint(&f.connection, f.commit).unwrap();
    assert_eq!(snapshot.bounded_statistics_decode_work().unwrap(), None);
}

#[derive(Debug)]
struct CancelDecodeProbe;

impl crate::storage::snapshot_read::SnapshotReadObserver for CancelDecodeProbe {
    fn reserve(&self, _bytes: u64) -> StorageResult<Box<dyn std::fmt::Debug>> {
        Ok(Box::new(()))
    }

    fn checkpoint(&self) -> StorageResult<()> {
        Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
            Some("cancel decode-work probe".to_owned()),
        )))
    }
}

#[test]
fn decode_work_preserves_snapshot_cancellation_and_sqlite_failures() {
    let f = fixture();
    let mut snapshot = Snapshot::resolve(&f.connection, f.commit).unwrap();
    snapshot.state.read_observer = Some(std::rc::Rc::new(CancelDecodeProbe));
    let error = snapshot.bounded_statistics_decode_work().unwrap_err();
    assert!(
        matches!(error, StorageError::Sqlite(rusqlite::Error::SqliteFailure(error, _))
        if error.code == rusqlite::ErrorCode::OperationInterrupted)
    );
    snapshot.state.read_observer = None;
    f.connection
        .execute_batch("DROP TABLE main._lithograph_checkpoints")
        .unwrap();
    assert!(snapshot.bounded_statistics_decode_work().is_err());
}

#[test]
fn combination_and_directed_degree_keep_parallel_edges_and_single_incident_loops() {
    let f = fixture();
    let snapshot = Snapshot::resolve(&f.connection, f.commit).unwrap();
    let labels = BTreeSet::from(f.labels);
    let combined = snapshot
        .bounded_label_combination((f.labels[0], 4), &labels, 16, 4096, 4096, &|| Ok(()))
        .unwrap()
        .unwrap();
    assert_eq!(
        (combined.expected, combined.lower, combined.upper),
        (2, 2, 2)
    );
    assert_eq!((combined.sample_rows, combined.sample_matches), (4, 2));
    assert!(combined.exhaustive);
    assert!(combined.work <= 4096);
    assert_eq!(combined.bytes, 4096);
    for (direction, expected, buckets, maximum) in [
        (Some(false), 5, [1, 2, 1, 0, 0], 3),
        (Some(true), 5, [1, 1, 2, 0, 0], 2),
        (None, 9, [0, 2, 2, 0, 0], 4),
    ] {
        let degrees = snapshot
            .bounded_degree_statistics(
                (Some(f.labels[0]), 4),
                Some(f.types[0]),
                direction,
                16,
                4096,
                16384,
                &|| Ok(()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            (degrees.expected, degrees.lower, degrees.upper),
            (expected, expected, Some(expected))
        );
        assert_eq!(degrees.mean, expected as f64 / 4.0);
        assert_eq!(degrees.max_observed, maximum);
        assert_eq!(degrees.buckets, buckets);
        assert_eq!((degrees.sample_rows, degrees.truncated_nodes), (4, 0));
        assert!(degrees.exhaustive);
        assert!(degrees.work <= 4096);
        assert_eq!(degrees.bytes, 16384);
    }
    let untyped = snapshot
        .bounded_degree_statistics((None, 6), None, Some(false), 16, 4096, 16384, &|| Ok(()))
        .unwrap()
        .unwrap();
    assert_eq!(
        (untyped.expected, untyped.lower, untyped.upper),
        (6, 6, Some(6))
    );
    assert_eq!(untyped.mean, 1.0);
}

#[test]
fn historical_and_staged_samples_read_their_own_overlay_without_persistent_writes() {
    let f = fixture();
    let new_node = allocate_node_id(&f.connection).unwrap();
    let mut changes = LayerBuilder::default();
    changes.add_node(new_node).unwrap();
    changes.add_label(new_node, f.labels[0]).unwrap();
    changes.add_label(new_node, f.labels[1]).unwrap();
    changes.remove_label(f.nodes[0], f.labels[1]).unwrap();
    changes.add_label(f.nodes[1], f.labels[1]).unwrap();
    changes.remove_relationship(f.relationships[0]).unwrap();
    changes
        .add_relationship(RelationshipRecord {
            id: allocate_relationship_id(&f.connection).unwrap(),
            source: new_node,
            target: f.nodes[0],
            type_id: f.types[0],
        })
        .unwrap();
    let old = Snapshot::resolve(&f.connection, f.commit).unwrap();
    let staged = Snapshot::resolve_with_layer_and_schema(
        &f.connection,
        f.commit,
        &changes,
        SchemaState::load(&f.connection, f.commit).unwrap(),
    )
    .unwrap();
    let before = f.connection.total_changes();
    for (snapshot, domain, matches) in [(&old, 4, 2), (&staged, 5, 3)] {
        let sample = snapshot
            .bounded_label_combination(
                (f.labels[0], domain),
                &BTreeSet::from(f.labels),
                16,
                4096,
                4096,
                &|| Ok(()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(sample.expected, matches);
        assert!(sample.exhaustive);
        let degrees = snapshot
            .bounded_degree_statistics(
                (Some(f.labels[0]), domain),
                Some(f.types[0]),
                Some(false),
                16,
                4096,
                16384,
                &|| Ok(()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(degrees.expected, 5);
        assert_eq!(degrees.upper, Some(5));
    }
    assert_eq!(f.connection.total_changes(), before);
    assert_eq!(branch_head(&f.connection, "main").unwrap(), f.commit);
    drop((old, staged));
    let current = commit_layer(
        &f.connection,
        "main",
        f.commit,
        None,
        &changes,
        &metadata(2),
    )
    .unwrap();
    let snapshot = Snapshot::resolve(&f.connection, current).unwrap();
    let totals = snapshot
        .bounded_statistics(
            &BTreeSet::from(f.labels),
            &BTreeSet::from(f.types),
            true,
            4096,
            65536,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        (totals.relationship_count, totals.type_counts[&f.types[0]]),
        (6, 5)
    );
}

#[test]
fn samples_respect_caps_empty_domains_and_stale_counts() {
    let f = fixture();
    let snapshot = Snapshot::resolve(&f.connection, f.commit).unwrap();
    for (samples, work, bytes) in [(0, 4096, 4096), (16, 0, 4096), (16, 4096, 1)] {
        assert!(
            snapshot
                .bounded_label_combination(
                    (f.labels[0], 4),
                    &BTreeSet::from(f.labels),
                    samples,
                    work,
                    bytes,
                    &|| Ok(()),
                )
                .unwrap()
                .is_none()
        );
    }
    for (samples, work, bytes) in [(0, 4096, 16384), (16, 0, 16384), (16, 4096, 1)] {
        assert!(
            snapshot
                .bounded_degree_statistics(
                    (Some(f.labels[0]), 4),
                    Some(f.types[0]),
                    Some(false),
                    samples,
                    work,
                    bytes,
                    &|| Ok(()),
                )
                .unwrap()
                .is_none()
        );
    }
    assert!(
        snapshot
            .bounded_label_combination(
                (f.labels[0], 0),
                &BTreeSet::from(f.labels),
                16,
                4096,
                4096,
                &|| Ok(()),
            )
            .unwrap()
            .is_none()
    );
    assert!(
        snapshot
            .bounded_label_combination(
                (f.labels[0], 1),
                &BTreeSet::from(f.labels),
                16,
                4096,
                4096,
                &|| Ok(()),
            )
            .unwrap()
            .is_none()
    );
    assert!(
        snapshot
            .bounded_degree_statistics(
                (Some(f.labels[0]), 4),
                Some(i64::MAX),
                Some(false),
                16,
                4096,
                16384,
                &|| Ok(()),
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn sampling_propagates_read_cancellation() {
    let f = fixture();
    let snapshot = Snapshot::resolve(&f.connection, f.commit).unwrap();
    let calls = Cell::new(0);
    let cancel = || {
        calls.set(calls.get() + 1);
        if calls.get() >= 3 {
            Err(StorageError::corrupt("test cancelled"))
        } else {
            Ok(())
        }
    };
    assert!(
        snapshot
            .bounded_label_combination(
                (f.labels[0], 4),
                &BTreeSet::from(f.labels),
                16,
                4096,
                4096,
                &cancel,
            )
            .is_err()
    );
    calls.set(0);
    assert!(
        snapshot
            .bounded_degree_statistics(
                (Some(f.labels[0]), 4),
                Some(f.types[0]),
                Some(false),
                16,
                4096,
                16384,
                &cancel,
            )
            .is_err()
    );
}

#[test]
fn capped_skew_and_zero_hit_partial_samples_keep_unknown_tails() {
    let f = fixture();
    let mut layer = LayerBuilder::default();
    for _ in 0..80 {
        layer
            .add_relationship(RelationshipRecord {
                id: allocate_relationship_id(&f.connection).unwrap(),
                source: f.nodes[0],
                target: f.nodes[1],
                type_id: f.types[0],
            })
            .unwrap();
    }
    let snapshot = Snapshot::resolve_with_layer_and_schema(
        &f.connection,
        f.commit,
        &layer,
        SchemaState::load(&f.connection, f.commit).unwrap(),
    )
    .unwrap();
    let degrees = snapshot
        .bounded_degree_statistics(
            (Some(f.labels[0]), 4),
            Some(f.types[0]),
            Some(false),
            16,
            768,
            16384,
            &|| Ok(()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(degrees.upper, None);
    assert_eq!(degrees.truncated_nodes, 1);
    assert_eq!(degrees.max_observed, 33);
    assert_eq!(degrees.buckets[4], 1);
    assert!(!degrees.exhaustive);
    assert!(degrees.work <= 768);
    let absent = BTreeSet::from([f.labels[0], i64::MAX]);
    let combined = snapshot
        .bounded_label_combination((f.labels[0], 4), &absent, 2, 100, 4096, &|| Ok(()))
        .unwrap()
        .unwrap();
    assert_eq!(combined.expected, 1);
    assert_eq!(combined.lower, 0);
    assert!(combined.upper > 0);
    assert!(!combined.exhaustive);
    assert!(combined.sample_rows <= 2);
}

#[test]
fn low_degree_large_domain_obtains_useful_samples_with_the_consumers_work_grant() {
    let f = fixture();
    let mut layer = LayerBuilder::default();
    for _ in 0..48 {
        let node = allocate_node_id(&f.connection).unwrap();
        layer.add_node(node).unwrap();
        layer.add_label(node, f.labels[0]).unwrap();
        layer
            .add_relationship(RelationshipRecord {
                id: allocate_relationship_id(&f.connection).unwrap(),
                source: node,
                target: f.nodes[1],
                type_id: f.types[0],
            })
            .unwrap();
    }
    let snapshot = Snapshot::resolve_with_layer_and_schema(
        &f.connection,
        f.commit,
        &layer,
        SchemaState::load(&f.connection, f.commit).unwrap(),
    )
    .unwrap();
    for direction in [Some(false), None] {
        let sample = snapshot
            .bounded_degree_statistics(
                (Some(f.labels[0]), 52),
                Some(f.types[0]),
                direction,
                16,
                512,
                16384,
                &|| Ok(()),
            )
            .unwrap()
            .unwrap();
        assert!(sample.sample_rows >= 8);
        assert!(sample.sample_rows <= 16);
        assert_eq!(sample.truncated_nodes, 0);
        assert!(!sample.exhaustive);
        assert_eq!(sample.upper, None);
        assert!(sample.work <= 512);
    }
}
