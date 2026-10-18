use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};

use rusqlite::Connection;

use super::*;
use crate::query::resources::QueryResources;
use crate::storage::{create_storage_schema, initialize_root};

fn connection() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").unwrap();
    create_storage_schema(&connection).unwrap();
    initialize_root(&connection).unwrap();
    connection
}

fn range_expression(count: i64) -> Expr {
    Expr::Function {
        name: "range".into(),
        args: vec![
            Expr::Literal(Value::Integer(1)),
            Expr::Literal(Value::Integer(count)),
        ],
        distinct: false,
        star: false,
    }
}

fn evaluate_range(resources: &QueryResources, count: i64) -> QueryResult<EvaluatedValue> {
    let connection = connection();
    let root = crate::storage::branch_head(&connection, "main").unwrap();
    let snapshot = Snapshot::resolve(&connection, root).unwrap();
    let interrupted = || false;
    let control = ExecutionControl::new(resources, &interrupted);
    evaluate_with_access_control(
        &range_expression(count),
        &snapshot,
        &BindingRow::default(),
        &BTreeMap::new(),
        &PropertyRules::default(),
        &control,
    )
}

#[test]
fn returned_expression_keeps_its_lease_across_concurrent_worker_handoff() {
    let unlimited = QueryResources::default();
    let output = evaluate_range(&unlimited, 100).unwrap();
    let result_bytes = output.reservation.bytes();
    let peak = unlimited.peak();
    drop(output);
    let limit = peak.max(result_bytes * 2 - 1);
    let resources = QueryResources::new(Some(limit));
    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        let worker_resources = resources.clone();
        let worker_barrier = barrier.clone();
        scope.spawn(move || {
            let output = evaluate_range(&worker_resources, 100).unwrap();
            assert_eq!(worker_resources.used(), output.reservation.bytes());
            worker_barrier.wait();
            worker_barrier.wait();
            drop(output);
        });
        barrier.wait();
        let error = evaluate_range(&resources, 100)
            .expect_err("simultaneously live output must exceed shared budget");
        assert_eq!(error.kind, QueryErrorKind::Resource);
        barrier.wait();
    });
    assert_eq!(resources.used(), 0);
    let output = evaluate_range(&resources, 100).unwrap();
    drop(output);
    assert_eq!(resources.used(), 0);
}

#[test]
fn function_intermediate_is_released_without_releasing_returned_result() {
    let resources = QueryResources::new(Some(256 * 1024 * 1024));
    let connection = connection();
    let root = crate::storage::branch_head(&connection, "main").unwrap();
    let snapshot = Snapshot::resolve(&connection, root).unwrap();
    let interrupted = || false;
    let control = ExecutionControl::new(&resources, &interrupted);
    let expression = Expr::Function {
        name: "size".into(),
        args: vec![range_expression(500_000)],
        distinct: false,
        star: false,
    };
    let result = evaluate_with_access_control(
        &expression,
        &snapshot,
        &BindingRow::default(),
        &BTreeMap::new(),
        &PropertyRules::default(),
        &control,
    )
    .unwrap();
    assert_eq!(result.value, Value::Integer(500_000));
    assert_eq!(resources.used(), result.reservation.bytes());
    assert!(resources.used() < 1024);
    assert!(resources.peak() > 16 * 1024 * 1024);
    drop(result);
    assert_eq!(resources.used(), 0);
}

fn path_with_nodes(count: usize) -> Value {
    Value::Path(PathValue {
        nodes: (0..count)
            .map(|_| crate::cypher::NodeValue {
                element_id: "n:1".to_owned(),
                labels: Vec::new(),
                properties: BTreeMap::new(),
            })
            .collect(),
        relationships: Vec::new(),
    })
}

#[test]
fn path_binding_conversion_checks_cancellation_before_the_256th_element() {
    let connection = connection();
    let root = crate::storage::branch_head(&connection, "main").unwrap();
    let snapshot = Snapshot::resolve(&connection, root).unwrap();
    let resources = QueryResources::default();
    let polls = std::cell::Cell::new(0);
    let interrupted = || {
        polls.set(polls.get() + 1);
        polls.get() >= 3
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let Value::Path(mut path) = path_with_nodes(4_096) else {
        unreachable!();
    };
    // Entry and output-reserve polls succeed. The next poll must precede ID
    // decoding at element 256, so this invalid ID must never be observed.
    path.nodes[255].element_id = "invalid".to_owned();
    let error = binding_from_value_controlled(&snapshot, Value::Path(path), &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(polls.get(), 3);
    assert_eq!(resources.used(), 0);
}

#[test]
fn path_binding_conversion_reserves_simultaneously_live_output_arrays() {
    let connection = connection();
    let root = crate::storage::branch_head(&connection, "main").unwrap();
    let snapshot = Snapshot::resolve(&connection, root).unwrap();
    let value = path_with_nodes(32);
    let input_bytes = crate::query::resources::value_bytes(&value).unwrap();
    let output_bytes = allocation_bytes::<i64>(32).unwrap();
    let resources = QueryResources::new(Some(input_bytes + output_bytes));
    let memory = resources.reserve(input_bytes).unwrap();
    let never = || false;
    let control = ExecutionControl::new(&resources, &never);
    let binding = binding_from_value_controlled(&snapshot, value, &control).unwrap();
    assert_eq!(resources.peak(), input_bytes + output_bytes);
    assert_eq!(resources.used(), input_bytes);
    assert!(matches!(&binding, BindingValue::Path { nodes, .. } if nodes.as_slice() == [1; 32]));
    drop(binding);
    drop(memory);
    assert_eq!(resources.used(), 0);
}
