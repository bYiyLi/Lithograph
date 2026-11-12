use std::cell::Cell;

use rusqlite::Connection;

use super::*;
use crate::storage::{
    CommitMetadata, LayerBuilder, OwnerKind, PropertyValue, RelationshipRecord, Snapshot,
    allocate_node_id, allocate_relationship_id, branch_head, commit_layer, create_storage_schema,
    initialize_root, integrity_check, intern_label, intern_property_key, intern_relationship_type,
};

fn storage() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta( \
                id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, \
                database_id TEXT NOT NULL, storage_format INTEGER NOT NULL)",
        )
        .unwrap();
    create_storage_schema(&connection).unwrap();
    initialize_root(&connection).unwrap();
    connection
}

fn commit(connection: &Connection, previous: HashId, layer: &LayerBuilder, at: i64) -> HashId {
    commit_layer(
        connection,
        "main",
        previous,
        None,
        layer,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: at,
        },
    )
    .unwrap()
}

#[test]
fn tiered_checkpoint_reuses_nearest_complete_ancestor_and_preserves_full_graph() {
    let connection = storage();
    let a = allocate_node_id(&connection).unwrap();
    let b = allocate_node_id(&connection).unwrap();
    let c = allocate_node_id(&connection).unwrap();
    let label = intern_label(&connection, "Tier").unwrap();
    let key = intern_property_key(&connection, "payload").unwrap();
    let kind = intern_relationship_type(&connection, "REACHES").unwrap();
    let relationship = RelationshipRecord {
        id: allocate_relationship_id(&connection).unwrap(),
        source: a,
        type_id: kind,
        target: b,
    };
    let mut initial = LayerBuilder::default();
    for node in [a, b] {
        initial.add_node(node).unwrap();
        initial.add_label(node, label).unwrap();
    }
    initial.add_relationship(relationship).unwrap();
    initial
        .set_property(OwnerKind::Node, a, key, PropertyValue::String("old".into()))
        .unwrap();
    let root = branch_head(&connection, "main").unwrap();
    let first = commit(&connection, root, &initial, 1);
    create_checkpoint(&connection, first).unwrap();

    let mut add = LayerBuilder::default();
    add.add_node(c).unwrap();
    add.add_label(c, label).unwrap();
    add.set_property(OwnerKind::Node, a, key, PropertyValue::String("new".into()))
        .unwrap();
    let second = commit(&connection, first, &add, 2);
    let mut remove = LayerBuilder::default();
    remove.remove_relationship(relationship).unwrap();
    let third = commit(&connection, second, &remove, 3);
    let (base, layers) = first_parent_layers(&connection, third, &|| Ok(())).unwrap();
    assert_eq!(base, Some(first));
    assert_eq!(layers.len(), 2);
    create_checkpoint(&connection, third).unwrap();
    let fourth = commit(&connection, third, &LayerBuilder::default(), 4);
    let (base, layers) = first_parent_layers(&connection, fourth, &|| Ok(())).unwrap();
    assert_eq!(base, Some(third));
    assert_eq!(layers.len(), 1);
    create_checkpoint(&connection, fourth).unwrap();
    assert!(integrity_check(&connection).unwrap().is_empty());
    let snapshot = Snapshot::resolve(&connection, fourth).unwrap();
    let stats = snapshot
        .statistics(
            &BTreeSet::from([label]),
            &BTreeSet::from([kind]),
            true,
            true,
        )
        .unwrap()
        .unwrap();
    assert_eq!(stats.node_count, 3);
    assert_eq!(stats.relationship_count, 0);
    assert_eq!(stats.label_counts.get(&label), Some(&3));
    drop(snapshot);

    delete_checkpoint(&connection, fourth).unwrap();
    delete_checkpoint(&connection, third).unwrap();
    let fallback = Snapshot::resolve(&connection, fourth).unwrap();
    let stats = fallback
        .statistics(
            &BTreeSet::from([label]),
            &BTreeSet::from([kind]),
            true,
            true,
        )
        .unwrap()
        .unwrap();
    assert_eq!(stats.node_count, 3);
    assert_eq!(stats.relationship_count, 0);
}

#[test]
fn incompatible_checkpoint_metadata_falls_back_to_canonical_history() {
    let connection = storage();
    let root = branch_head(&connection, "main").unwrap();
    let first = commit(&connection, root, &LayerBuilder::default(), 1);
    create_checkpoint(&connection, first).unwrap();
    let second = commit(&connection, first, &LayerBuilder::default(), 2);
    connection
        .execute(
            "UPDATE main._lithograph_checkpoints SET metadata=NULL WHERE commit_id=?1",
            [first.as_bytes().as_slice()],
        )
        .unwrap();
    let (base, layers) = first_parent_layers(&connection, second, &|| Ok(())).unwrap();
    assert!(base.is_none());
    assert_eq!(layers.len(), 3);
    create_checkpoint(&connection, second).unwrap();
    assert!(usable_checkpoint(&connection, second).unwrap());
    assert!(integrity_check(&connection).unwrap().is_empty());
}

