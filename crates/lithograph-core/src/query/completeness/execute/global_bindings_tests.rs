use std::cell::Cell;

use super::*;
use crate::query::resources::QueryResources;

fn globals() -> BindingRow {
    let mut globals = BindingRow::default();
    globals.insert(
        "imported".into(),
        BindingValue::Scalar(Value::List((0..10_000).map(Value::Integer).collect())),
    );
    globals
}

#[test]
fn global_restore_rejects_each_copy_before_allocating_it() {
    let resources = QueryResources::new(Some(1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let error = restore_global_bindings(RowSet::seed(), &globals(), &control)
        .err()
        .expect("large imported value cannot fit its copied row");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert!(resources.peak() <= 1024);
    assert_eq!(resources.used(), 0);
}

#[test]
fn global_restore_copy_checks_cancellation_and_releases_memory() {
    let resources = QueryResources::default();
    let checks = Cell::new(0);
    let interrupted = || {
        checks.set(checks.get() + 1);
        checks.get() >= 8
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = restore_global_bindings(RowSet::seed(), &globals(), &control)
        .err()
        .expect("cancel inside imported value traversal");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(checks.get(), 8);
    assert_eq!(resources.used(), 0);
}

#[test]
fn global_restore_retains_each_output_copy_until_rows_are_dropped() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let globals = globals();
    let rows = restore_global_bindings(
        RowSet {
            columns: vec!["local".into()],
            rows: vec![BindingRow::default(), BindingRow::default()],
            memory: None,
        },
        &globals,
        &control,
    )
    .expect("restore scoped CALL variables");
    assert_eq!(rows.columns, ["local", "imported"]);
    for row in &rows.rows {
        assert_eq!(row.values["imported"], globals.values["imported"]);
    }
    let value_bytes = expression::binding_value_bytes(&globals.values["imported"], None).unwrap();
    assert!(resources.used() >= value_bytes * 2);
    drop(rows);
    assert_eq!(resources.used(), 0);
}
