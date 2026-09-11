use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, LogicalOperator, QueryCursor, QueryErrorKind, SnapshotSelector, prepare,
};
use lithograph_core::storage::{
    CommitMetadata, LayerBuilder, OwnerKind, PointValue, PropertyValue, RelationshipRecord,
    VectorCoordinateType, VectorValue, ZonedDateTimeValue, allocate_node_id,
    allocate_relationship_id, branch_head, commit_layer, create_checkpoint, create_storage_schema,
    initialize_root, intern_label, intern_property_key, intern_relationship_type,
};
use rusqlite::Connection;

#[path = "phase04_query/regressions.rs"]
mod regressions;

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite must open");
    connection.execute_batch(
        "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1),magic TEXT NOT NULL,database_id TEXT NOT NULL,storage_format INTEGER NOT NULL);",
    ).expect("metadata table");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn metadata(message: &str, committed_at: i64) -> CommitMetadata {
    CommitMetadata {
        author: Some("phase04-test".to_owned()),
        message: Some(message.to_owned()),
        committed_at,
    }
}

struct Fixture {
    connection: Connection,
    first_commit: lithograph_core::storage::HashId,
    second_commit: lithograph_core::storage::HashId,
}

fn fixture() -> Fixture {
    let connection = fresh_storage();
    let root = branch_head(&connection, "main").expect("main");
    let person = intern_label(&connection, "Person").expect("Person");
    let secret = intern_label(&connection, "Secret").expect("Secret");
    let knows = intern_relationship_type(&connection, "KNOWS").expect("KNOWS");
    let name = intern_property_key(&connection, "name").expect("name");
    let age = intern_property_key(&connection, "age").expect("age");
    let alice = allocate_node_id(&connection).expect("alice");
    let bob = allocate_node_id(&connection).expect("bob");
    let carol = allocate_node_id(&connection).expect("carol");
    let hidden = allocate_node_id(&connection).expect("hidden");
    let mut layer = LayerBuilder::default();
    for node in [alice, bob, carol, hidden] {
        layer.add_node(node).expect("add node");
    }
    for node in [alice, bob, carol] {
        layer.add_label(node, person).expect("Person label");
    }
    layer.add_label(hidden, secret).expect("Secret label");
    for (node, value, years) in [
        (alice, "Alice", 41),
        (bob, "Bob", 33),
        (carol, "Carol", 27),
        (hidden, "Hidden", 99),
    ] {
        layer
            .set_property(
                OwnerKind::Node,
                node,
                name,
                PropertyValue::String(value.to_owned()),
            )
            .expect("name property");
        layer
            .set_property(OwnerKind::Node, node, age, PropertyValue::Integer(years))
            .expect("age property");
    }
    for (source, target) in [(alice, bob), (bob, carol), (alice, hidden)] {
        layer
            .add_relationship(RelationshipRecord {
                id: allocate_relationship_id(&connection).expect("relationship id"),
                source,
                type_id: knows,
                target,
            })
            .expect("add relationship");
    }
    let first_commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("first", 1),
    )
    .expect("first commit");
    create_checkpoint(&connection, first_commit).expect("checkpoint");

    let mut second = LayerBuilder::default();
    second
        .set_property(OwnerKind::Node, bob, age, PropertyValue::Integer(34))
        .expect("update age");
    let second_commit = commit_layer(
        &connection,
        "main",
        first_commit,
        None,
        &second,
        &metadata("second", 2),
    )
    .expect("second commit");
    Fixture {
        connection,
        first_commit,
        second_commit,
    }
}

fn rows(connection: &Connection, query: &str, options: ExecutionOptions) -> Vec<Vec<Value>> {
    rows_with_params(connection, query, BTreeMap::new(), options)
}

fn rows_with_params(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Vec<Vec<Value>> {
    let prepared = prepare(connection, query, params, options).expect("prepare query");
    let mut cursor = QueryCursor::new(prepared);
    let mut result = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 2).expect("query batch");
        result.extend(batch.rows);
        if batch.done {
            return result;
        }
    }
}