#[test]
fn checkpoint_metadata_type_corruption_is_unknown_and_preserves_canonical_reads() {
    use rusqlite::types::Value;

    let connection = storage();
    let node = allocate_node_id(&connection).unwrap();
    let key = intern_property_key(&connection, "payload").unwrap();
    let mut layer = LayerBuilder::default();
    layer.add_node(node).unwrap();
    layer
        .set_property(OwnerKind::Node, node, key, PropertyValue::Integer(7))
        .unwrap();
    let root = branch_head(&connection, "main").unwrap();
    let target = commit(&connection, root, &layer, 1);
    create_checkpoint(&connection, target).unwrap();
    let valid = encode_statistics(
        &load_checkpoint_statistics(&connection, target)
            .unwrap()
            .unwrap(),
    );
    for metadata in [
        Value::Null,
        Value::Integer(7),
        Value::Real(1.25),
        Value::Text("broken".into()),
        Value::Text(String::from_utf8(valid.clone()).unwrap()),
        Value::Text("x".repeat(16 * 1024 * 1024)),
        Value::Blob(b"broken".to_vec()),
    ] {
        connection
            .execute(
                "UPDATE main._lithograph_checkpoints SET metadata=?2 WHERE commit_id=?1",
                params![target.as_bytes().as_slice(), metadata],
            )
            .unwrap();
        let writes = connection.total_changes();
        assert!(
            load_checkpoint_statistics(&connection, target)
                .unwrap()
                .is_none()
        );
        assert!(!usable_checkpoint(&connection, target).unwrap());
        let (base, layers) = first_parent_layers(&connection, target, &|| Ok(())).unwrap();
        assert_eq!(base, None);
        assert_eq!(layers.len(), 2);
        let snapshot = Snapshot::resolve(&connection, target).unwrap();
        assert!(snapshot.node_exists(node).unwrap());
        assert_eq!(
            snapshot.property(OwnerKind::Node, node, key).unwrap(),
            Some(PropertyValue::Integer(7))
        );
        assert_eq!(branch_head(&connection, "main").unwrap(), target);
        assert_eq!(connection.total_changes(), writes);
    }
    connection
        .execute(
            "UPDATE main._lithograph_checkpoints SET metadata=?2 WHERE commit_id=?1",
            params![target.as_bytes().as_slice(), valid],
        )
        .unwrap();
    assert!(usable_checkpoint(&connection, target).unwrap());
    assert!(integrity_check(&connection).unwrap().is_empty());
}

#[test]
fn checkpoint_metadata_type_guard_has_fixed_sql_work_for_large_text() {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    let connection = storage();
    let root = branch_head(&connection, "main").unwrap();
    create_checkpoint(&connection, root).unwrap();
    let mut work = Vec::new();
    for bytes in [32, 16 * 1024 * 1024] {
        connection
            .execute(
                "UPDATE main._lithograph_checkpoints SET metadata=?2 WHERE commit_id=?1",
                params![root.as_bytes().as_slice(), "x".repeat(bytes)],
            )
            .unwrap();
        let steps = Arc::new(AtomicU64::new(0));
        let observed = steps.clone();
        connection
            .progress_handler(
                1,
                Some(move || observed.fetch_add(1, Ordering::Relaxed) >= 128),
            )
            .unwrap();
        let statistics = load_checkpoint_statistics(&connection, root);
        let usable = usable_checkpoint(&connection, root);
        let lineage = first_parent_layers(&connection, root, &|| Ok(()));
        connection
            .progress_handler(0, None::<fn() -> bool>)
            .unwrap();
        assert!(statistics.unwrap().is_none());
        assert!(!usable.unwrap());
        assert_eq!(lineage.unwrap().0, None);
        work.push(steps.load(Ordering::Relaxed));
    }
    assert!(
        work[0] < 128,
        "metadata probes exceeded their SQL work budget: {work:?}"
    );
    assert_eq!(
        work[0], work[1],
        "TEXT size changed the metadata probe work"
    );
}

#[test]
fn checkpoint_metadata_unknown_fallback_preserves_sql_errors_and_interrupts() {
    let connection = storage();
    let root = branch_head(&connection, "main").unwrap();
    create_checkpoint(&connection, root).unwrap();
    connection.progress_handler(1, Some(|| true)).unwrap();
    let interrupted = load_checkpoint_statistics(&connection, root);
    connection
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    assert!(matches!(
        interrupted,
        Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _)))
            if code.extended_code == rusqlite::ffi::SQLITE_INTERRUPT
    ));
    connection
        .execute_batch("DROP TABLE main._lithograph_checkpoints")
        .unwrap();
    assert!(matches!(
        load_checkpoint_statistics(&connection, root),
        Err(StorageError::Sqlite(_))
    ));
}

