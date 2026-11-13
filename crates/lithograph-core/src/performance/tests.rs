use super::*;

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

#[test]
fn counters_are_disabled_by_default_resettable_and_saturating() {
    let _collection = Collection::new();
    assert!(!is_enabled());
    record_binding_row_clone(42);
    record_predicate_conversion();
    assert_eq!(snapshot(), PerformanceCounters::default());

    set_enabled(true);
    let mut counters = snapshot();
    counters.binding_row_clones = u64::MAX;
    counters.binding_row_clone_accounted_bytes = u64::MAX - 1;
    counters.predicate_conversions = u64::MAX;
    COUNTERS.set(counters);
    record_binding_row_clone(42);
    record_predicate_conversion();
    assert_eq!(snapshot().binding_row_clones, u64::MAX);
    assert_eq!(snapshot().binding_row_clone_accounted_bytes, u64::MAX);
    assert_eq!(snapshot().predicate_conversions, u64::MAX);
    reset();
    assert!(is_enabled());
    assert_eq!(snapshot(), PerformanceCounters::default());
}
