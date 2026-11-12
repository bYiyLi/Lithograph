use super::*;
use crate::query::QueryErrorKind;
use crate::query::plan::PhysicalOperator;

fn hash_plan() -> PhysicalPlan {
    PhysicalPlan {
        operators: vec![
            PhysicalOperator::NodeScan {
                variable: "build".to_owned(),
            },
            PhysicalOperator::HashJoin,
            PhysicalOperator::Project,
        ],
    }
}

#[test]
fn selected_hash_starts_at_zero_and_non_hash_or_plain_execution_has_no_hash_stats() {
    let resources = QueryResources::default();
    let physical = hash_plan();
    let metrics =
        QueryMetrics::for_execution_with_resources(ExecutionMode::Profile, &physical, &resources)
            .unwrap();
    let operators = metrics.operator_profile().unwrap();
    assert_eq!(operators[1].hash, Some(HashJoinRuntimeMetrics::default()));
    assert!(operators[0].hash.is_none());
    assert!(operators[2].hash.is_none());
    assert_eq!(operators[1].rows, 0);
    assert!(metrics.execution_memory_peak_bytes().is_none());
    let plain =
        QueryMetrics::for_execution_with_resources(ExecutionMode::Execute, &physical, &resources)
            .unwrap();
    assert!(plain.operator_profile().is_none());
    assert!(plain.execution_memory_peak_bytes().is_none());
}

#[test]
fn hash_profile_replaces_cumulative_snapshots_at_only_the_selected_slot() {
    let resources = QueryResources::default();
    let mut metrics = QueryMetrics::for_execution_with_resources(
        ExecutionMode::Profile,
        &hash_plan(),
        &resources,
    )
    .unwrap();
    let first = HashJoinRuntimeMetrics {
        build_rows: 3,
        probe_rows: 5,
        hash_collisions: 1,
        spill_partitions: 2,
        spill_batches: 4,
        spill_vm_steps: 7,
        spill_encoded_bytes: 11,
        memory_peak_bytes: 13,
        temp_allocated_bytes: 17,
    };
    metrics.set_hash_profile(Some(1), first);
    let next = HashJoinRuntimeMetrics {
        probe_rows: 9,
        spill_batches: 6,
        ..first
    };
    metrics.set_hash_profile(Some(1), next);
    metrics.set_hash_profile(Some(0), next);
    metrics.set_hash_profile(None, next);
    metrics.set_hash_profile(Some(999), next);
    let previous = metrics.activate_profile_slot(Some(0));
    metrics.record_db_hits(7);
    metrics.restore_access_operator(previous);
    let operators = metrics.operator_profile().unwrap();
    assert_eq!(operators[1].hash, Some(next));
    assert!(operators[0].hash.is_none());
    assert_eq!(operators[1].db_hits, 0);
    assert_eq!(
        operators
            .iter()
            .map(|operator| operator.db_hits)
            .sum::<u64>(),
        metrics.db_hits
    );
}

#[test]
fn terminal_hash_execution_peak_is_frozen_once_and_survives_summary_clone() {
    let resources = QueryResources::default();
    let mut metrics = QueryMetrics::for_execution_with_resources(
        ExecutionMode::Profile,
        &hash_plan(),
        &resources,
    )
    .unwrap();
    let stats = HashJoinRuntimeMetrics {
        build_rows: 3,
        probe_rows: 9,
        ..HashJoinRuntimeMetrics::default()
    };
    metrics.set_hash_profile(Some(1), stats);
    let temporary = resources.reserve(1024).unwrap();
    metrics.finish_profile();
    let frozen = metrics.execution_memory_peak_bytes().unwrap();
    assert_eq!(frozen, resources.peak());
    let late = resources.reserve(frozen + 1).unwrap();
    assert!(resources.peak() > frozen);
    metrics.finish_profile();
    assert_eq!(metrics.execution_memory_peak_bytes(), Some(frozen));
    let summary = crate::query::QuerySummary {
        query_type: crate::query::QueryType::Read,
        commit: None,
        merge_session: None,
        counters: crate::query::QueryCounters::default(),
        metrics,
    };
    let copy = summary.try_clone().unwrap();
    assert_eq!(copy, summary);
    assert_eq!(copy.metrics.execution_memory_peak_bytes(), Some(frozen));
    assert_eq!(
        copy.metrics.operator_profile().unwrap()[1].hash,
        Some(stats)
    );
    drop((copy, summary, late, temporary));
    assert_eq!(resources.used(), 0);
}