#[test]
fn corrupt_checkpoint_is_ignored_by_readers_without_writing_main() {
    let connection = storage();
    let node = allocate_node_id(&connection).unwrap();
    let mut layer = LayerBuilder::default();
    layer.add_node(node).unwrap();
    let root = branch_head(&connection, "main").unwrap();
    let first = commit(&connection, root, &layer, 1);
    create_checkpoint(&connection, first).unwrap();
    let second = commit(&connection, first, &LayerBuilder::default(), 2);

    // Simulate damage confined to derived state, not to canonical history.
    connection
        .execute(
            "DELETE FROM main._lithograph_cp_nodes WHERE commit_id=?1",
            [first.as_bytes().as_slice()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE main._lithograph_checkpoints SET metadata=?2 WHERE commit_id=?1",
            rusqlite::params![first.as_bytes().as_slice(), b"invalid metadata"],
        )
        .unwrap();
    let before = connection.total_changes();
    assert!(
        Snapshot::resolve(&connection, first)
            .unwrap()
            .node_exists(node)
            .unwrap()
    );
    assert!(
        Snapshot::resolve(&connection, second)
            .unwrap()
            .node_exists(node)
            .unwrap()
    );
    assert_eq!(
        connection.total_changes(),
        before,
        "read repaired main implicitly"
    );
    assert_eq!(branch_head(&connection, "main").unwrap(), second);
    create_checkpoint(&connection, second).unwrap();
    assert!(
        Snapshot::resolve(&connection, second)
            .unwrap()
            .node_exists(node)
            .unwrap()
    );
}

#[test]
fn authorized_commit_automatically_publishes_a_tiered_checkpoint() {
    let connection = storage();
    let node = allocate_node_id(&connection).unwrap();
    let mut initial = LayerBuilder::default();
    initial.add_node(node).unwrap();
    let mut head = commit(
        &connection,
        branch_head(&connection, "main").unwrap(),
        &initial,
        1,
    );
    let mut previous = head;
    for index in 2..=640 {
        previous = head;
        head = commit(&connection, head, &LayerBuilder::default(), index);
    }
    assert_eq!(branch_head(&connection, "main").unwrap(), head);
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_checkpoints",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(count > 0, "auto policy never ran at its depth threshold");
    assert!(count <= 4, "automatic maintenance ran too often");
    let (base, replay_layers) = first_parent_layers(&connection, head, &|| Ok(())).unwrap();
    assert!(base.is_some(), "no complete ancestor checkpoint");
    assert!(replay_layers.len() < 640);
    assert!(
        Snapshot::resolve(&connection, head)
            .unwrap()
            .node_exists(node)
            .unwrap()
    );
    assert!(
        Snapshot::resolve(&connection, previous)
            .unwrap()
            .node_exists(node)
            .unwrap()
    );
    assert!(integrity_check(&connection).unwrap().is_empty());
}

#[test]
fn interrupted_copy_rolls_back_partial_derived_rows_without_changing_branch_head() {
    let connection = storage();
    let root = branch_head(&connection, "main").unwrap();
    let node = allocate_node_id(&connection).unwrap();
    let mut initial = LayerBuilder::default();
    initial.add_node(node).unwrap();
    let first = commit(&connection, root, &initial, 1);
    create_checkpoint(&connection, first).unwrap();
    let second = commit(&connection, first, &LayerBuilder::default(), 2);
    let interrupted_during_copy = Cell::new(false);
    let error = create_checkpoint_checked(&connection, second, &|| {
        let staged: i64 = connection.query_row(
            "SELECT count(*) FROM main._lithograph_cp_nodes WHERE commit_id=?1",
            [second.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        if staged > 0 {
            interrupted_during_copy.set(true);
            return Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                None,
            )));
        }
        Ok(())
    })
    .unwrap_err();
    assert!(interrupted_during_copy.get());
    assert!(matches!(error, StorageError::Sqlite(_)));
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_checkpoints WHERE commit_id=?1)",
            [second.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!exists);
    assert_eq!(branch_head(&connection, "main").unwrap(), second);
    create_checkpoint(&connection, second).unwrap();
    assert!(integrity_check(&connection).unwrap().is_empty());
}

