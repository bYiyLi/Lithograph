use lithograph_core::storage::{
    CommitMetadata, LayerBuilder, OwnerKind, PointValue, PropertyValue, RelationshipRecord,
    Snapshot, VectorCoordinateType, VectorValue, ZonedDateTimeValue, allocate_node_id,
    allocate_relationship_id, branch_head, commit_layer, create_branch, create_checkpoint,
    create_storage_schema, delete_checkpoint, initialize_root, integrity_check, intern_label,
    intern_property_key, intern_relationship_type, layer_between, layer_between_commits,
    load_snapshot_state, root_commit,
};
use rusqlite::{Connection, params};

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite must open");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
                id INTEGER PRIMARY KEY CHECK(id = 1),\
                magic TEXT NOT NULL,\
                database_id TEXT NOT NULL,\
                storage_format INTEGER NOT NULL\
            );",
        )
        .expect("test metadata table must be created");
    create_storage_schema(&connection).expect("storage schema must initialize");
    initialize_root(&connection).expect("Root Commit must initialize");
    connection
}

fn metadata(message: &str, committed_at: i64) -> CommitMetadata {
    CommitMetadata {
        author: Some("phase02-test".to_owned()),
        message: Some(message.to_owned()),
        committed_at,
    }
}

#[test]
fn fresh_init_is_idempotent_and_has_one_root_and_main() {
    let connection = fresh_storage();
    let first = initialize_root(&connection).expect("repeated init must succeed");
    let second = initialize_root(&connection).expect("repeated init must remain idempotent");
    assert_eq!(first, second);
    assert_eq!(
        first.root,
        root_commit(&connection).expect("Root must resolve")
    );
    assert_eq!(
        branch_head(&connection, "main").expect("main must resolve"),
        first.root
    );
    let commit_count: i64 = connection
        .query_row("SELECT count(*) FROM _lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count must query");
    let branch_count: i64 = connection
        .query_row("SELECT count(*) FROM _lithograph_branches", [], |row| {
            row.get(0)
        })
        .expect("branch count must query");
    assert_eq!(commit_count, 1);
    assert_eq!(branch_count, 1);
    assert!(
        integrity_check(&connection)
            .expect("integrity must run")
            .is_empty()
    );
}

#[test]
fn graph_delta_supports_labels_properties_parallel_edges_and_self_loops() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root must resolve");
    let (layer, ids) = graph_fixture(&connection);
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("graph", 1),
    )
    .expect("graph commit");
    let snapshot = Snapshot::resolve(&connection, commit).expect("snapshot resolves");
    assert_graph_snapshot(&snapshot, &ids);
    assert!(
        integrity_check(&connection)
            .expect("integrity must run")
            .is_empty()
    );
}

#[test]
fn first_parent_layer_composition_matches_full_snapshot_diff() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root must resolve");
    let label = intern_label(&connection, "Composed").expect("label");
    let key = intern_property_key(&connection, "value").expect("property key");
    let rel_type = intern_relationship_type(&connection, "LINK").expect("relationship type");
    let n1 = allocate_node_id(&connection).expect("n1");
    let n2 = allocate_node_id(&connection).expect("n2");
    let n3 = allocate_node_id(&connection).expect("n3");
    let n4 = allocate_node_id(&connection).expect("n4");
    let r1 = allocate_relationship_id(&connection).expect("r1");

    let mut base_layer = LayerBuilder::default();
    base_layer.add_node(n1).expect("base n1");
    base_layer.add_node(n2).expect("base n2");
    base_layer.add_label(n1, label).expect("base label");
    base_layer
        .set_property(OwnerKind::Node, n1, key, PropertyValue::Integer(1))
        .expect("base property");
    let relationship = RelationshipRecord {
        id: r1,
        source: n1,
        type_id: rel_type,
        target: n2,
    };
    base_layer
        .add_relationship(relationship)
        .expect("base relationship");
    let base = commit_layer(
        &connection,
        "main",
        root,
        None,
        &base_layer,
        &metadata("compose-base", 10),
    )
    .expect("base commit");

    let mut first = LayerBuilder::default();
    first.add_node(n3).expect("temporary node");
    first.add_label(n3, label).expect("temporary label");
    first
        .set_property(OwnerKind::Node, n3, key, PropertyValue::Integer(30))
        .expect("temporary property");
    first
        .set_property(OwnerKind::Node, n1, key, PropertyValue::Integer(2))
        .expect("first property update");
    let first_commit = commit_layer(
        &connection,
        "main",
        base,
        None,
        &first,
        &metadata("compose-first", 11),
    )
    .expect("first staged commit");

    let mut second = LayerBuilder::default();
    second
        .remove_property(OwnerKind::Node, n3, key)
        .expect("remove temporary property");
    second
        .remove_label(n3, label)
        .expect("remove temporary label");
    second.remove_node(n3).expect("remove temporary node");
    second.add_node(n4).expect("final node");
    second
        .set_property(OwnerKind::Node, n1, key, PropertyValue::Integer(3))
        .expect("final property update");
    second
        .remove_relationship(relationship)
        .expect("remove base relationship");
    let head = commit_layer(
        &connection,
        "main",
        first_commit,
        None,
        &second,
        &metadata("compose-second", 12),
    )
    .expect("second staged commit");

    let before = load_snapshot_state(&connection, base).expect("base state");
    let after = load_snapshot_state(&connection, head).expect("head state");
    let full = layer_between(&before, &after).expect("full state diff");
    let incremental =
        layer_between_commits(&connection, base, head).expect("incremental first-parent diff");
    assert_eq!(incremental, full);
    let counts = incremental.delta_counts();
    assert_eq!(counts.nodes_created, 1);
    assert_eq!(counts.relationships_deleted, 1);
    assert_eq!(counts.properties_set, 1);
}

