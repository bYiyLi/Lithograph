use super::*;
use crate::storage::{
    CommitMetadata, LayerBuilder, RelationshipRecord, allocate_node_id, allocate_relationship_id,
    branch_head, commit_layer, create_checkpoint, create_storage_schema, delete_checkpoint,
    initialize_root, intern_label, intern_relationship_type,
};
use rusqlite::Connection;

fn storage() -> Connection {
    let connection = Connection::open_in_memory().expect("SQLite");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(
                id INTEGER PRIMARY KEY CHECK(id=1),
                magic TEXT NOT NULL,
                database_id TEXT NOT NULL,
                storage_format INTEGER NOT NULL
            )",
        )
        .expect("metadata");
    create_storage_schema(&connection).expect("schema");
    initialize_root(&connection).expect("root");
    connection
}

fn metadata(at: i64) -> CommitMetadata {
    CommitMetadata {
        author: None,
        message: None,
        committed_at: at,
    }
}

fn assert_counts(
    snapshot: &Snapshot<'_>,
    labels: &[LabelId; 2],
    types: &[RelationshipTypeId; 2],
    expected: [u64; 6],
) {
    let stats = snapshot
        .statistics(
            &BTreeSet::from(*labels),
            &BTreeSet::from(*types),
            true,
            true,
        )
        .expect("statistics")
        .expect("valid statistics");
    assert_eq!(stats.node_count, expected[0]);
    assert_eq!(stats.relationship_count, expected[1]);
    for (index, label) in labels.iter().enumerate() {
        assert_eq!(
            stats.label_counts.get(label).copied().unwrap_or(0),
            expected[2 + index]
        );
    }
    for (index, kind) in types.iter().enumerate() {
        assert_eq!(
            stats.type_counts.get(kind).copied().unwrap_or(0),
            expected[4 + index]
        );
    }
}

#[test]
fn checkpoint_and_overlay_statistics_handle_add_delete_and_type_changes() {
    let connection = storage();
    let (n1, n2, n3) = (
        allocate_node_id(&connection).unwrap(),
        allocate_node_id(&connection).unwrap(),
        allocate_node_id(&connection).unwrap(),
    );
    let labels = [
        intern_label(&connection, "StatA").unwrap(),
        intern_label(&connection, "StatB").unwrap(),
    ];
    let types = [
        intern_relationship_type(&connection, "REL_A").unwrap(),
        intern_relationship_type(&connection, "REL_B").unwrap(),
    ];
    let r1 = RelationshipRecord {
        id: allocate_relationship_id(&connection).unwrap(),
        source: n1,
        target: n2,
        type_id: types[0],
    };
    let mut initial = LayerBuilder::default();
    for node in [n1, n2] {
        initial.add_node(node).unwrap();
        initial.add_label(node, labels[0]).unwrap();
    }
    initial.add_relationship(r1).unwrap();
    let root = branch_head(&connection, "main").unwrap();
    let first = commit_layer(&connection, "main", root, None, &initial, &metadata(1))
        .expect("checkpoint source");
    create_checkpoint(&connection, first).expect("derived checkpoint");
    let snapshot = Snapshot::resolve(&connection, first).expect("first snapshot");
    assert_counts(&snapshot, &labels, &types, [2, 1, 2, 0, 1, 0]);
    drop(snapshot);

    let r2 = RelationshipRecord {
        id: allocate_relationship_id(&connection).unwrap(),
        source: n2,
        target: n3,
        type_id: types[1],
    };
    let mut changes = LayerBuilder::default();
    changes.remove_relationship(r1).unwrap();
    changes.remove_label(n1, labels[0]).unwrap();
    changes.remove_node(n1).unwrap();
    changes.add_node(n3).unwrap();
    changes.add_label(n3, labels[1]).unwrap();
    changes.add_relationship(r2).unwrap();
    let second = commit_layer(&connection, "main", first, None, &changes, &metadata(2))
        .expect("overlay commit");
    let snapshot = Snapshot::resolve(&connection, second).expect("overlay snapshot");
    assert_counts(&snapshot, &labels, &types, [2, 1, 1, 1, 0, 1]);
    let only_labels = snapshot
        .statistics(
            &BTreeSet::from(labels),
            &BTreeSet::from(types),
            false,
            false,
        )
        .expect("limited statistics")
        .expect("checkpoint statistics");
    assert_eq!(only_labels.label_counts[&labels[0]], 1);
    assert_eq!(only_labels.type_counts[&types[1]], 1);
    drop(snapshot);

    // Losing derived metadata must not affect canonical graph reads.
    connection
        .execute(
            "UPDATE main._lithograph_checkpoints SET metadata = NULL WHERE commit_id=?1",
            [first.as_bytes().as_slice()],
        )
        .unwrap();
    let snapshot = Snapshot::resolve(&connection, second).expect("metadata-free snapshot");
    assert!(
        snapshot
            .statistics(&BTreeSet::from(labels), &BTreeSet::from(types), true, true)
            .unwrap()
            .is_none()
    );
    drop(snapshot);

    delete_checkpoint(&connection, first).expect("derived checkpoint deleted");
    let reconstructed = Snapshot::resolve(&connection, second).expect("history replay");
    assert_counts(&reconstructed, &labels, &types, [2, 1, 1, 1, 0, 1]);
}

#[test]
fn count_adjustment_is_saturating_and_scoped_to_requested_types() {
    let mut count = 0;
    adjust_count(&mut count, true, false);
    assert_eq!(count, 0);
    adjust_count(&mut count, false, true);
    adjust_count(&mut count, false, false);
    adjust_count(&mut count, true, true);
    assert_eq!(count, 1);
    adjust_count(&mut count, true, false);
    assert_eq!(count, 0);

    let mut counts = std::collections::BTreeMap::new();
    let requested = BTreeSet::from([7]);
    adjust_type_count(&mut counts, &requested, Some(8), true);
    adjust_type_count(&mut counts, &requested, None, false);
    assert!(counts.is_empty());
    adjust_type_count(&mut counts, &requested, Some(7), true);
    adjust_type_count(&mut counts, &requested, Some(7), false);
    assert_eq!(counts.get(&7), Some(&0));
}