#[test]
fn interrupt_during_automatic_checkpoint_rolls_back_canonical_commit() {
    use crate::storage::commit::commit_layer_checked;

    let connection = storage();
    let node = allocate_node_id(&connection).unwrap();
    let mut initial = LayerBuilder::default();
    initial.add_node(node).unwrap();
    let mut head = commit(
        &connection,
        branch_head(&connection, "main").unwrap(),
        &initial,
        1,
    );
    for timestamp in 2..=2048 {
        let before = head;
        let intercepted = Cell::new(false);
        let check = || {
            let in_progress: i64 = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM main._lithograph_checkpoints AS cp \
                 WHERE cp.metadata IS NULL AND EXISTS( \
                   SELECT 1 FROM main._lithograph_cp_nodes AS n \
                   WHERE n.commit_id=cp.commit_id))",
                [],
                |row| row.get(0),
            )?;
            if in_progress == 1 {
                intercepted.set(true);
                return Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                    None,
                )));
            }
            Ok(())
        };
        let result = commit_layer_checked(
            &connection,
            "main",
            before,
            None,
            &LayerBuilder::default(),
            &CommitMetadata {
                author: None,
                message: None,
                committed_at: timestamp,
            },
            &check,
        );
        if intercepted.get() {
            assert!(result.is_err(), "checkpoint interruption was swallowed");
            assert_eq!(branch_head(&connection, "main").unwrap(), before);
            let partial: i64 = connection
                .query_row(
                    "SELECT count(*) FROM main._lithograph_checkpoints WHERE metadata IS NULL",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(partial, 0, "incomplete derived checkpoint survived");
            assert!(
                Snapshot::resolve(&connection, before)
                    .unwrap()
                    .node_exists(node)
                    .unwrap()
            );
            assert!(integrity_check(&connection).unwrap().is_empty());
            return;
        }
        head = result.unwrap();
    }
    panic!("automatic checkpoint was never reached by the cancellation probe");
}

#[test]
fn oversized_checkpoint_metadata_never_becomes_a_snapshot_source() {
    let connection = storage();
    let node = allocate_node_id(&connection).unwrap();
    let mut initial = LayerBuilder::default();
    initial.add_node(node).unwrap();
    let root = branch_head(&connection, "main").unwrap();
    let first = commit(&connection, root, &initial, 1);
    create_checkpoint(&connection, first).unwrap();
    let second = commit(&connection, first, &LayerBuilder::default(), 2);
    connection
        .execute(
            "UPDATE main._lithograph_checkpoints SET metadata=zeroblob(262145) WHERE commit_id=?1",
            [first.as_bytes().as_slice()],
        )
        .unwrap();
    let (base, layers) = first_parent_layers(&connection, second, &|| Ok(())).unwrap();
    assert!(base.is_none(), "unbounded metadata was accepted as a base");
    assert_eq!(layers.len(), 3);
    let writes_before = connection.total_changes();
    assert!(
        Snapshot::resolve(&connection, second)
            .unwrap()
            .node_exists(node)
            .unwrap()
    );
    assert_eq!(connection.total_changes(), writes_before);
    create_checkpoint(&connection, second).unwrap();
    assert!(
        Snapshot::resolve(&connection, second)
            .unwrap()
            .node_exists(node)
            .unwrap()
    );
    assert!(integrity_check(&connection).unwrap().is_empty());
}

#[test]
fn excessive_statistics_groups_roll_back_checkpoint_without_changing_history() {
    let connection = storage();
    let node = allocate_node_id(&connection).unwrap();
    let mut layer = LayerBuilder::default();
    layer.add_node(node).unwrap();
    for at in 0..=MAX_STATISTICS_GROUPS {
        layer
            .add_label(
                node,
                intern_label(&connection, &format!("Group{at}")).unwrap(),
            )
            .unwrap();
    }
    let root = branch_head(&connection, "main").unwrap();
    let target = commit(&connection, root, &layer, 1);
    let before = Snapshot::resolve(&connection, target)
        .unwrap()
        .semantic_hash()
        .unwrap();
    let error = create_checkpoint(&connection, target).unwrap_err();
    assert!(
        matches!(error, StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
        if code.extended_code == rusqlite::ffi::SQLITE_TOOBIG)
    );
    assert!(!usable_checkpoint(&connection, target).unwrap());
    let copied: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_cp_labels",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        copied, 0,
        "failed budget left partially published derived rows"
    );
    assert_eq!(branch_head(&connection, "main").unwrap(), target);
    assert_eq!(
        Snapshot::resolve(&connection, target)
            .unwrap()
            .semantic_hash()
            .unwrap(),
        before
    );
    assert!(integrity_check(&connection).unwrap().is_empty());
}

