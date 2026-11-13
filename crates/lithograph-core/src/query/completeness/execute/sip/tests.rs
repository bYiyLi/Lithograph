use std::cell::Cell;

use rusqlite::Connection;

use super::*;
use crate::cypher::{AstKind, ClauseKind};
use crate::query::QueryResources;

fn step_and_row(values: Vec<Value>) -> (MatchStep, BindingRow) {
    let connection = Connection::open_in_memory().expect("empty SQLite for structural lowering");
    let parsed = crate::cypher::parse("MATCH(a) WHERE elementId(a) IN ids RETURN a")
        .expect("parse the exact SIP predicate");
    let clause = parsed
        .root
        .descendants()
        .find(|node| node.kind == AstKind::Clause(ClauseKind::Match))
        .expect("MATCH clause");
    // No labels/types occur in the pattern, so this lowerer reads no graph tables.
    let step = crate::query::plan::lower_match(&connection, clause, false)
        .expect("lower the plain Node pattern");
    let mut row = BindingRow::default();
    row.insert("ids".to_owned(), BindingValue::Scalar(Value::List(values)));
    (step, row)
}

fn three_non_null_values() -> Vec<Value> {
    vec![
        Value::String("n:3".to_owned()),
        Value::Null,
        Value::String("n:1".to_owned()),
        Value::String("n:3".to_owned()),
    ]
}

#[test]
fn complete_mask_keeps_its_capacity_claim_through_sort_and_dedup() {
    let (step, row) = step_and_row(three_non_null_values());
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);

    let mask = node_ids(&step, &row, &control)
        .expect("valid typed list")
        .expect("complete mask");

    assert_eq!(mask.0, vec![1, 3]);
    assert_eq!(mask.0.capacity(), 3);
    assert_eq!(mask.1.bytes(), 3 * 8);
    assert_eq!(resources.used(), mask.1.bytes());
    assert_eq!(resources.used(), mask.0.capacity() as u64 * 8);
    assert_eq!(resources.peak(), 3 * 8);
    drop(mask);
    assert_eq!(resources.used(), 0);
}

#[test]
fn streaming_mask_uses_the_same_complete_validation_and_budget_claim() {
    let (step, row) = step_and_row(three_non_null_values());
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mask = streaming_node_ids(&step, &row, &control)
        .unwrap()
        .expect("fresh plain streaming Node can reuse the completed mask");
    assert_eq!(mask.0, vec![1, 3]);
    assert_eq!(mask.1.bytes(), 24);
    assert_eq!(resources.used(), 24);
    drop(mask);
    assert_eq!(resources.used(), 0);

    let (step, mixed) = step_and_row(vec![Value::Integer(1), Value::String("n:3".into())]);
    assert!(
        streaming_node_ids(&step, &mixed, &control)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        resources.used(),
        0,
        "mixed families keep original IN evaluation"
    );
}

#[test]
fn streaming_mask_never_narrows_an_existing_binding_optional_or_path_pattern() {
    let (step, mut row) = step_and_row(three_non_null_values());
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    row.insert("a".into(), BindingValue::Scalar(Value::Integer(1)));
    assert!(streaming_node_ids(&step, &row, &control).unwrap().is_none());
    let (_, row) = step_and_row(three_non_null_values());
    let mut optional = step.clone();
    optional.optional = true;
    assert!(
        streaming_node_ids(&optional, &row, &control)
            .unwrap()
            .is_none()
    );
    let mut path = step.clone();
    path.parts[0].path_variable = Some("p".into());
    assert!(streaming_node_ids(&path, &row, &control).unwrap().is_none());
    let mut multiple = step.clone();
    multiple.parts.push(step.parts[0].clone());
    assert!(
        streaming_node_ids(&multiple, &row, &control)
            .unwrap()
            .is_none()
    );
    assert_eq!(resources.used(), 0);
}

#[test]
fn insufficient_mask_budget_returns_optional_fallback_without_a_live_claim() {
    let (step, row) = step_and_row(three_non_null_values());
    let resources = QueryResources::new(Some(16));
    let control = ExecutionControl::new(&resources, &|| false);

    let mask = node_ids(&step, &row, &control)
        .expect("optional mask refusal must not become RESOURCE_ERROR");

    assert!(mask.is_none());
    assert_eq!(resources.used(), 0);
    assert!(resources.peak() <= 16);
}

#[test]
fn cancellation_after_reservation_releases_the_real_in_progress_mask() {
    let count = 768;
    let (step, row) = step_and_row(
        (1..=count)
            .map(|id| Value::String(format!("n:{id}")))
            .collect(),
    );
    let resources = QueryResources::default();
    let observed_live = Cell::new(0);
    let seen_checks = Cell::new(0);
    let interrupted = || {
        seen_checks.set(seen_checks.get() + 1);
        let live = resources.used();
        if live > 0 {
            observed_live.set(live);
            true
        } else {
            false
        }
    };
    let control = ExecutionControl::new(&resources, &interrupted);

    let error =
        node_ids(&step, &row, &control).expect_err("cancel while filling an already reserved mask");

    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert!(
        seen_checks.get() >= 5,
        "validation and reserve checks must pass first"
    );
    assert_eq!(observed_live.get(), count * 8);
    assert_eq!(resources.peak(), count * 8);
    assert_eq!(resources.used(), 0);
}
