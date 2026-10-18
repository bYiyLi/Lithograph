use std::cell::Cell;
use std::collections::BTreeSet;

use super::*;
use crate::query::{ExecutionOptions, QueryCursor, QueryResources, prepare};

#[test]
fn controlled_keys_keep_existing_property_equality_semantics() {
    let values = [
        PropertyValue::Integer(i64::MAX),
        PropertyValue::Float(1.0),
        PropertyValue::Float(-0.0),
        PropertyValue::Float(f64::NAN),
        PropertyValue::String("人物`.".repeat(30_000)),
        PropertyValue::List(vec![PropertyValue::Integer(1), PropertyValue::Float(2.0)]),
        PropertyValue::List(vec![PropertyValue::Float(f64::NAN)]),
        PropertyValue::Point(storage::PointValue {
            crs: 1,
            coordinates: vec![-0.0, 2.0],
        }),
        PropertyValue::Vector(storage::VectorValue {
            coordinate_type: VectorCoordinateType::I16,
            dimension: 2,
            packed: vec![1, 0, 2, 0],
        }),
        PropertyValue::Vector(storage::VectorValue {
            coordinate_type: VectorCoordinateType::F32,
            dimension: 2,
            packed: [-0.0_f32, 2.0]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect(),
        }),
        PropertyValue::Vector(storage::VectorValue {
            coordinate_type: VectorCoordinateType::F64,
            dimension: 2,
            packed: [1.0_f64, f64::NAN]
                .into_iter()
                .flat_map(f64::to_le_bytes)
                .collect(),
        }),
    ];
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    for value in values {
        let key = equality_key(&value, &control).unwrap();
        let expected = property_equality_key(&value).unwrap();
        assert_eq!(key.as_ref().map(|key| &key.bytes), expected.as_ref());
        drop(key);
        assert_eq!(resources.used(), 0);
    }
}

#[test]
fn canonical_key_refuses_before_copying_and_releases_failed_growth() {
    let resources = QueryResources::new(Some(1_024));
    let control = ExecutionControl::new(&resources, &|| false);
    let value = PropertyValue::String("x".repeat(64 * 1024));
    let error = equality_key(&value, &control).err().unwrap();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.used(), 0);
    let mut key = KeyBytes::new(&control).unwrap();
    key.append(b"old", &control).unwrap();
    let before = resources.used();
    assert!(key.append(&[0; 1_024], &control).is_err());
    assert_eq!(&key.bytes, b"old");
    assert_eq!(resources.used(), before);
    drop(key);
    assert_eq!(resources.used(), 0);
}

#[test]
fn borrowed_list_type_validation_checks_before_256_items() {
    let resources = QueryResources::default();
    let calls = Cell::new(0);
    let interrupt = || {
        calls.set(calls.get() + 1);
        true
    };
    let control = ExecutionControl::new(&resources, &interrupt);
    let rule = PropertyType::List {
        element: Box::new(PropertyType::Integer),
    };
    let value = PropertyValue::List(vec![PropertyValue::Integer(1); 10_000]);
    let error = accepts(&rule, &value, &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(calls.get(), 1);
    assert_eq!(resources.used(), 0);
}

#[test]
fn changed_definition_comparison_cancels_inside_large_type_rule() {
    let resources = QueryResources::default();
    let rule = PropertyRule {
        property_type: PropertyType::Union {
            members: vec![PropertyType::Integer; 1_024],
        },
        required: true,
    };
    let left = GraphNodeType {
        label: "Thing".to_owned(),
        implied_labels: BTreeSet::from(["Entity".to_owned()]),
        properties: BTreeMap::from([("p".to_owned(), rule)]),
    };
    let mut right = left.clone();
    let control = ExecutionControl::new(&resources, &|| false);
    assert!(same_node(&left, &right, &control).unwrap());
    let checks = Cell::new(0);
    let interrupt = || {
        checks.set(checks.get() + 1);
        true
    };
    let control = ExecutionControl::new(&resources, &interrupt);
    assert_eq!(
        same_node(&left, &right, &control).unwrap_err().kind,
        QueryErrorKind::Interrupted
    );
    assert_eq!(checks.get(), 1);
    right.properties.get_mut("p").unwrap().required = false;
    assert!(!same_node(&left, &right, &ExecutionControl::new(&resources, &|| false)).unwrap());
    assert_eq!(resources.used(), 0);
}

pub(super) fn fixture(query: &str) -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").unwrap();
    storage::create_storage_schema(&connection).unwrap();
    storage::initialize_root(&connection).unwrap();
    let prepared = prepare(
        &connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    let mut cursor = QueryCursor::new(prepared);
    while !cursor.next_batch(&connection, 100).unwrap().done {}
    drop(cursor);
    connection
}

fn uniqueness_schema() -> SchemaState {
    let definition = ConstraintDefinition {
        name: "unique_p".to_owned(),
        target: SchemaTarget::Node {
            label: "Thing".to_owned(),
        },
        properties: vec!["p".to_owned()],
        kind: ConstraintDefinitionKind::Unique,
        origin: None,
    };
    SchemaState {
        constraints: BTreeMap::from([(definition.name.clone(), definition)]),
        ..SchemaState::default()
    }
}

fn scratch_count(connection: &Connection) -> i64 {
    connection
        .query_row(
            "SELECT count(*) FROM temp._lithograph_constraint_validation_seen",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn uniqueness_conflict_keeps_kind_and_cleans_scratch_keys() {
    let connection = fixture("CREATE (:Thing {p:1}), (:Thing {p:1.0})");
    let resources = QueryResources::new(Some(16 * 1024 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let commit = storage::branch_head(&connection, "main").unwrap();
    let snapshot =
        crate::query::graph::resolve_snapshot_with_control(&connection, commit, &control).unwrap();
    let retained = resources.used();
    let error = validate_schema_transition_controlled(
        &connection,
        &SchemaState::default(),
        &uniqueness_schema(),
        &snapshot,
        &control,
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(scratch_count(&connection), 0);
    assert_eq!(resources.used(), retained);
    drop(snapshot);
    assert_eq!(resources.used(), 0);
}

#[test]
fn uniqueness_budget_failure_cleans_scratch_and_observed_values() {
    let connection = fixture("CREATE (:Thing {p:range(1,10000)})");
    let limit = 64 * 1024 * 1024;
    let resources = QueryResources::new(Some(limit));
    let control = ExecutionControl::new(&resources, &|| false);
    let commit = storage::branch_head(&connection, "main").unwrap();
    let snapshot =
        crate::query::graph::resolve_snapshot_with_control(&connection, commit, &control).unwrap();
    let retained = resources.used();
    let hold = resources.reserve(limit - retained - 128 * 1024).unwrap();
    let error = validate_schema_transition_controlled(
        &connection,
        &SchemaState::default(),
        &uniqueness_schema(),
        &snapshot,
        &control,
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert_eq!(scratch_count(&connection), 0);
    drop(hold);
    assert_eq!(resources.used(), retained);
    drop(snapshot);
    assert_eq!(resources.used(), 0);
}