#[test]
fn inconsistent_statistics_metadata_is_not_a_snapshot_base() {
    let connection = storage();
    let node = allocate_node_id(&connection).unwrap();
    let mut layer = LayerBuilder::default();
    layer.add_node(node).unwrap();
    let root = branch_head(&connection, "main").unwrap();
    let target = commit(&connection, root, &layer, 1);
    create_checkpoint(&connection, target).unwrap();
    for (labels, types, relationships) in [
        (json!([[1, 2]]), json!([]), 0),
        (json!([[1, 1], [1, 1]]), json!([]), 0),
        (json!([[0, 1]]), json!([]), 0),
        (json!([]), json!([[1, 1]]), 0),
        (json!([]), json!([]), 1),
    ] {
        let metadata = json!({"version":1,"nodes":1,"relationships":relationships,
            "labels":labels,"types":types})
        .to_string()
        .into_bytes();
        connection
            .execute(
                "UPDATE main._lithograph_checkpoints SET metadata=?2 WHERE commit_id=?1",
                params![target.as_bytes().as_slice(), metadata],
            )
            .unwrap();
        let writes = connection.total_changes();
        assert!(!usable_checkpoint(&connection, target).unwrap());
        assert!(
            Snapshot::resolve(&connection, target)
                .unwrap()
                .node_exists(node)
                .unwrap()
        );
        assert_eq!(connection.total_changes(), writes);
    }
}

fn paged_checkpoint_fixture() -> (
    Connection,
    Vec<i64>,
    Vec<RelationshipRecord>,
    LabelId,
    RelationshipTypeId,
    i64,
    HashId,
) {
    let connection = storage();
    let root = branch_head(&connection, "main").unwrap();
    let label = intern_label(&connection, "Paged").unwrap();
    let kind = intern_relationship_type(&connection, "NEXT").unwrap();
    let property = intern_property_key(&connection, "weight").unwrap();
    let nodes = (0..1500)
        .map(|_| allocate_node_id(&connection).unwrap())
        .collect::<Vec<_>>();
    let mut relationships = Vec::new();
    let mut layer = LayerBuilder::default();
    for (position, &node) in nodes.iter().enumerate() {
        layer.add_node(node).unwrap();
        layer.add_label(node, label).unwrap();
        layer
            .set_property(
                OwnerKind::Node,
                node,
                property,
                PropertyValue::Integer(position as i64),
            )
            .unwrap();
    }
    for (position, pair) in nodes.windows(2).enumerate() {
        let id = allocate_relationship_id(&connection).unwrap();
        let relationship = RelationshipRecord {
            id,
            source: pair[0],
            type_id: kind,
            target: pair[1],
        };
        layer.add_relationship(relationship).unwrap();
        relationships.push(relationship);
        layer
            .set_property(
                OwnerKind::Relationship,
                id,
                property,
                PropertyValue::Integer(position as i64),
            )
            .unwrap();
    }
    let first = commit(&connection, root, &layer, 1);
    create_checkpoint(&connection, first).unwrap();
    let second = commit(&connection, first, &LayerBuilder::default(), 2);
    (
        connection,
        nodes,
        relationships,
        label,
        kind,
        property,
        second,
    )
}

fn assert_paged_ancestor_copy(
    connection: &Connection,
    second: HashId,
    nodes: &[i64],
    label: LabelId,
    kind: RelationshipTypeId,
) {
    let expected = Snapshot::resolve(connection, second)
        .unwrap()
        .semantic_hash()
        .unwrap();
    let interrupted = Cell::new(false);
    let result = create_checkpoint_checked(connection, second, &|| {
        let copied: i64 = connection.query_row(
            "SELECT count(*) FROM main._lithograph_cp_nodes WHERE commit_id=?1",
            [second.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        if copied >= 512 {
            interrupted.set(true);
            return Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                None,
            )));
        }
        Ok(())
    });
    assert!(interrupted.get(), "copy never reached the first page");
    assert!(result.is_err());
    assert!(
        !usable_checkpoint(connection, second).unwrap(),
        "failed copy published a checkpoint"
    );
    assert_eq!(branch_head(connection, "main").unwrap(), second);
    create_checkpoint(connection, second).unwrap();

    for (table, expected) in [
        ("_lithograph_cp_nodes", 1500),
        ("_lithograph_cp_labels", 1500),
        ("_lithograph_cp_relationships", 1499),
        ("_lithograph_cp_properties", 2999),
    ] {
        let actual: i64 = connection
            .query_row(
                &format!("SELECT count(*) FROM main.{table} WHERE commit_id=?1"),
                [second.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actual, expected, "{table} lost rows at a page boundary");
    }
    let snapshot = Snapshot::resolve(connection, second).unwrap();
    assert_eq!(snapshot.semantic_hash().unwrap(), expected);
    assert!(snapshot.node_exists(nodes[0]).unwrap());
    assert!(snapshot.node_exists(*nodes.last().unwrap()).unwrap());
    let stats = snapshot
        .statistics(
            &BTreeSet::from([label]),
            &BTreeSet::from([kind]),
            true,
            true,
        )
        .unwrap()
        .unwrap();
    assert_eq!(stats.node_count, 1500);
    assert_eq!(stats.relationship_count, 1499);
    drop(snapshot);
}