struct GraphFixtureIds {
    alice: i64,
    person: i64,
    works_at: i64,
    knows: i64,
    name: i64,
    score: i64,
}

fn graph_fixture(connection: &Connection) -> (LayerBuilder, GraphFixtureIds) {
    let alice = allocate_node_id(connection).expect("node id");
    let acme = allocate_node_id(connection).expect("node id");
    let person = intern_label(connection, "Person").expect("label id");
    let works_at = intern_relationship_type(connection, "WORKS_AT").expect("type id");
    let knows = intern_relationship_type(connection, "KNOWS").expect("type id");
    let name = intern_property_key(connection, "name").expect("property key");
    let score = intern_property_key(connection, "score").expect("property key");
    let relationships = [
        RelationshipRecord {
            id: allocate_relationship_id(connection).expect("relationship id"),
            source: alice,
            type_id: works_at,
            target: acme,
        },
        RelationshipRecord {
            id: allocate_relationship_id(connection).expect("relationship id"),
            source: alice,
            type_id: works_at,
            target: acme,
        },
        RelationshipRecord {
            id: allocate_relationship_id(connection).expect("relationship id"),
            source: alice,
            type_id: knows,
            target: alice,
        },
    ];
    let mut layer = LayerBuilder::default();
    layer.add_node(alice).expect("node add");
    layer.add_node(acme).expect("node add");
    layer.add_label(alice, person).expect("label add");
    layer
        .set_property(
            OwnerKind::Node,
            alice,
            name,
            PropertyValue::String("Alice".to_owned()),
        )
        .expect("property set");
    layer
        .set_property(OwnerKind::Node, alice, score, PropertyValue::Float(-0.0))
        .expect("property set");
    for relationship in relationships {
        layer
            .add_relationship(relationship)
            .expect("relationship add");
    }
    (
        layer,
        GraphFixtureIds {
            alice,
            person,
            works_at,
            knows,
            name,
            score,
        },
    )
}

fn assert_graph_snapshot(snapshot: &Snapshot<'_>, ids: &GraphFixtureIds) {
    assert!(snapshot.node_exists(ids.alice).expect("node lookup"));
    assert_eq!(
        snapshot.labels(ids.alice).expect("label lookup"),
        vec![ids.person]
    );
    assert_eq!(
        snapshot
            .property(OwnerKind::Node, ids.alice, ids.name)
            .expect("property lookup"),
        Some(PropertyValue::String("Alice".to_owned()))
    );
    let value = snapshot
        .property(OwnerKind::Node, ids.alice, ids.score)
        .expect("property lookup")
        .expect("float exists");
    match value {
        PropertyValue::Float(value) => assert_eq!(value.to_bits(), (-0.0_f64).to_bits()),
        other => panic!("score must be Float, got {other:?}"),
    }
    assert_eq!(
        snapshot
            .outgoing(ids.alice, Some(ids.works_at))
            .expect("adjacency")
            .len(),
        2
    );
    assert_eq!(
        snapshot
            .outgoing(ids.alice, Some(ids.knows))
            .expect("adjacency")
            .len(),
        1
    );
    assert_eq!(
        snapshot
            .incoming(ids.alice, Some(ids.knows))
            .expect("adjacency")
            .len(),
        1
    );
}

