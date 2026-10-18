use std::cell::Cell;

use super::*;

fn connection() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").unwrap();
    storage::create_storage_schema(&connection).unwrap();
    storage::initialize_root(&connection).unwrap();
    connection
}

fn snapshot(connection: &Connection) -> Snapshot<'_> {
    let root = storage::branch_head(connection, "main").unwrap();
    let key = storage::intern_property_key(connection, "items").unwrap();
    let mut snapshot = Snapshot::resolve(connection, root).unwrap();
    let mut layer = LayerBuilder::default();
    layer.add_node(1).unwrap();
    layer
        .set_property(
            OwnerKind::Node,
            1,
            key,
            PropertyValue::List((0..10_000).map(PropertyValue::Integer).collect()),
        )
        .unwrap();
    snapshot.apply_layer_checked(&layer, &|| Ok(())).unwrap();
    snapshot
}

#[test]
fn merge_property_match_reserves_actual_value_before_copying_it() {
    let connection = connection();
    let snapshot = snapshot(&connection);
    let resources = QueryResources::new(Some(1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let error = properties_match(
        &snapshot,
        OwnerKind::Node,
        1,
        &BTreeMap::from([("items".into(), Value::Integer(1))]),
        &Default::default(),
        &control,
    )
    .expect_err("actual MERGE property requires its own conversion memory");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.used(), 0);
    assert_eq!(resources.peak(), 0);
}

#[test]
fn merge_property_match_checks_cancellation_within_actual_list() {
    let connection = connection();
    let snapshot = snapshot(&connection);
    let resources = QueryResources::default();
    let checks = Cell::new(0);
    let interrupted = || {
        checks.set(checks.get() + 1);
        checks.get() >= 12
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let expected = Value::List((0..10_000).map(Value::Integer).collect());
    let error = properties_match(
        &snapshot,
        OwnerKind::Node,
        1,
        &BTreeMap::from([("items".into(), expected)]),
        &Default::default(),
        &control,
    )
    .expect_err("cancel in MERGE property conversion or equality");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(checks.get(), 12);
    assert_eq!(resources.used(), 0);
}

#[test]
fn merge_property_match_keeps_cypher_numeric_list_equality() {
    let connection = connection();
    let snapshot = snapshot(&connection);
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let expected = Value::List(
        (0..10_000)
            .map(|value| Value::Float(value as f64))
            .collect(),
    );
    assert!(
        properties_match(
            &snapshot,
            OwnerKind::Node,
            1,
            &BTreeMap::from([("items".into(), expected)]),
            &Default::default(),
            &control,
        )
        .unwrap()
    );
    assert_eq!(resources.used(), 0);
}

#[test]
fn merge_property_match_does_not_match_absent_or_null_predicates() {
    let connection = connection();
    let mut snapshot = snapshot(&connection);
    storage::intern_property_key(&connection, "known_absent").unwrap();
    let number = storage::intern_property_key(&connection, "number").unwrap();
    let mut layer = LayerBuilder::default();
    layer
        .set_property(OwnerKind::Node, 1, number, PropertyValue::Integer(42))
        .unwrap();
    snapshot.apply_layer_checked(&layer, &|| Ok(())).unwrap();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    for (key, expected, matches) in [
        ("known_absent", Value::Null, false),
        ("known_absent", Value::Integer(42), false),
        ("unknown", Value::Null, false),
        ("unknown", Value::Integer(42), false),
        ("number", Value::Null, false),
        ("number", Value::Float(42.0), true),
        ("number", Value::Float(43.0), false),
    ] {
        assert_eq!(
            properties_match(
                &snapshot,
                OwnerKind::Node,
                1,
                &BTreeMap::from([(key.to_owned(), expected)]),
                &Default::default(),
                &control,
            )
            .unwrap(),
            matches,
            "{key} predicate",
        );
    }
    assert_eq!(resources.used(), 0);
}

#[test]
fn merge_null_pattern_property_is_a_semantic_error_before_any_write() {
    let connection = connection();
    let before = storage::branch_head(&connection, "main").unwrap();
    let prepared = crate::query::prepare(
        &connection,
        "MERGE (:Invalid {value:null}) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    let mut cursor = crate::query::QueryCursor::new(prepared);
    let resources = cursor.resources().clone();
    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("MERGE must reject null pattern properties");
    assert_eq!(error.kind, QueryErrorKind::Semantic);
    assert_eq!(storage::branch_head(&connection, "main").unwrap(), before);
    assert!(
        storage::find_label(&connection, "Invalid")
            .unwrap()
            .is_none()
    );
    assert!(
        storage::find_property_key(&connection, "value")
            .unwrap()
            .is_none()
    );
    assert_eq!(resources.used(), 0);
    assert!(connection.is_autocommit());
}