fn paged_replay_changes(
    nodes: &[i64],
    relationships: &[RelationshipRecord],
    label: LabelId,
    property: i64,
) -> LayerBuilder {
    // Exercise the replay path separately from the ancestor copy path.
    // Deleting over 1,024 consecutive keys crosses multiple bounded pages.
    let mut changes = LayerBuilder::default();
    for &node in nodes.iter().take(1100) {
        changes.remove_node(node).unwrap();
        changes.remove_label(node, label).unwrap();
        changes
            .remove_property(OwnerKind::Node, node, property)
            .unwrap();
    }
    for relationship in relationships.iter().take(1100) {
        changes.remove_relationship(*relationship).unwrap();
        changes
            .remove_property(OwnerKind::Relationship, relationship.id, property)
            .unwrap();
    }
    for relationship in relationships.iter().skip(1100) {
        changes
            .set_property(
                OwnerKind::Relationship,
                relationship.id,
                property,
                PropertyValue::Integer(-1),
            )
            .unwrap();
    }
    changes
}

fn assert_paged_delta_replay(
    connection: &Connection,
    second: HashId,
    nodes: &[i64],
    relationships: &[RelationshipRecord],
    label: LabelId,
    kind: RelationshipTypeId,
    property: i64,
) {
    let changes = paged_replay_changes(nodes, relationships, label, property);
    let third = commit(connection, second, &changes, 3);
    let expected = Snapshot::resolve(connection, third)
        .unwrap()
        .semantic_hash()
        .unwrap();
    let interrupted_replay = Cell::new(false);
    let interrupted = create_checkpoint_checked(connection, third, &|| {
        let copied_properties: i64 = connection.query_row(
            "SELECT count(*) FROM main._lithograph_cp_properties WHERE commit_id=?1",
            [third.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        if copied_properties == 2999 {
            let remaining_relationships: i64 = connection.query_row(
                "SELECT count(*) FROM main._lithograph_cp_relationships WHERE commit_id=?1",
                [third.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            if remaining_relationships < 1499 {
                interrupted_replay.set(true);
                return Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                    None,
                )));
            }
        }
        Ok(())
    });
    assert!(
        interrupted_replay.get(),
        "replay was not cancellable between pages"
    );
    assert!(interrupted.is_err());
    assert!(!usable_checkpoint(connection, third).unwrap());
    assert_eq!(branch_head(connection, "main").unwrap(), third);
    create_checkpoint(connection, third).unwrap();
    for (table, expected) in [
        ("_lithograph_cp_nodes", 400),
        ("_lithograph_cp_labels", 400),
        ("_lithograph_cp_relationships", 399),
        ("_lithograph_cp_properties", 799),
    ] {
        let actual: i64 = connection
            .query_row(
                &format!("SELECT count(*) FROM main.{table} WHERE commit_id=?1"),
                [third.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actual, expected, "{table} replay missed a page");
    }
    let replayed = Snapshot::resolve(connection, third).unwrap();
    assert_eq!(replayed.semantic_hash().unwrap(), expected);
    let replayed_stats = replayed
        .statistics(
            &BTreeSet::from([label]),
            &BTreeSet::from([kind]),
            true,
            true,
        )
        .unwrap()
        .unwrap();
    assert_eq!(replayed_stats.node_count, 400);
    assert_eq!(replayed_stats.relationship_count, 399);
    assert_eq!(replayed_stats.label_counts.get(&label), Some(&400));
    drop(replayed);
    delete_checkpoint(connection, third).unwrap();
    let fallback = Snapshot::resolve(connection, third).unwrap();
    assert_eq!(fallback.semantic_hash().unwrap(), expected);
    let fallback_stats = fallback
        .statistics(
            &BTreeSet::from([label]),
            &BTreeSet::from([kind]),
            true,
            true,
        )
        .unwrap()
        .unwrap();
    assert_eq!(replayed_stats, fallback_stats);
    assert!(integrity_check(connection).unwrap().is_empty());
}

#[test]
fn checkpoint_copy_paginates_all_tables_and_rolls_back_mid_copy() {
    let (connection, nodes, relationships, label, kind, property, second) =
        paged_checkpoint_fixture();
    assert_paged_ancestor_copy(&connection, second, &nodes, label, kind);
    assert_paged_delta_replay(
        &connection,
        second,
        &nodes,
        &relationships,
        label,
        kind,
        property,
    );
}

fn automatic_checkpoint_count(connection: &Connection) -> i64 {
    connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_checkpoints \
             WHERE json_extract(CAST(metadata AS TEXT), '$.automatic') = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn automatic_retention_is_bounded_and_preserves_manual_checkpoints_and_history() {
    let connection = storage();
    let node = allocate_node_id(&connection).unwrap();
    let mut initial = LayerBuilder::default();
    initial.add_node(node).unwrap();
    let root = branch_head(&connection, "main").unwrap();
    let manual = commit(&connection, root, &initial, 1);
    create_checkpoint(&connection, manual).unwrap();
    policy::publish_automatic_checkpoint(&connection, manual, None, &|| Ok(())).unwrap();
    assert_eq!(automatic_checkpoint_count(&connection), 0);
    let mut head = manual;
    let mut old = manual;
    for at in 2..10 {
        head = commit(&connection, head, &LayerBuilder::default(), at);
        policy::publish_automatic_checkpoint(&connection, head, None, &|| Ok(())).unwrap();
        if at == 2 {
            old = head;
        }
        assert!(automatic_checkpoint_count(&connection) <= 4);
    }
    assert!(usable_checkpoint(&connection, manual).unwrap());
    assert!(!usable_checkpoint(&connection, old).unwrap());
    assert!(usable_checkpoint(&connection, head).unwrap());
    for target in [manual, old, head] {
        assert!(
            Snapshot::resolve(&connection, target)
                .unwrap()
                .node_exists(node)
                .unwrap()
        );
    }
    assert_eq!(branch_head(&connection, "main").unwrap(), head);
    assert!(integrity_check(&connection).unwrap().is_empty());
}

#[test]
fn automatic_retention_preserves_the_cold_branch_operations_immutable_base() {
    let connection = storage();
    let node = allocate_node_id(&connection).unwrap();
    let key = intern_property_key(&connection, "retained").unwrap();
    let mut layer = LayerBuilder::default();
    layer.add_node(node).unwrap();
    layer
        .set_property(OwnerKind::Node, node, key, PropertyValue::Integer(7))
        .unwrap();
    let root = branch_head(&connection, "main").unwrap();
    let first = commit(&connection, root, &layer, 1);
    policy::publish_automatic_checkpoint(&connection, first, None, &|| Ok(())).unwrap();
    // The age adjustment is confined to this synthetic derived cache.
    connection
        .execute(
            "UPDATE main._lithograph_checkpoints SET created_at=0 WHERE commit_id=?1",
            [first.as_bytes().as_slice()],
        )
        .unwrap();
    super::super::create_branch(&connection, "cold", first).unwrap();
    let pinned = Snapshot::resolve(&connection, first).unwrap();
    let mut main = first;
    for timestamp in 2..=4 {
        main = commit(&connection, main, &LayerBuilder::default(), timestamp);
        policy::publish_automatic_checkpoint(&connection, main, None, &|| Ok(())).unwrap();
    }
    let cold = super::super::commit_layer(
        &connection,
        "cold",
        first,
        None,
        &LayerBuilder::default(),
        &CommitMetadata {
            committed_at: 5,
            ..CommitMetadata::root()
        },
    )
    .unwrap();
    // This uses CommitRead.base=None, just as schema/version callers do.
    policy::publish_automatic_checkpoint(&connection, cold, None, &|| Ok(())).unwrap();
    assert_eq!(automatic_checkpoint_count(&connection), 4);
    assert!(usable_checkpoint(&connection, first).unwrap());
    assert!(usable_checkpoint(&connection, cold).unwrap());
    assert!(pinned.node_exists(node).unwrap());
    assert_eq!(
        pinned.property(OwnerKind::Node, node, key).unwrap(),
        Some(PropertyValue::Integer(7))
    );
    assert_eq!(branch_head(&connection, "main").unwrap(), main);
    assert_eq!(branch_head(&connection, "cold").unwrap(), cold);
    assert!(integrity_check(&connection).unwrap().is_empty());
}

#[test]
fn automatic_eviction_cancellation_rolls_back_publication_and_all_deletions() {
    let connection = storage();
    let mut head = branch_head(&connection, "main").unwrap();
    let mut oldest = head;
    for at in 1..=4 {
        head = commit(&connection, head, &LayerBuilder::default(), at);
        policy::publish_automatic_checkpoint(&connection, head, None, &|| Ok(())).unwrap();
        if at == 1 {
            oldest = head;
        }
    }
    let next = commit(&connection, head, &LayerBuilder::default(), 5);
    let cancelled = Cell::new(false);
    let result = policy::publish_automatic_checkpoint(&connection, next, None, &|| {
        if !usable_checkpoint(&connection, oldest)? {
            cancelled.set(true);
            return Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                None,
            )));
        }
        Ok(())
    });
    assert!(cancelled.get());
    assert!(result.is_err());
    assert!(usable_checkpoint(&connection, oldest).unwrap());
    assert!(!usable_checkpoint(&connection, next).unwrap());
    assert_eq!(automatic_checkpoint_count(&connection), 4);
    assert_eq!(branch_head(&connection, "main").unwrap(), next);
    assert!(integrity_check(&connection).unwrap().is_empty());
}

