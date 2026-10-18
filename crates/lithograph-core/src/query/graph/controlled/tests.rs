use std::cell::Cell;

use super::*;
use crate::query::QueryErrorKind;
use crate::query::graph::ResolvedGraphView;

#[test]
fn property_string_is_refused_before_a_runtime_copy_is_created() {
    let property = storage::PropertyValue::String("a".repeat(65_536));
    let resources = QueryResources::new(Some(16_384));
    let never = || false;
    let control = ExecutionControl::new(&resources, &never);
    assert_eq!(
        borrowed_property(&property, &control).unwrap_err().kind,
        QueryErrorKind::Resource
    );
    assert_eq!(resources.used(), 0);
    assert_eq!(resources.peak(), 0);
}

#[test]
fn converted_list_retains_its_values_until_the_owner_is_released() {
    let property =
        storage::PropertyValue::List(vec![
            storage::PropertyValue::String("sample".to_owned());
            16
        ]);
    let resources = QueryResources::new(Some(65_536));
    let never = || false;
    let control = ExecutionControl::new(&resources, &never);
    let (value, memory) = borrowed_property(&property, &control).unwrap();
    assert!(memory.bytes() >= value_bytes_controlled(&value, &control).unwrap());
    assert_eq!(resources.used(), memory.bytes());
    drop(value);
    drop(memory);
    assert_eq!(resources.used(), 0);
}

#[test]
fn long_text_copy_and_vector_conversion_check_cancellation() {
    let property = storage::PropertyValue::String("文".repeat(100_000));
    let resources = QueryResources::default();
    let calls = Cell::new(0);
    let interrupted = || {
        calls.set(calls.get() + 1);
        calls.get() >= 3
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    assert_eq!(
        borrowed_property(&property, &control).unwrap_err().kind,
        QueryErrorKind::Interrupted
    );
    assert_eq!(resources.used(), 0);

    let vector = storage::PropertyValue::Vector(storage::VectorValue {
        coordinate_type: storage::VectorCoordinateType::I8,
        dimension: 4_096,
        packed: vec![1; 4_096],
    });
    let calls = Cell::new(0);
    let interrupted = || {
        calls.set(calls.get() + 1);
        calls.get() >= 3
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    assert_eq!(
        borrowed_property(&vector, &control).unwrap_err().kind,
        QueryErrorKind::Interrupted
    );
    assert_eq!(resources.used(), 0);
}

fn fixture() -> rusqlite::Connection {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1),magic TEXT NOT NULL,database_id TEXT NOT NULL,storage_format INTEGER NOT NULL);").unwrap();
    storage::create_storage_schema(&connection).unwrap();
    storage::initialize_root(&connection).unwrap();
    let parameters = BTreeMap::from([("text".to_owned(), Value::String("x".repeat(65_536)))]);
    let prepared = crate::query::prepare(
        &connection,
        "CREATE (:人物 {secret:$text, visible:1}) FINISH",
        parameters,
        crate::query::ExecutionOptions::default(),
    )
    .unwrap();
    let mut cursor = crate::query::QueryCursor::new(prepared);
    loop {
        if cursor.next_batch(&connection, 1).unwrap().done {
            cursor.complete(&connection).unwrap();
            break;
        }
    }
    connection
}

#[test]
fn borrowed_graph_materialization_enforces_budget_before_visible_property_copy() {
    let connection = fixture();
    let head = storage::branch_head(&connection, "main").unwrap();
    let snapshot = Snapshot::resolve(&connection, head).unwrap();
    let mut node = None;
    snapshot
        .visit_nodes(|id| {
            node = Some(id);
            Ok(())
        })
        .unwrap();
    let resources = QueryResources::new(Some(16_384));
    let never = || false;
    let control = ExecutionControl::new(&resources, &never);
    let error = materialize_node_with_control(
        &snapshot,
        node.unwrap(),
        &PropertyRules::default(),
        &control,
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.used(), 0);
    assert!(resources.peak() < 16_384);

    let options = crate::query::ExecutionOptions::parse_text(
        r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#,
    )
    .unwrap();
    let (value, memory) = materialize_node_with_control(
        &snapshot,
        node.unwrap(),
        &options.access.read.query,
        &control,
    )
    .unwrap();
    assert_eq!(value.labels, vec!["人物"]);
    assert_eq!(
        value.properties,
        BTreeMap::from([("visible".to_owned(), Value::Integer(1))])
    );
    drop(value);
    drop(memory);
    assert_eq!(resources.used(), 0);
}

fn label_layer() -> (storage::LayerBuilder, RelationshipRecord) {
    let relationship = RelationshipRecord {
        id: 1,
        source: 1,
        type_id: 1,
        target: 2,
    };
    let mut layer = storage::LayerBuilder::default();
    for node in [1, 2] {
        layer.add_node(node).unwrap();
        for label in 1..=4_096 {
            layer.add_label(node, label).unwrap();
        }
    }
    layer.add_relationship(relationship).unwrap();
    (layer, relationship)
}

#[test]
fn graph_view_label_visitors_use_the_current_callback_after_snapshot_resolution() {
    let connection = fixture();
    let head = storage::branch_head(&connection, "main").unwrap();
    let (layer, relationship) = label_layer();
    let resources = QueryResources::default();
    let never = || false;
    let resolving_control = ExecutionControl::new(&resources, &never);
    let snapshot = resolve_snapshot_with_layer_and_schema_controlled(
        &connection,
        head,
        &layer,
        storage::SchemaState::load(&connection, head).unwrap(),
        &resolving_control,
    )
    .unwrap();
    let view = ResolvedGraphView {
        required: vec![1, 4_096],
        ..ResolvedGraphView::default()
    };
    assert!(
        view.visible_node_controlled(&snapshot, 1, &resolving_control)
            .unwrap()
    );
    assert!(
        view.visible_relationship_controlled(&snapshot, relationship, &resolving_control)
            .unwrap()
    );
    for path in 0..3 {
        let polls = Cell::new(0);
        let interrupted = || {
            polls.set(polls.get() + 1);
            polls.get() >= 10
        };
        let control = ExecutionControl::new(&resources, &interrupted);
        let error = match path {
            0 => view.visible_node_controlled(&snapshot, 1, &control),
            1 => view.visible_existing_node_controlled(&snapshot, 1, Some(1), &control),
            _ => view.visible_relationship_controlled(&snapshot, relationship, &control),
        }
        .expect_err("the current callback must interrupt an already resolved Label overlay");
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(polls.get(), 10);
    }
}