#[test]
fn typed_property_payloads_round_trip_without_json_type_loss() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root must resolve");
    let node = allocate_node_id(&connection).expect("node id");
    let values = [
        (
            "list",
            PropertyValue::List(vec![PropertyValue::Integer(7), PropertyValue::Integer(8)]),
        ),
        ("date", PropertyValue::Date(20_000)),
        ("local_time", PropertyValue::LocalTime(123_456_789)),
        (
            "time",
            PropertyValue::Time {
                nanoseconds: 9_876_543_210,
                offset_seconds: 28_800,
            },
        ),
        (
            "local_datetime",
            PropertyValue::LocalDateTime {
                day: 20_001,
                nanoseconds: 42,
            },
        ),
        (
            "zoned_datetime",
            PropertyValue::ZonedDateTime(ZonedDateTimeValue {
                epoch_seconds: 1_700_000_000,
                nanoseconds: 123,
                zone_id: "Asia/Shanghai".to_owned(),
            }),
        ),
        (
            "duration",
            PropertyValue::Duration {
                months: 2,
                days: 3,
                seconds: 4,
                nanoseconds: 5,
            },
        ),
        (
            "point",
            PropertyValue::Point(PointValue {
                crs: 7_203,
                coordinates: vec![1.25, -0.0],
            }),
        ),
        (
            "vector",
            PropertyValue::Vector(VectorValue {
                coordinate_type: VectorCoordinateType::F32,
                dimension: 2,
                packed: [1.5_f32.to_le_bytes(), (-0.0_f32).to_le_bytes()].concat(),
            }),
        ),
        ("uuid", PropertyValue::Uuid([0x11; 16])),
    ];
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("node add");
    let mut keys = Vec::new();
    for (name, value) in &values {
        let key = intern_property_key(&connection, name).expect("property key");
        layer
            .set_property(OwnerKind::Node, node, key, value.clone())
            .expect("property set");
        keys.push(key);
    }
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("typed", 2),
    )
    .expect("typed commit");
    let snapshot = Snapshot::resolve(&connection, commit).expect("snapshot");
    for ((_, expected), key) in values.iter().zip(keys) {
        let actual = snapshot
            .property(OwnerKind::Node, node, key)
            .expect("property lookup")
            .expect("property exists");
        assert_eq!(&actual, expected);
    }
}

#[test]
fn multiple_layers_resolve_against_first_parent_history() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    let node = allocate_node_id(&connection).expect("node id");
    let key = intern_property_key(&connection, "version").expect("property key");
    let label = intern_label(&connection, "Tracked").expect("label");

    let mut first = LayerBuilder::default();
    first.add_node(node).expect("node add");
    first
        .set_property(OwnerKind::Node, node, key, PropertyValue::Integer(1))
        .expect("property set");
    let c1 = commit_layer(
        &connection,
        "main",
        root,
        None,
        &first,
        &metadata("one", 10),
    )
    .expect("first commit");

    let mut second = LayerBuilder::default();
    second.add_label(node, label).expect("label add");
    second
        .set_property(OwnerKind::Node, node, key, PropertyValue::Integer(2))
        .expect("property update");
    let c2 = commit_layer(&connection, "main", c1, None, &second, &metadata("two", 11))
        .expect("second commit");

    let first_snapshot = Snapshot::resolve(&connection, c1).expect("first snapshot");
    let second_snapshot = Snapshot::resolve(&connection, c2).expect("second snapshot");
    assert_eq!(
        first_snapshot
            .property(OwnerKind::Node, node, key)
            .expect("property"),
        Some(PropertyValue::Integer(1))
    );
    assert_eq!(
        second_snapshot
            .property(OwnerKind::Node, node, key)
            .expect("property"),
        Some(PropertyValue::Integer(2))
    );
    assert_eq!(second_snapshot.labels(node).expect("labels"), vec![label]);
}

