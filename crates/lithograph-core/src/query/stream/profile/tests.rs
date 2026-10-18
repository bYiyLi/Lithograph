use super::*;
use crate::query::QueryErrorKind;
use crate::query::plan::PhysicalOperator;

fn plan() -> PhysicalPlan {
    PhysicalPlan {
        operators: vec![
            PhysicalOperator::NodeScan {
                variable: "n".to_owned(),
            },
            PhysicalOperator::Filter,
            PhysicalOperator::Project,
        ],
    }
}

#[test]
fn profile_arrays_and_names_are_reserved_before_construction() {
    let physical = plan();
    let bytes = initial_profile_bytes(&physical, 1).expect("profile bytes");
    let resources = QueryResources::new(Some(bytes - 1));
    let error =
        QueryMetrics::for_execution_with_resources(ExecutionMode::Profile, &physical, &resources)
            .expect_err("profile must fail before allocation");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.used(), 0);
    assert_eq!(resources.peak(), 0);
}

#[test]
fn profile_clone_requires_a_second_live_reservation() {
    let physical = plan();
    let bytes = initial_profile_bytes(&physical, 1).expect("profile bytes");
    let resources = QueryResources::new(Some(bytes * 2 - 1));
    let metrics =
        QueryMetrics::for_execution_with_resources(ExecutionMode::Profile, &physical, &resources)
            .expect("profile metrics");
    assert_eq!(resources.used(), bytes);
    let error = metrics
        .try_clone()
        .expect_err("clone must not reuse the original claim");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.used(), bytes);
    drop(metrics);
    assert_eq!(resources.used(), 0);
}

#[test]
fn profile_clone_preserves_counters_and_releases_its_own_allocation() {
    let physical = plan();
    let bytes = initial_profile_bytes(&physical, 1).expect("profile bytes");
    let resources = QueryResources::new(Some(bytes * 3 + 512));
    let mut metrics =
        QueryMetrics::for_execution_with_resources(ExecutionMode::Profile, &physical, &resources)
            .expect("profile metrics");
    metrics.rows = 7;
    metrics.record_db_hits(11);
    metrics.finish_profile();
    let copy = metrics
        .try_clone_with_extra_bytes(512)
        .expect("profile and summary string copy");
    assert_eq!(copy, metrics);
    assert_eq!(resources.used(), bytes * 2 + 512);
    assert_eq!(
        copy.operator_profile()
            .expect("profile")
            .last()
            .expect("last operator")
            .rows,
        7
    );
    drop(copy);
    assert_eq!(resources.used(), bytes);
    drop(metrics);
    assert_eq!(resources.used(), 0);
}

#[test]
fn plain_metrics_keep_the_execution_budget_for_summary_string_copies() {
    let resources = QueryResources::new(Some(64));
    let metrics =
        QueryMetrics::for_execution_with_resources(ExecutionMode::Execute, &plan(), &resources)
            .expect("plain metrics");
    assert_eq!(resources.used(), 0);
    assert!(metrics.operator_profile().is_none());
    let error = metrics
        .try_clone_with_extra_bytes(65)
        .expect_err("summary copy must use execution quota");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    let copy = metrics
        .try_clone_with_extra_bytes(64)
        .expect("bounded summary metadata");
    assert_eq!(copy, metrics);
    assert_eq!(resources.used(), 64);
    drop(copy);
    assert_eq!(resources.used(), 0);
}