#[test]
fn non_hash_profile_omits_execution_hash_memory_metrics() {
    let resources = QueryResources::default();
    let mut metrics =
        QueryMetrics::for_execution_with_resources(ExecutionMode::Profile, &plan(), &resources)
            .unwrap();
    metrics.set_hash_profile(Some(0), HashJoinRuntimeMetrics::default());
    metrics.finish_profile();
    assert!(metrics.execution_memory_peak_bytes().is_none());
    assert!(
        metrics
            .operator_profile()
            .unwrap()
            .iter()
            .all(|operator| operator.hash.is_none())
    );
}

#[test]
fn exact_clause_scopes_keep_points_and_later_scans_distinct() {
    let physical = PhysicalPlan {
        operators: vec![
            PhysicalOperator::NodeIdSeek {
                variable: "a".into(),
            },
            PhysicalOperator::NodeScan {
                variable: "b".into(),
            },
            PhysicalOperator::NodeIdSeek {
                variable: "c".into(),
            },
            PhysicalOperator::NodeScan {
                variable: "d".into(),
            },
            PhysicalOperator::NodeScan {
                variable: "e".into(),
            },
            PhysicalOperator::Project,
        ],
    };
    let resources = QueryResources::new(None);
    let mut metrics =
        QueryMetrics::for_execution_with_resources(ExecutionMode::Profile, &physical, &resources)
            .unwrap();
    let bindings = [0, 1, 2, 3, 4].map(|slot| ProfileBinding {
        site: crate::query::plan::OperatorSite {
            clause_start: if slot == 4 { 20 } else { 10 },
            part: Some(slot),
            role: ProfileRole::NodeAccess,
        },
        physical_slot: slot,
    });
    metrics.install_profile_sites(bindings.into_iter()).unwrap();
    // An AST-based group count can exceed the physical access count when
    // an incoming relationship start omits its NodeScan. Exact clauses must
    // use their own storage slots rather than consume a later clause.
    metrics.activate_match_clause_access(10, 6);
    metrics.record_db_hits(2);
    let point = metrics.activate_profile_slot(Some(0));
    metrics.record_db_hits(1);
    metrics.restore_access_operator(point);
    metrics.activate_match_clause_access(20, 1);
    metrics.record_db_hits(7);
    let operators = metrics.operator_profile().unwrap();
    assert_eq!(operators[0].db_hits, 1);
    assert_eq!(operators[1].db_hits, 2);
    assert_eq!(operators[2].db_hits, 0);
    assert_eq!(operators[3].db_hits, 0);
    assert_eq!(operators[4].db_hits, 7);
    assert_eq!(
        operators
            .iter()
            .map(|operator| operator.db_hits)
            .sum::<u64>(),
        metrics.db_hits
    );
    drop(metrics);
    assert_eq!(resources.used(), 0);
}

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

fn access_plan() -> PhysicalPlan {
    PhysicalPlan {
        operators: vec![
            PhysicalOperator::NodeScan {
                variable: "n".to_owned(),
            },
            PhysicalOperator::AdjacencySeek {
                from: "n".to_owned(),
                relationship_type: None,
                direction: crate::query::plan::Direction::Outgoing,
            },
            PhysicalOperator::NodeIdSeek {
                variable: "m".to_owned(),
            },
        ],
    }
}

#[test]
fn profile_hits_are_attributed_to_the_exact_access_operator() {
    let physical = access_plan();
    let resources = QueryResources::new(None);
    let mut metrics =
        QueryMetrics::for_execution_with_resources(ExecutionMode::Profile, &physical, &resources)
            .expect("profile metrics");

    let adjacency = metrics
        .activate_access_for_operator(Some("n"), "AdjacencySeek")
        .expect("adjacency operator");
    metrics.record_db_hits(3);
    metrics.restore_access_operator(Some(adjacency));

    let identity = metrics
        .activate_access_for_operator(Some("m"), "NodeIdSeek")
        .expect("identity operator");
    metrics.record_db_hits(2);
    metrics.restore_access_operator(Some(identity));

    let operators = metrics.operator_profile().expect("profile");
    assert_eq!(operators[0].db_hits, 0);
    assert_eq!(operators[1].db_hits, 3);
    assert_eq!(operators[2].db_hits, 2);
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