#[test]
fn pinned_checkpoints_defer_publication_without_exceeding_retention() {
    use crate::query::{ExecutionControl, QueryResources};
    let connection = storage();
    let mut head = branch_head(&connection, "main").unwrap();
    let mut targets = Vec::new();
    for at in 1..=4 {
        head = commit(&connection, head, &LayerBuilder::default(), at);
        policy::publish_automatic_checkpoint(&connection, head, None, &|| Ok(())).unwrap();
        targets.push(head);
    }
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let pinned = targets
        .iter()
        .map(|target| {
            crate::query::resolve_snapshot_with_control(&connection, *target, &control).unwrap()
        })
        .collect::<Vec<_>>();
    let identity = snapshot_pin_identity(&connection).unwrap();
    let next = commit(&connection, head, &LayerBuilder::default(), 5);
    let observer = crate::query::snapshot_observer(&resources);
    let error = policy::publish_automatic_checkpoint(
        &connection,
        next,
        Some(observer.as_ref()),
        &|| Ok(()),
    )
    .unwrap_err();
    assert!(
        matches!(error, StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
        if code.extended_code == rusqlite::ffi::SQLITE_TOOBIG)
    );
    assert_eq!(automatic_checkpoint_count(&connection), 4);
    assert!(!usable_checkpoint(&connection, next).unwrap());
    assert_eq!(branch_head(&connection, "main").unwrap(), next);
    for target in &targets {
        assert!(usable_checkpoint(&connection, *target).unwrap());
        assert!(resources.checkpoint_is_pinned(identity, *target).unwrap());
    }
    let shared = pinned[0].resolved_state();
    drop(pinned);
    assert!(
        resources
            .checkpoint_is_pinned(identity, targets[0])
            .unwrap()
    );
    drop(shared);
    assert_eq!(resources.used(), 0);
    policy::publish_automatic_checkpoint(&connection, next, Some(observer.as_ref()), &|| Ok(()))
        .unwrap();
    assert_eq!(automatic_checkpoint_count(&connection), 4);
    assert!(usable_checkpoint(&connection, next).unwrap());
}

