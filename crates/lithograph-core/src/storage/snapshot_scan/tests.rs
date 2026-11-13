use std::sync::{Arc, atomic::AtomicU64, atomic::Ordering};

use super::*;
use crate::storage::{LayerBuilder, create_checkpoint, delete_checkpoint};

mod fixture;

use fixture::Fixture;

fn seek(
    snapshot: &Snapshot<'_>,
    fixture: &Fixture,
    kind: AdjacencyKind,
    minimum: NodeId,
    cursor: Option<AdjacencyCursor>,
    limit: usize,
) -> AdjacencyScanPage {
    match kind {
        AdjacencyKind::Outgoing => snapshot.scan_outgoing_from_neighbor_page(
            fixture.nodes[0],
            fixture.types[0],
            minimum,
            cursor,
            limit,
        ),
        AdjacencyKind::Incoming => snapshot.scan_incoming_from_neighbor_page(
            fixture.nodes[0],
            fixture.types[0],
            minimum,
            cursor,
            limit,
        ),
        AdjacencyKind::Incident => unreachable!("seek requires a directed stream"),
    }
    .expect("typed neighbor seek")
}

fn drain(
    snapshot: &Snapshot<'_>,
    fixture: &Fixture,
    kind: AdjacencyKind,
    minimum: NodeId,
    limit: usize,
) -> Vec<RelationshipRecord> {
    let mut cursor = None;
    let mut records = Vec::new();
    for _ in 0..64 {
        let page = seek(snapshot, fixture, kind, minimum, cursor, limit);
        records.extend(page.items);
        let Some(next) = page.next_cursor else {
            return records;
        };
        assert_ne!(cursor, Some(next), "every checked page must advance");
        cursor = Some(next);
    }
    panic!("small adjacency did not terminate");
}

#[test]
fn typed_neighbor_seek_keeps_parallel_edges_with_and_without_checkpoint() {
    for checkpoint in [false, true] {
        let fixture = Fixture::new(checkpoint);
        let snapshot = Snapshot::resolve(&fixture.db, fixture.head).expect("snapshot");
        for kind in [AdjacencyKind::Outgoing, AdjacencyKind::Incoming] {
            for minimum in [i64::MIN, 1, 2, 3, 4, 5, i64::MAX] {
                let expected = fixture.expected(kind, minimum, &fixture.edges);
                for limit in [1, 2, 4_096] {
                    assert_eq!(drain(&snapshot, &fixture, kind, minimum, limit), expected);
                }
            }
        }
    }
}

#[test]
fn typed_neighbor_seek_advances_existing_cursor_without_rewinding_parallel_edges() {
    for checkpoint in [false, true] {
        let fixture = Fixture::new(checkpoint);
        let snapshot = Snapshot::resolve(&fixture.db, fixture.head).expect("snapshot");
        for kind in [AdjacencyKind::Outgoing, AdjacencyKind::Incoming] {
            let expected = fixture.expected(kind, 0, &fixture.edges);
            let first = snapshot
                .scan_directed_adjacency_page(
                    kind,
                    fixture.nodes[0],
                    Some(fixture.types[0]),
                    None,
                    1,
                    None,
                )
                .expect("ordinary first page");
            assert_eq!(first.items, expected[..1]);
            let target = adjacency_key(kind, expected[2]).neighbor_id;
            let second = seek(&snapshot, &fixture, kind, target, first.next_cursor, 1);
            assert_eq!(second.items, expected[2..3]);
            let third = seek(&snapshot, &fixture, kind, 0, second.next_cursor, 1);
            assert_eq!(third.items, expected[3..4]);
            let end = seek(&snapshot, &fixture, kind, target, third.next_cursor, 1);
            assert!(end.items.is_empty());
            assert!(end.next_cursor.is_none());
        }
    }
}