#[test]
fn match_where_projection_order_skip_limit_execute_against_snapshot() {
    let fixture = fixture();
    let result = rows(
        &fixture.connection,
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age >= 30 RETURN a.name AS name, b.name AS friend ORDER BY name DESC SKIP 0 LIMIT 5",
        ExecutionOptions::default(),
    );
    assert_eq!(
        result,
        vec![
            vec![
                Value::String("Bob".to_owned()),
                Value::String("Carol".to_owned())
            ],
            vec![
                Value::String("Alice".to_owned()),
                Value::String("Bob".to_owned())
            ],
        ]
    );
}

#[test]
fn multi_pattern_and_parameters_execute_as_cartesian_match_steps() {
    let fixture = fixture();
    let params = BTreeMap::from([("minimum".to_owned(), Value::Integer(30))]);
    let result = rows_with_params(
        &fixture.connection,
        "MATCH (a:Person), (b:Person) WHERE a.name = 'Alice' AND b.age >= $minimum AND b.name <> a.name RETURN a.name AS source, b.name AS peer ORDER BY peer",
        params,
        ExecutionOptions::default(),
    );
    assert_eq!(
        result,
        vec![vec![
            Value::String("Alice".to_owned()),
            Value::String("Bob".to_owned())
        ]]
    );
}

#[test]
fn incoming_and_undirected_relationship_patterns_use_adjacency() {
    let fixture = fixture();
    let incoming = rows(
        &fixture.connection,
        "MATCH (a:Person)<-[:KNOWS]-(b:Person) RETURN a.name AS target, b.name AS source ORDER BY target",
        ExecutionOptions::default(),
    );
    assert_eq!(incoming.len(), 2);
    assert_eq!(incoming[0][0], Value::String("Bob".to_owned()));
    let undirected = rows(
        &fixture.connection,
        "MATCH (a:Person)-[:KNOWS]-(b:Person) RETURN a.name AS a, b.name AS b ORDER BY a, b",
        ExecutionOptions::default(),
    );
    assert_eq!(undirected.len(), 4);
}

#[test]
fn optional_match_preserves_unmatched_rows_with_null() {
    let fixture = fixture();
    let result = rows(
        &fixture.connection,
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN a.name AS name, b.name AS friend ORDER BY name, friend",
        ExecutionOptions::default(),
    );
    assert_eq!(
        result,
        vec![
            vec![
                Value::String("Alice".to_owned()),
                Value::String("Bob".to_owned())
            ],
            vec![
                Value::String("Bob".to_owned()),
                Value::String("Carol".to_owned())
            ],
            vec![Value::String("Carol".to_owned()), Value::Null],
        ]
    );
}

#[test]
fn graph_view_filters_scan_and_relationship_traversal_without_dictionary_writes() {
    let fixture = fixture();
    let options = ExecutionOptions::parse_text(r#"{"graphView":{"excludeAnyLabels":["Secret"]}}"#)
        .expect("options");
    let result = rows(
        &fixture.connection,
        "MATCH (a)-[:KNOWS]->(b) RETURN b.name AS name ORDER BY name",
        options,
    );
    assert_eq!(
        result,
        vec![
            vec![Value::String("Bob".to_owned())],
            vec![Value::String("Carol".to_owned())],
        ]
    );
    let before: i64 = fixture
        .connection
        .query_row("SELECT count(*) FROM _lithograph_labels", [], |row| {
            row.get(0)
        })
        .expect("label count");
    let unknown =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["DoesNotExist"]}}"#)
            .expect("options");
    assert!(rows(&fixture.connection, "MATCH (n) RETURN n", unknown).is_empty());
    let after: i64 = fixture
        .connection
        .query_row("SELECT count(*) FROM _lithograph_labels", [], |row| {
            row.get(0)
        })
        .expect("label count");
    assert_eq!(before, after);
}