#[test]
fn committed_identity_allocator_does_not_reuse_deleted_ids() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    let first = allocate_node_id(&connection).expect("node id");
    let mut add = LayerBuilder::default();
    add.add_node(first).expect("node add");
    let c1 = commit_layer(&connection, "main", root, None, &add, &metadata("add", 20))
        .expect("add commit");
    let mut remove = LayerBuilder::default();
    remove.remove_node(first).expect("node remove");
    let c2 = commit_layer(
        &connection,
        "main",
        c1,
        None,
        &remove,
        &metadata("remove", 21),
    )
    .expect("remove commit");
    let second = allocate_node_id(&connection).expect("next node id");
    assert!(second > first);
    assert!(
        !Snapshot::resolve(&connection, c2)
            .expect("snapshot")
            .node_exists(first)
            .expect("lookup")
    );
}

#[test]
fn merge_shaped_snapshot_replays_first_parent_and_merge_layer_only() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    create_branch(&connection, "side", root).expect("side branch");
    let main_node = allocate_node_id(&connection).expect("main node");
    let side_node = allocate_node_id(&connection).expect("side node");

    let mut main_layer = LayerBuilder::default();
    main_layer.add_node(main_node).expect("main node add");
    let main_commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &main_layer,
        &metadata("main", 30),
    )
    .expect("main commit");

    let mut side_layer = LayerBuilder::default();
    side_layer.add_node(side_node).expect("side node add");
    let side_commit = commit_layer(
        &connection,
        "side",
        root,
        None,
        &side_layer,
        &metadata("side", 31),
    )
    .expect("side commit");

    let merge_commit = commit_layer(
        &connection,
        "main",
        main_commit,
        Some(side_commit),
        &LayerBuilder::default(),
        &metadata("merge", 32),
    )
    .expect("merge-shaped commit");
    let snapshot = Snapshot::resolve(&connection, merge_commit).expect("merge snapshot");
    assert!(snapshot.node_exists(main_node).expect("main lookup"));
    assert!(!snapshot.node_exists(side_node).expect("side lookup"));
    let parent2: Vec<u8> = connection
        .query_row(
            "SELECT parent2 FROM _lithograph_commits WHERE id = ?1",
            [merge_commit.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("parent2 query");
    assert_eq!(parent2, side_commit.as_bytes());
    assert!(integrity_check(&connection).expect("integrity").is_empty());
}

#[test]
fn checkpoint_is_derived_and_adjacency_uses_addressable_indexes() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    let source = allocate_node_id(&connection).expect("source");
    let target = allocate_node_id(&connection).expect("target");
    let type_id = intern_relationship_type(&connection, "EDGE").expect("type");
    let relationship_id = allocate_relationship_id(&connection).expect("relationship");
    let mut layer = LayerBuilder::default();
    layer.add_node(source).expect("source add");
    layer.add_node(target).expect("target add");
    layer
        .add_relationship(RelationshipRecord {
            id: relationship_id,
            source,
            type_id,
            target,
        })
        .expect("relationship add");
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("checkpoint", 40),
    )
    .expect("commit");
    let before = Snapshot::resolve(&connection, commit)
        .expect("snapshot")
        .semantic_hash()
        .expect("hash");
    let commits_before: i64 = connection
        .query_row("SELECT count(*) FROM _lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count");

    create_checkpoint(&connection, commit).expect("checkpoint create");
    let with_checkpoint = Snapshot::resolve(&connection, commit)
        .expect("checkpoint snapshot")
        .semantic_hash()
        .expect("hash");
    assert_eq!(before, with_checkpoint);
    let commits_after: i64 = connection
        .query_row("SELECT count(*) FROM _lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count");
    assert_eq!(commits_before, commits_after);

    assert_query_plan_uses_index(
        &connection,
        "EXPLAIN QUERY PLAN SELECT relationship_id FROM _lithograph_cp_relationships WHERE commit_id = ?1 AND source_id = ?2 AND type_id = ?3 ORDER BY type_id, target_id, relationship_id",
        commit,
        source,
        type_id,
        "_lithograph_cp_rel_out",
    );
    assert_query_plan_uses_index(
        &connection,
        "EXPLAIN QUERY PLAN SELECT relationship_id FROM _lithograph_cp_relationships WHERE commit_id = ?1 AND target_id = ?2 AND type_id = ?3 ORDER BY type_id, source_id, relationship_id",
        commit,
        target,
        type_id,
        "_lithograph_cp_rel_in",
    );

    delete_checkpoint(&connection, commit).expect("checkpoint delete");
    let rebuilt = Snapshot::resolve(&connection, commit)
        .expect("rebuilt snapshot")
        .semantic_hash()
        .expect("hash");
    assert_eq!(before, rebuilt);
}