#[test]
fn typed_neighbor_seek_merges_committed_and_staged_tombstones() {
    let fixture = Fixture::new(true);
    let inserted = fixture.edge(fixture.nodes[0], fixture.nodes[2]);
    let mut committed = LayerBuilder::default();
    committed
        .remove_relationship(fixture.edges[1])
        .expect("remove");
    committed.add_relationship(inserted).expect("add");
    let child = fixture.commit(&committed);
    let snapshot = Snapshot::resolve(&fixture.db, child).expect("child snapshot");
    let mut visible = fixture.edges.clone();
    visible.retain(|record| record.id != fixture.edges[1].id);
    visible.push(inserted);
    let added_out = fixture.edge(fixture.nodes[0], fixture.nodes[4]);
    let added_in = fixture.edge(fixture.nodes[2], fixture.nodes[0]);
    let mut layer = LayerBuilder::default();
    for index in [0, 2, 5, 7] {
        layer
            .remove_relationship(fixture.edges[index])
            .expect("tombstone");
    }
    for record in [added_out, added_in] {
        layer.add_relationship(record).expect("staged add");
    }
    let staged = Snapshot::resolve_with_layer_and_schema(
        &fixture.db,
        child,
        &layer,
        snapshot.schema_state().expect("schema"),
    )
    .expect("staged snapshot");
    let mut staged_visible = visible.clone();
    staged_visible.retain(|record| {
        ![0, 2, 5, 7]
            .iter()
            .any(|i| fixture.edges[*i].id == record.id)
    });
    staged_visible.extend([added_out, added_in]);
    for kind in [AdjacencyKind::Outgoing, AdjacencyKind::Incoming] {
        for minimum in [0, fixture.nodes[2], fixture.nodes[3], fixture.nodes[4]] {
            assert_eq!(
                drain(&snapshot, &fixture, kind, minimum, 1),
                fixture.expected(kind, minimum, &visible)
            );
            assert_eq!(
                drain(&staged, &fixture, kind, minimum, 1),
                fixture.expected(kind, minimum, &staged_visible)
            );
        }
    }
    let tombstones = seek(
        &staged,
        &fixture,
        AdjacencyKind::Outgoing,
        fixture.nodes[3],
        None,
        1,
    );
    assert!(tombstones.items.is_empty());
    assert!(
        tombstones.next_cursor.is_some(),
        "checked tombstones advance"
    );
    let historical = Snapshot::resolve(&fixture.db, fixture.head).expect("historical snapshot");
    assert_eq!(
        drain(&historical, &fixture, AdjacencyKind::Outgoing, 0, 1),
        fixture.expected(AdjacencyKind::Outgoing, 0, &fixture.edges)
    );
}

#[test]
fn typed_neighbor_seek_rejects_foreign_domains_and_staged_revisions() {
    let fixture = Fixture::new(true);
    let snapshot = Snapshot::resolve(&fixture.db, fixture.head).expect("snapshot");
    let cursor = seek(&snapshot, &fixture, AdjacencyKind::Outgoing, 0, None, 1).next_cursor;
    assert!(cursor.is_some());
    for result in [
        snapshot.scan_outgoing_from_neighbor_page(fixture.nodes[1], fixture.types[0], 0, cursor, 1),
        snapshot.scan_outgoing_from_neighbor_page(fixture.nodes[0], fixture.types[1], 0, cursor, 1),
        snapshot.scan_incoming_from_neighbor_page(fixture.nodes[0], fixture.types[0], 0, cursor, 1),
    ] {
        assert!(result.is_err(), "a seek must not rebind a foreign cursor");
    }
    let untyped = snapshot
        .scan_outgoing_page(fixture.nodes[0], None, None, 1)
        .expect("untyped")
        .next_cursor;
    assert!(
        snapshot
            .scan_outgoing_from_neighbor_page(fixture.nodes[0], fixture.types[0], 0, untyped, 1)
            .is_err()
    );
    let mut layer = LayerBuilder::default();
    layer
        .add_relationship(fixture.edge(fixture.nodes[0], fixture.nodes[2]))
        .expect("new edge");
    let schema = snapshot.schema_state().expect("schema");
    let staged =
        Snapshot::resolve_with_layer_and_schema(&fixture.db, fixture.head, &layer, schema.clone())
            .expect("staged");
    assert!(
        staged
            .scan_outgoing_from_neighbor_page(fixture.nodes[0], fixture.types[0], 0, cursor, 1)
            .is_err()
    );
    let staged_cursor = seek(&staged, &fixture, AdjacencyKind::Outgoing, 0, None, 1).next_cursor;
    layer
        .add_relationship(fixture.edge(fixture.nodes[0], fixture.nodes[4]))
        .expect("next revision");
    let revised =
        Snapshot::resolve_with_layer_and_schema(&fixture.db, fixture.head, &layer, schema)
            .expect("revised staged");
    assert!(
        revised
            .scan_outgoing_from_neighbor_page(
                fixture.nodes[0],
                fixture.types[0],
                0,
                staged_cursor,
                1
            )
            .is_err()
    );
    let child = fixture.commit(&layer);
    let committed = Snapshot::resolve(&fixture.db, child).expect("committed");
    assert!(
        committed
            .scan_outgoing_from_neighbor_page(fixture.nodes[0], fixture.types[0], 0, cursor, 1)
            .is_err()
    );
}