#[test]
fn tiered_checkpoint_never_adopts_a_newer_second_parent_or_sibling_base() {
    let connection = storage();
    let root = branch_head(&connection, "main").unwrap();
    let shared = allocate_node_id(&connection).unwrap();
    let mut initial = LayerBuilder::default();
    initial.add_node(shared).unwrap();
    let first = commit(&connection, root, &initial, 1);
    create_checkpoint(&connection, first).unwrap();
    crate::storage::create_branch(&connection, "side", first).unwrap();
    let side_node = allocate_node_id(&connection).unwrap();
    let mut side_layer = LayerBuilder::default();
    side_layer.add_node(side_node).unwrap();
    let metadata = CommitMetadata {
        author: None,
        message: None,
        committed_at: 2,
    };
    let side = commit_layer(&connection, "side", first, None, &side_layer, &metadata).unwrap();
    create_checkpoint(&connection, side).unwrap();
    let main_node = allocate_node_id(&connection).unwrap();
    let mut main_layer = LayerBuilder::default();
    main_layer.add_node(main_node).unwrap();
    let merged = commit_layer(
        &connection,
        "main",
        first,
        Some(side),
        &main_layer,
        &metadata,
    )
    .unwrap();
    let (base, layers) = first_parent_layers(&connection, merged, &|| Ok(())).unwrap();
    assert_eq!(base, Some(first));
    assert_eq!(layers.len(), 1);
    create_checkpoint(&connection, merged).unwrap();
    let snapshot = Snapshot::resolve(&connection, merged).unwrap();
    assert!(snapshot.node_exists(shared).unwrap());
    assert!(snapshot.node_exists(main_node).unwrap());
    assert!(!snapshot.node_exists(side_node).unwrap());
    drop(snapshot);
    delete_checkpoint(&connection, merged).unwrap();
    delete_checkpoint(&connection, first).unwrap();
    let snapshot = Snapshot::resolve(&connection, merged).unwrap();
    assert!(snapshot.node_exists(main_node).unwrap());
    assert!(!snapshot.node_exists(side_node).unwrap());
    assert_eq!(branch_head(&connection, "main").unwrap(), merged);
    assert_eq!(branch_head(&connection, "side").unwrap(), side);
    assert!(integrity_check(&connection).unwrap().is_empty());
}