fn assert_query_plan_uses_index(
    connection: &Connection,
    sql: &str,
    commit: lithograph_core::storage::HashId,
    node_id: i64,
    type_id: i64,
    expected_index: &str,
) {
    let mut statement = connection.prepare(sql).expect("query plan prepare");
    let rows = statement
        .query_map(
            params![commit.as_bytes().as_slice(), node_id, type_id],
            |row| row.get::<_, String>(3),
        )
        .expect("query plan rows");
    let plan = rows
        .collect::<Result<Vec<_>, _>>()
        .expect("query plan collect")
        .join("\n");
    assert!(
        plan.contains(expected_index),
        "plan did not use {expected_index}: {plan}"
    );
}

#[test]
fn canonical_layer_hash_is_independent_of_mutation_insertion_order() {
    let mut first = LayerBuilder::default();
    first.add_node(2).expect("node");
    first.add_node(1).expect("node");
    first.add_label(2, 4).expect("label");
    first
        .set_property(OwnerKind::Node, 1, 3, PropertyValue::Integer(7))
        .expect("property");

    let mut second = LayerBuilder::default();
    second
        .set_property(OwnerKind::Node, 1, 3, PropertyValue::Integer(7))
        .expect("property");
    second.add_label(2, 4).expect("label");
    second.add_node(1).expect("node");
    second.add_node(2).expect("node");

    assert_eq!(
        first.canonical_lce1().expect("canonical bytes"),
        second.canonical_lce1().expect("canonical bytes")
    );
    assert_eq!(
        first.content_hash().expect("hash"),
        second.content_hash().expect("hash")
    );
}

#[test]
fn outer_transaction_rollback_removes_layer_commit_and_ref_move() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    let baseline_layers: i64 = connection
        .query_row("SELECT count(*) FROM _lithograph_layers", [], |row| {
            row.get(0)
        })
        .expect("layer count");
    connection.execute_batch("BEGIN").expect("outer begin");
    let node = allocate_node_id(&connection).expect("node id");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("node add");
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("rolled back", 50),
    )
    .expect("logical commit inside outer transaction");
    assert_eq!(branch_head(&connection, "main").expect("head"), commit);
    connection
        .execute_batch("ROLLBACK")
        .expect("outer rollback");

    assert_eq!(
        branch_head(&connection, "main").expect("head after rollback"),
        root
    );
    let commit_count: i64 = connection
        .query_row("SELECT count(*) FROM _lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count");
    let layer_count: i64 = connection
        .query_row("SELECT count(*) FROM _lithograph_layers", [], |row| {
            row.get(0)
        })
        .expect("layer count");
    assert_eq!(commit_count, 1);
    assert_eq!(layer_count, baseline_layers);
    let reused_uncommitted = allocate_node_id(&connection).expect("post-rollback node id");
    assert_eq!(reused_uncommitted, node);
}