#[test]
fn typed_neighbor_seek_rebuilds_from_history_after_checkpoint_deletion() {
    let fixture = Fixture::new(true);
    for checkpoint in [false, true] {
        if checkpoint {
            create_checkpoint(&fixture.db, fixture.head).expect("rebuild");
        } else {
            delete_checkpoint(&fixture.db, fixture.head).expect("delete checkpoint");
        }
        let snapshot = Snapshot::resolve(&fixture.db, fixture.head).expect("snapshot");
        for kind in [AdjacencyKind::Outgoing, AdjacencyKind::Incoming] {
            assert_eq!(
                drain(&snapshot, &fixture, kind, fixture.nodes[1], 1),
                fixture.expected(kind, fixture.nodes[1], &fixture.edges)
            );
        }
    }
}

#[test]
fn neighbor_seek_bounds_preserve_untyped_and_incident_paging() {
    for checkpoint in [false, true] {
        let fixture = Fixture::new(checkpoint);
        let snapshot = Snapshot::resolve(&fixture.db, fixture.head).expect("snapshot");
        for kind in [
            AdjacencyKind::Outgoing,
            AdjacencyKind::Incoming,
            AdjacencyKind::Incident,
        ] {
            let mut cursor = None;
            let mut records = Vec::new();
            loop {
                let page = match kind {
                    AdjacencyKind::Outgoing => {
                        snapshot.scan_outgoing_page(fixture.nodes[0], None, cursor, 1)
                    }
                    AdjacencyKind::Incoming => {
                        snapshot.scan_incoming_page(fixture.nodes[0], None, cursor, 1)
                    }
                    AdjacencyKind::Incident => {
                        snapshot.scan_incident_page(fixture.nodes[0], None, cursor, 1)
                    }
                }
                .expect("untyped page");
                records.extend(page.items);
                let Some(next) = page.next_cursor else {
                    break;
                };
                assert_ne!(cursor, Some(next), "untyped pages must advance");
                cursor = Some(next);
            }
            let mut expected: Vec<_> = fixture
                .edges
                .iter()
                .copied()
                .filter(|record| adjacency_matches(kind, fixture.nodes[0], None, *record))
                .collect();
            records.sort();
            expected.sort();
            assert_eq!(records, expected, "parallel edges and self-loops survive");
        }
    }
}

#[test]
fn typed_neighbor_seek_skips_checkpoint_prefix_in_real_sqlite_work() {
    let fixture = Fixture::new(false);
    let mut layer = LayerBuilder::default();
    for _ in 0..4_096 {
        layer
            .add_relationship(fixture.edge(fixture.nodes[0], fixture.nodes[1]))
            .expect("prefix edge");
    }
    let child = fixture.commit(&layer);
    create_checkpoint(&fixture.db, child).expect("checkpoint");
    let snapshot = Snapshot::resolve(&fixture.db, child).expect("snapshot");
    let work = Arc::new(AtomicU64::new(0));
    let counter = work.clone();
    fixture
        .db
        .progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .expect("install SQLite progress observer");
    let page = seek(
        &snapshot,
        &fixture,
        AdjacencyKind::Outgoing,
        fixture.nodes[3],
        None,
        2,
    );
    let seek_work = work.swap(0, Ordering::Relaxed);
    assert_eq!(
        page.items,
        fixture.expected(AdjacencyKind::Outgoing, fixture.nodes[3], &fixture.edges)
    );
    let mut cursor = None;
    let mut scanned = Vec::new();
    loop {
        let page = snapshot
            .scan_outgoing_page(fixture.nodes[0], Some(fixture.types[0]), cursor, 256)
            .expect("sequential page");
        scanned.extend(
            page.items
                .into_iter()
                .filter(|record| record.target >= fixture.nodes[3]),
        );
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    let scan_work = work.load(Ordering::Relaxed);
    fixture
        .db
        .progress_handler(0, None::<fn() -> bool>)
        .expect("remove SQLite progress observer");
    assert_eq!(scanned, page.items);
    assert!(seek_work > 0, "actual SQLite execution must be observed");
    assert!(seek_work < 1_000, "seek work {seek_work}");
    assert!(
        scan_work > seek_work * 10,
        "seek {seek_work}, scan {scan_work}"
    );
}