#[test]
fn graph_view_normalization_and_invalid_options_match_contract() {
    let fixture = fixture();
    let query = "MATCH (n) RETURN n.name AS name ORDER BY name";
    let full = rows(&fixture.connection, query, ExecutionOptions::default());
    let empty = rows(
        &fixture.connection,
        query,
        ExecutionOptions::parse_text(r#"{"graphView":{}}"#).expect("empty graph view"),
    );
    assert_eq!(empty, full);

    let required_once = rows(
        &fixture.connection,
        query,
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Person"]}}"#)
            .expect("single required label"),
    );
    let required_duplicate = rows(
        &fixture.connection,
        query,
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Person","Person"]}}"#)
            .expect("duplicate required label"),
    );
    assert_eq!(required_duplicate, required_once);

    let labels_before: i64 = fixture
        .connection
        .query_row("SELECT count(*) FROM _lithograph_labels", [], |row| {
            row.get(0)
        })
        .expect("label count");
    let unknown_excluded = rows(
        &fixture.connection,
        query,
        ExecutionOptions::parse_text(r#"{"graphView":{"excludeAnyLabels":["DoesNotExist"]}}"#)
            .expect("unknown excluded label"),
    );
    let labels_after: i64 = fixture
        .connection
        .query_row("SELECT count(*) FROM _lithograph_labels", [], |row| {
            row.get(0)
        })
        .expect("label count");
    assert_eq!(unknown_excluded, full);
    assert_eq!(labels_before, labels_after);

    for invalid in [
        r#"{"graphView":null}"#,
        r#"{"graphView":{"unknown":[]}}"#,
        r#"{"graphView":{"requireAllLabels":null}}"#,
        r#"{"graphView":{"requireAllLabels":["Person",1]}}"#,
        r#"{"graphView":{"requireAllLabels":["Person"],"excludeAnyLabels":["Person"]}}"#,
    ] {
        let error = ExecutionOptions::parse_text(invalid).expect_err("invalid graphView");
        assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    }
}

#[test]
fn graph_view_applies_before_optional_distinct_and_aggregation() {
    let fixture = fixture();
    let options = ExecutionOptions::parse_text(r#"{"graphView":{"excludeAnyLabels":["Secret"]}}"#)
        .expect("options");
    let count = rows(
        &fixture.connection,
        "MATCH (n) RETURN count(n) AS total",
        options.clone(),
    );
    assert_eq!(count, vec![vec![Value::Integer(3)]]);

    let distinct = rows(
        &fixture.connection,
        "MATCH (a)-[:KNOWS]-(b) RETURN DISTINCT a.name AS name ORDER BY name",
        options.clone(),
    );
    assert_eq!(
        distinct,
        vec![
            vec![Value::String("Alice".to_owned())],
            vec![Value::String("Bob".to_owned())],
            vec![Value::String("Carol".to_owned())],
        ]
    );

    let optional = rows(
        &fixture.connection,
        "MATCH (a) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a.name AS name, b.name AS friend ORDER BY name, friend",
        options,
    );
    assert_eq!(
        optional,
        vec![
            vec![
                Value::String("Alice".to_owned()),
                Value::String("Bob".to_owned())
            ],
            vec![
                Value::String("Bob".to_owned()),
                Value::String("Carol".to_owned())
            ],
            vec![Value::String("Carol".to_owned()), Value::Null],
        ]
    );
}

#[test]
fn historical_snapshot_and_current_branch_are_isolated() {
    let fixture = fixture();
    let current = rows(
        &fixture.connection,
        "MATCH (n:Person) WHERE n.name = 'Bob' RETURN n.age AS age",
        ExecutionOptions::default(),
    );
    assert_eq!(current, vec![vec![Value::Integer(34)]]);
    let historical = ExecutionOptions {
        snapshot: SnapshotSelector::Commit(fixture.first_commit.to_hex()),
        ..ExecutionOptions::default()
    };
    let old = rows(
        &fixture.connection,
        "MATCH (n:Person) WHERE n.name = 'Bob' RETURN n.age AS age",
        historical,
    );
    assert_eq!(old, vec![vec![Value::Integer(33)]]);
    assert_eq!(
        branch_head(&fixture.connection, "main").expect("main"),
        fixture.second_commit
    );
}

#[test]
fn explain_does_not_execute_and_profile_returns_data_with_metrics() {
    let fixture = fixture();
    let prepared = prepare(
        &fixture.connection,
        "EXPLAIN MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS name",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("explain prepare");
    let mut explain = QueryCursor::new(prepared);
    assert_eq!(explain.columns(), &["plan".to_owned()]);
    let batch = explain
        .next_batch(&fixture.connection, 10)
        .expect("explain");
    assert!(batch.done);
    assert_eq!(batch.summary.as_ref().expect("summary").metrics.db_hits, 0);
    let Value::String(plan) = &batch.rows[0][0] else {
        panic!("plan must be String");
    };
    assert!(plan.contains("LabelIndexScan"));
    assert!(plan.contains("AdjacencySeek"));

    let prepared = prepare(
        &fixture.connection,
        "PROFILE MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS name",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("profile prepare");
    let mut profile = QueryCursor::new(prepared);
    let batch = profile
        .next_batch(&fixture.connection, 10)
        .expect("profile");
    assert_eq!(batch.rows.len(), 2);
    let summary = batch.summary.expect("profile summary");
    assert_eq!(summary.metrics.rows, 2);
    assert!(summary.metrics.db_hits > 0);
}

#[test]
fn basic_count_and_distinct_execute_with_cypher_values() {
    let fixture = fixture();
    let count = rows(
        &fixture.connection,
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN count(b) AS total",
        ExecutionOptions::default(),
    );
    assert_eq!(count, vec![vec![Value::Integer(2)]]);
    let distinct = rows(
        &fixture.connection,
        "MATCH (a:Person)-[:KNOWS]-(b:Person) RETURN DISTINCT a.name AS name ORDER BY name",
        ExecutionOptions::default(),
    );
    assert_eq!(
        distinct,
        vec![
            vec![Value::String("Alice".to_owned())],
            vec![Value::String("Bob".to_owned())],
            vec![Value::String("Carol".to_owned())],
        ]
    );
}

#[test]
fn distinct_uses_cypher_numeric_equivalence_across_integer_and_float() {
    let connection = fresh_storage();
    let root = branch_head(&connection, "main").expect("main");
    let score = intern_property_key(&connection, "score").expect("score");
    let mut layer = LayerBuilder::default();
    let integer_node = allocate_node_id(&connection).expect("integer node");
    let float_node = allocate_node_id(&connection).expect("float node");
    for node in [integer_node, float_node] {
        layer.add_node(node).expect("add node");
    }
    layer
        .set_property(
            OwnerKind::Node,
            integer_node,
            score,
            PropertyValue::Integer(1),
        )
        .expect("integer score");
    layer
        .set_property(
            OwnerKind::Node,
            float_node,
            score,
            PropertyValue::Float(1.0),
        )
        .expect("float score");
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("numeric distinct", 1),
    )
    .expect("commit");
    create_checkpoint(&connection, commit).expect("checkpoint");

    let result = rows(
        &connection,
        "MATCH (n) RETURN DISTINCT n.score AS score ORDER BY score",
        ExecutionOptions::default(),
    );
    assert_eq!(result, vec![vec![Value::Integer(1)]]);
}

#[test]
fn graph_materialization_covers_all_persisted_property_value_families() {
    let connection = fresh_storage();
    let root = branch_head(&connection, "main").expect("main");
    let rich = intern_label(&connection, "Rich").expect("Rich");
    let node = allocate_node_id(&connection).expect("node");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("node");
    layer.add_label(node, rich).expect("label");
    for (name, value) in rich_scalar_properties()
        .into_iter()
        .chain(rich_vector_properties())
    {
        let key = intern_property_key(&connection, name).expect("property key");
        layer
            .set_property(OwnerKind::Node, node, key, value)
            .expect("property");
    }
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("rich values", 1),
    )
    .expect("commit");
    create_checkpoint(&connection, commit).expect("checkpoint");

    let result = rows(
        &connection,
        "MATCH (n:Rich) RETURN n",
        ExecutionOptions::default(),
    );
    assert_eq!(result.len(), 1);
    let Value::Node(materialized) = &result[0][0] else {
        panic!("query must materialize a Node");
    };
    assert_eq!(materialized.properties.len(), 19);
    assert!(matches!(materialized.properties["date"], Value::Date(_)));
    assert!(matches!(materialized.properties["point"], Value::Point(_)));
    assert!(matches!(
        materialized.properties["vector_f64"],
        Value::Vector(_)
    ));
    assert!(matches!(materialized.properties["uuid"], Value::Uuid(_)));
}

fn rich_scalar_properties() -> Vec<(&'static str, PropertyValue)> {
    vec![
        ("boolean", PropertyValue::Boolean(true)),
        ("integer", PropertyValue::Integer(7)),
        ("float", PropertyValue::Float(2.5)),
        ("string", PropertyValue::String("value".to_owned())),
        (
            "list",
            PropertyValue::List(vec![PropertyValue::Integer(1), PropertyValue::Integer(2)]),
        ),
        ("date", PropertyValue::Date(0)),
        ("local_time", PropertyValue::LocalTime(1_234_000_000)),
        (
            "time",
            PropertyValue::Time {
                nanoseconds: 3_600_000_000_000,
                offset_seconds: 3_600,
            },
        ),
        (
            "local_datetime",
            PropertyValue::LocalDateTime {
                day: 0,
                nanoseconds: 2_000_000_000,
            },
        ),
        (
            "zoned_datetime",
            PropertyValue::ZonedDateTime(ZonedDateTimeValue {
                epoch_seconds: 0,
                nanoseconds: 500_000_000,
                zone_id: "+01:00".to_owned(),
            }),
        ),
        (
            "duration",
            PropertyValue::Duration {
                months: 1,
                days: 2,
                seconds: 3,
                nanoseconds: 4,
            },
        ),
        (
            "point",
            PropertyValue::Point(PointValue {
                crs: 7_203,
                coordinates: vec![1.25, -2.5],
            }),
        ),
        (
            "uuid",
            PropertyValue::Uuid([
                0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
                0x00, 0x00,
            ]),
        ),
    ]
}

fn rich_vector_properties() -> Vec<(&'static str, PropertyValue)> {
    vec![
        (
            "vector_i8",
            PropertyValue::Vector(VectorValue {
                coordinate_type: VectorCoordinateType::I8,
                dimension: 2,
                packed: vec![1, 255],
            }),
        ),
        (
            "vector_i16",
            PropertyValue::Vector(VectorValue {
                coordinate_type: VectorCoordinateType::I16,
                dimension: 2,
                packed: [1_i16.to_le_bytes(), (-2_i16).to_le_bytes()].concat(),
            }),
        ),
        (
            "vector_i32",
            PropertyValue::Vector(VectorValue {
                coordinate_type: VectorCoordinateType::I32,
                dimension: 2,
                packed: [1_i32.to_le_bytes(), (-2_i32).to_le_bytes()].concat(),
            }),
        ),
        (
            "vector_i64",
            PropertyValue::Vector(VectorValue {
                coordinate_type: VectorCoordinateType::I64,
                dimension: 2,
                packed: [1_i64.to_le_bytes(), (-2_i64).to_le_bytes()].concat(),
            }),
        ),
        (
            "vector_f32",
            PropertyValue::Vector(VectorValue {
                coordinate_type: VectorCoordinateType::F32,
                dimension: 2,
                packed: [1.5_f32.to_le_bytes(), (-2.5_f32).to_le_bytes()].concat(),
            }),
        ),
        (
            "vector_f64",
            PropertyValue::Vector(VectorValue {
                coordinate_type: VectorCoordinateType::F64,
                dimension: 2,
                packed: [1.5_f64.to_le_bytes(), (-2.5_f64).to_le_bytes()].concat(),
            }),
        ),
    ]
}

#[test]
fn external_sort_spill_matches_expected_cypher_order() {
    let connection = fresh_storage();
    let root = branch_head(&connection, "main").expect("main");
    let person = intern_label(&connection, "Person").expect("Person");
    let rank = intern_property_key(&connection, "rank").expect("rank");
    let mut layer = LayerBuilder::default();
    for value in (0_i64..1_300).rev() {
        let node = allocate_node_id(&connection).expect("node");
        layer.add_node(node).expect("add node");
        layer.add_label(node, person).expect("label");
        layer
            .set_property(OwnerKind::Node, node, rank, PropertyValue::Integer(value))
            .expect("rank");
    }
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("sort", 1),
    )
    .expect("commit");
    create_checkpoint(&connection, commit).expect("checkpoint");
    let result = rows(
        &connection,
        "MATCH (n:Person) RETURN n.rank AS rank ORDER BY rank ASC",
        ExecutionOptions::default(),
    );
    assert_eq!(result.len(), 1_300);
    for (index, row) in result.iter().enumerate() {
        assert_eq!(row, &vec![Value::Integer(index as i64)]);
    }
}

#[test]
fn fixed_path_binding_materializes_visible_nodes_and_relationships() {
    let fixture = fixture();
    let result = rows(
        &fixture.connection,
        "MATCH p=(a:Person)-[:KNOWS]->(b:Person) RETURN p ORDER BY a.name",
        ExecutionOptions::default(),
    );
    assert_eq!(result.len(), 2);
    let Value::Path(first) = &result[0][0] else {
        panic!("path binding must return a Path value");
    };
    assert_eq!(first.nodes.len(), 2);
    assert_eq!(first.relationships.len(), 1);
    assert_eq!(
        first.nodes[0].properties["name"],
        Value::String("Alice".to_owned())
    );
    assert_eq!(
        first.nodes[1].properties["name"],
        Value::String("Bob".to_owned())
    );
}

#[test]
fn planner_statistics_choose_most_selective_label_scan() {
    let connection = fresh_storage();
    let root = branch_head(&connection, "main").expect("main");
    let person = intern_label(&connection, "Person").expect("Person");
    let rare = intern_label(&connection, "Rare").expect("Rare");
    let mut layer = LayerBuilder::default();
    for index in 0..2 {
        let node = allocate_node_id(&connection).expect("node");
        layer.add_node(node).expect("add node");
        layer.add_label(node, person).expect("Person");
        if index == 0 {
            layer.add_label(node, rare).expect("Rare");
        }
    }
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("stats", 1),
    )
    .expect("commit");
    create_checkpoint(&connection, commit).expect("checkpoint");
    let mut overlay = LayerBuilder::default();
    for _ in 0..8 {
        let node = allocate_node_id(&connection).expect("overlay node");
        overlay.add_node(node).expect("add overlay node");
        overlay.add_label(node, person).expect("overlay Person");
    }
    commit_layer(
        &connection,
        "main",
        commit,
        None,
        &overlay,
        &metadata("stats overlay", 2),
    )
    .expect("overlay commit");

    let prepared = prepare(
        &connection,
        "MATCH (n:Person:Rare) RETURN n",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare");
    assert_eq!(prepared.statistics.node_count, 10);
    assert_eq!(prepared.statistics.label_selectivity(person), Some(1.0));
    assert_eq!(prepared.statistics.label_selectivity(rare), Some(0.1));
    assert!(matches!(
        prepared.logical.operators.first(),
        Some(LogicalOperator::LabelScan { label, .. }) if label == "Rare"
    ));
}

#[test]
fn missing_derived_statistics_falls_back_without_scanning_graph_rows() {
    let fixture = fixture();
    fixture
        .connection
        .execute("UPDATE _lithograph_checkpoints SET metadata = NULL", [])
        .expect("remove derived statistics");
    fixture
        .connection
        .execute_batch("DROP TABLE _lithograph_cp_nodes")
        .expect("remove graph rows used by a full scan");

    let prepared = prepare(
        &fixture.connection,
        "EXPLAIN MATCH (n:Person) RETURN n",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("planning without derived statistics must not scan graph rows");
    assert_eq!(prepared.statistics.node_count, 0);
    let mut cursor = QueryCursor::new(prepared);
    let batch = cursor
        .next_batch(&fixture.connection, 1)
        .expect("EXPLAIN must not execute graph access");
    assert!(batch.done);
    assert_eq!(batch.rows.len(), 1);
}

#[test]
fn adjacency_execution_crosses_multiple_pages_and_deduplicates_self_loops() {
    let connection = fresh_storage();
    let root = branch_head(&connection, "main").expect("main");
    let hub = intern_label(&connection, "Hub").expect("Hub");
    let knows = intern_relationship_type(&connection, "KNOWS").expect("KNOWS");
    let center = allocate_node_id(&connection).expect("center");
    let mut layer = LayerBuilder::default();
    layer.add_node(center).expect("center node");
    layer.add_label(center, hub).expect("Hub");
    for _ in 0..600 {
        let target = allocate_node_id(&connection).expect("target");
        layer.add_node(target).expect("target node");
        let relationship = allocate_relationship_id(&connection).expect("relationship");
        layer
            .add_relationship(RelationshipRecord {
                id: relationship,
                source: center,
                type_id: knows,
                target,
            })
            .expect("relationship");
    }
    let self_loop = allocate_relationship_id(&connection).expect("self-loop");
    layer
        .add_relationship(RelationshipRecord {
            id: self_loop,
            source: center,
            type_id: knows,
            target: center,
        })
        .expect("self-loop relationship");
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("adjacency", 1),
    )
    .expect("commit");
    create_checkpoint(&connection, commit).expect("checkpoint");

    let directed = rows(
        &connection,
        "MATCH (h:Hub)-[:KNOWS]->(n) RETURN count(n) AS total",
        ExecutionOptions::default(),
    );
    assert_eq!(directed, vec![vec![Value::Integer(601)]]);
    let undirected = rows(
        &connection,
        "MATCH (h:Hub)-[:KNOWS]-(n) RETURN count(n) AS total",
        ExecutionOptions::default(),
    );
    assert_eq!(undirected, vec![vec![Value::Integer(601)]]);
}

#[test]
fn historical_graph_view_uses_target_snapshot_membership() {
    let connection = fresh_storage();
    let root = branch_head(&connection, "main").expect("main");
    let secret = intern_label(&connection, "Secret").expect("Secret");
    let rel_type = intern_relationship_type(&connection, "LINK").expect("LINK");
    let source = allocate_node_id(&connection).expect("source");
    let target = allocate_node_id(&connection).expect("target");
    let relationship = allocate_relationship_id(&connection).expect("relationship");
    let mut first = LayerBuilder::default();
    first.add_node(source).expect("source node");
    first.add_node(target).expect("target node");
    first.add_label(target, secret).expect("Secret");
    first
        .add_relationship(RelationshipRecord {
            id: relationship,
            source,
            type_id: rel_type,
            target,
        })
        .expect("relationship");
    let first_commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &first,
        &metadata("hidden", 1),
    )
    .expect("first commit");
    create_checkpoint(&connection, first_commit).expect("checkpoint");

    let mut second = LayerBuilder::default();
    second.remove_label(target, secret).expect("remove Secret");
    let _second_commit = commit_layer(
        &connection,
        "main",
        first_commit,
        None,
        &second,
        &metadata("visible", 2),
    )
    .expect("second commit");

    let current_options =
        ExecutionOptions::parse_text(r#"{"graphView":{"excludeAnyLabels":["Secret"]}}"#)
            .expect("current graph view");
    assert_eq!(
        rows(
            &connection,
            "MATCH (a)-[:LINK]->(b) RETURN count(b) AS total",
            current_options
        ),
        vec![vec![Value::Integer(1)]]
    );

    let historical_options = ExecutionOptions::parse_text(&format!(
        "{{\"at\":\"commit/{}\",\"graphView\":{{\"excludeAnyLabels\":[\"Secret\"]}}}}",
        first_commit.to_hex()
    ))
    .expect("historical graph view");
    assert_eq!(
        rows(
            &connection,
            "MATCH (a)-[:LINK]->(b) RETURN count(b) AS total",
            historical_options
        ),
        vec![vec![Value::Integer(0)]]
    );
}