#[test]
fn persisted_byte_mutation_is_detected_by_integrity_checker() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    let node = allocate_node_id(&connection).expect("node");
    let key = intern_property_key(&connection, "name").expect("key");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("node add");
    layer
        .set_property(
            OwnerKind::Node,
            node,
            key,
            PropertyValue::String("before".to_owned()),
        )
        .expect("property");
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("tamper target", 60),
    )
    .expect("commit");
    let layer_id: i64 = connection
        .query_row(
            "SELECT layer_id FROM _lithograph_commits WHERE id = ?1",
            [commit.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("layer id");
    connection
        .execute(
            "UPDATE _lithograph_property_delta SET text_value = 'after' WHERE layer_id = ?1 AND owner_kind = 1 AND owner_id = ?2 AND key_id = ?3",
            params![layer_id, node, key],
        )
        .expect("intentional corruption");
    let issues = integrity_check(&connection).expect("integrity check");
    assert!(
        issues
            .iter()
            .any(|issue| issue.code == "history.layer_hash_mismatch"),
        "expected layer hash mismatch, got {issues:?}"
    );
}

#[test]
fn relationship_identity_tuple_change_is_rejected_and_rolled_back() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    let source = allocate_node_id(&connection).expect("source");
    let first_target = allocate_node_id(&connection).expect("first target");
    let second_target = allocate_node_id(&connection).expect("second target");
    let type_id = intern_relationship_type(&connection, "STABLE").expect("type");
    let relationship_id = allocate_relationship_id(&connection).expect("relationship id");
    let mut initial = LayerBuilder::default();
    initial.add_node(source).expect("source add");
    initial.add_node(first_target).expect("first target add");
    initial.add_node(second_target).expect("second target add");
    initial
        .add_relationship(RelationshipRecord {
            id: relationship_id,
            source,
            type_id,
            target: first_target,
        })
        .expect("relationship add");
    let first = commit_layer(
        &connection,
        "main",
        root,
        None,
        &initial,
        &metadata("initial relationship", 70),
    )
    .expect("initial commit");
    let commits_before: i64 = connection
        .query_row("SELECT count(*) FROM _lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count");
    let layers_before: i64 = connection
        .query_row("SELECT count(*) FROM _lithograph_layers", [], |row| {
            row.get(0)
        })
        .expect("layer count");

    let mut invalid = LayerBuilder::default();
    invalid
        .add_relationship(RelationshipRecord {
            id: relationship_id,
            source,
            type_id,
            target: second_target,
        })
        .expect("synthetic invalid relationship delta");
    let error = commit_layer(
        &connection,
        "main",
        first,
        None,
        &invalid,
        &metadata("invalid tuple", 71),
    )
    .expect_err("relationship tuple mutation must fail");
    assert!(error.to_string().contains("immutable endpoints or type"));
    assert_eq!(
        branch_head(&connection, "main").expect("branch head"),
        first
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM _lithograph_commits", [], |row| row
                .get::<_, i64>(0))
            .expect("commit count"),
        commits_before
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM _lithograph_layers", [], |row| row
                .get::<_, i64>(0))
            .expect("layer count"),
        layers_before
    );
}

#[test]
fn tampered_content_addressed_layer_is_not_reused() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    let node = allocate_node_id(&connection).expect("node");
    let key = intern_property_key(&connection, "payload").expect("key");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("node add");
    layer
        .set_property(
            OwnerKind::Node,
            node,
            key,
            PropertyValue::String("canonical".to_owned()),
        )
        .expect("property");
    let first = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("first", 80),
    )
    .expect("first commit");
    let layer_id: i64 = connection
        .query_row(
            "SELECT layer_id FROM _lithograph_commits WHERE id = ?1",
            [first.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("layer id");
    connection
        .execute(
            "UPDATE _lithograph_property_delta SET text_value = 'tampered' WHERE layer_id = ?1 AND owner_kind = 1 AND owner_id = ?2 AND key_id = ?3",
            params![layer_id, node, key],
        )
        .expect("tamper layer payload");

    let error = commit_layer(
        &connection,
        "main",
        first,
        None,
        &layer,
        &metadata("reuse", 81),
    )
    .expect_err("tampered Layer must not be reused by hash");
    assert!(error.to_string().contains("persisted content hash"));
    assert_eq!(
        branch_head(&connection, "main").expect("branch head"),
        first
    );
}

#[test]
fn tampered_content_addressed_commit_is_not_reused() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    let node = allocate_node_id(&connection).expect("node");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("node add");
    let commit_metadata = metadata("content addressed", 120);
    let commit = commit_layer(&connection, "main", root, None, &layer, &commit_metadata)
        .expect("initial commit");

    connection
        .execute(
            "UPDATE _lithograph_branches SET commit_id = ?1 WHERE name = 'main'",
            [root.as_bytes().as_slice()],
        )
        .expect("synthetic ref reset");
    connection
        .execute(
            "UPDATE _lithograph_commits SET message = 'tampered' WHERE id = ?1",
            [commit.as_bytes().as_slice()],
        )
        .expect("synthetic commit corruption");

    let error = commit_layer(&connection, "main", root, None, &layer, &commit_metadata)
        .expect_err("tampered Commit must not be reused by content id");
    assert!(error.to_string().contains("content-addressed fields"));
    assert_eq!(branch_head(&connection, "main").expect("branch head"), root);
}
