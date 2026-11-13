use crate::cypher::Value;
use crate::performance::{reset, set_enabled, snapshot};
use crate::query::expression::{
    BindingRow, BindingValue, LoadCsvContext, binding_row_bytes_controlled,
    clone_binding_row_unreserved_controlled, predicate,
};
use crate::query::{ExecutionControl, QueryErrorKind, QueryResources};
use std::cell::Cell;

struct Collection;

impl Collection {
    fn new() -> Self {
        set_enabled(false);
        reset();
        Self
    }
}

impl Drop for Collection {
    fn drop(&mut self) {
        set_enabled(false);
        reset();
    }
}

fn row() -> BindingRow {
    let mut row = BindingRow::default();
    for id in 0..300 {
        row.insert(format!("node_{id}"), BindingValue::Node(id));
    }
    row.insert(
        "payload".into(),
        BindingValue::Scalar(Value::String("λ".repeat(1024))),
    );
    row.used_relationships.insert(17);
    row.load_csv_context = Some(LoadCsvContext {
        file: Some("synthetic.csv".into()),
        line: Some(3),
    });
    row
}

#[test]
fn only_successful_controlled_copies_count_the_copied_reservation_footprint() {
    let _collection = Collection::new();
    let original = row();
    let disabled = clone_binding_row_unreserved_controlled(&original, None).unwrap();
    assert_eq!(disabled, original);
    assert_eq!(snapshot().binding_row_clones, 0);

    set_enabled(true);
    let copied = clone_binding_row_unreserved_controlled(&original, None).unwrap();
    assert_eq!(copied, original);
    let expected = binding_row_bytes_controlled(&copied, None).unwrap();
    assert_eq!(snapshot().binding_row_clones, 1);
    assert_eq!(snapshot().binding_row_clone_accounted_bytes, expected);

    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| true);
    let before = snapshot();
    let error = clone_binding_row_unreserved_controlled(&original, Some(&control)).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(
        snapshot(),
        before,
        "a failed partial copy is not a successful clone"
    );
    assert_eq!(resources.used(), 0);
}

fn controlled_copy_checks(enabled: bool, original: &BindingRow) -> (u64, u64) {
    set_enabled(enabled);
    reset();
    let calls = Cell::new(0_u64);
    let interrupted = || {
        calls.set(calls.get() + 1);
        false
    };
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &interrupted);
    let memory = control
        .reserve(binding_row_bytes_controlled(original, None).unwrap())
        .unwrap();
    let copied = clone_binding_row_unreserved_controlled(original, Some(&control)).unwrap();
    assert_eq!(&copied, original);
    let peak = resources.peak();
    drop((copied, memory));
    assert_eq!(resources.used(), 0);
    (calls.get(), peak)
}

#[test]
fn footprint_diagnostics_add_no_execution_control_checks_or_reservations() {
    let _collection = Collection::new();
    let original = row();
    let disabled = controlled_copy_checks(false, &original);
    assert_eq!(snapshot().binding_row_clones, 0);
    let enabled = controlled_copy_checks(true, &original);
    assert!(
        disabled.0 > 0,
        "the real cooperative clone checks cancellation"
    );
    assert_eq!(enabled, disabled);
    assert_eq!(snapshot().binding_row_clones, 1);
}

#[test]
fn predicate_counter_records_conversion_attempts_including_null_and_type_errors() {
    let _collection = Collection::new();
    assert!(predicate(Value::Boolean(true)).unwrap());
    assert_eq!(snapshot().predicate_conversions, 0);

    set_enabled(true);
    assert!(predicate(Value::Boolean(true)).unwrap());
    assert!(!predicate(Value::Boolean(false)).unwrap());
    assert!(!predicate(Value::Null).unwrap());
    assert_eq!(
        predicate(Value::Integer(7)).unwrap_err().kind,
        QueryErrorKind::Type
    );
    assert_eq!(snapshot().predicate_conversions, 4);
    assert_eq!(snapshot().binding_row_clones, 0);
}

#[test]
fn ordinary_clone_and_cooperative_copy_use_the_same_successful_copy_counter() {
    let _collection = Collection::new();
    let original = row();
    let mut ordinary = original.clone();
    assert_eq!(ordinary, original);
    assert_eq!(snapshot().binding_row_clones, 0);
    ordinary.values.clear();
    ordinary.order.clear();
    ordinary.used_relationships.clear();
    ordinary.load_csv_context = None;
    assert!(!original.values.is_empty() && !original.order.is_empty());
    assert!(!original.used_relationships.is_empty() && original.load_csv_context.is_some());

    set_enabled(true);
    let copied = original.clone();
    let clone_bytes = binding_row_bytes_controlled(&copied, None).unwrap();
    assert_eq!(copied, original);
    assert_eq!(snapshot().binding_row_clones, 1);
    assert_eq!(snapshot().binding_row_clone_accounted_bytes, clone_bytes);
    let controlled = clone_binding_row_unreserved_controlled(&original, None).unwrap();
    let controlled_bytes = binding_row_bytes_controlled(&controlled, None).unwrap();
    assert_eq!(controlled, copied);
    assert_eq!(
        snapshot().binding_row_clones,
        2,
        "no nested double-counting"
    );
    assert_eq!(
        snapshot().binding_row_clone_accounted_bytes,
        clone_bytes + controlled_bytes
    );
}
