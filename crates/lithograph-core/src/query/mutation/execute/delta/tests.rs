use std::cell::Cell;

use super::*;

fn long_property() -> PropertyValue {
    PropertyValue::List((0..10_000).map(PropertyValue::Integer).collect())
}

#[test]
fn final_layer_property_clone_observes_cancellation() {
    let mut delta = DeltaBuilder::default();
    delta.properties.insert(
        (OwnerKind::Node, 1, 1),
        Slot {
            before: None,
            current: Some(long_property()),
        },
    );
    let resources = QueryResources::new(None);
    let checks = Cell::new(0);
    let interrupted = || {
        checks.set(checks.get() + 1);
        checks.get() >= 10
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = delta
        .layer(&control)
        .expect_err("cancel checked property copy while finalizing layer");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert_eq!(checks.get(), 10);
    assert_eq!(resources.used(), 0);
}

#[test]
fn final_counters_property_encoding_observes_cancellation_and_releases_workspace() {
    let value = long_property();
    let mut delta = DeltaBuilder::default();
    delta.properties.insert(
        (OwnerKind::Node, 1, 1),
        Slot {
            before: Some(value.clone()),
            current: Some(value),
        },
    );
    let resources = QueryResources::new(Some(64 * 1024 * 1024));
    let encoding_checks = Cell::new(0);
    let interrupted = || {
        if resources.used() == 0 {
            return false;
        }
        encoding_checks.set(encoding_checks.get() + 1);
        encoding_checks.get() >= 20
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = delta
        .counters(&control)
        .expect_err("cancel inside canonical encoding after its workspace is reserved");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(encoding_checks.get(), 20);
    assert_eq!(resources.used(), 0);
    assert!(
        matches!(delta.properties[&(OwnerKind::Node, 1, 1)].current.as_ref(), Some(PropertyValue::List(values)) if values.len() == 10_000)
    );
}
