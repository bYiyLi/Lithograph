use super::*;

fn multiway_plan() -> PhysicalPlan {
    PhysicalPlan {
        operators: vec![
            PhysicalOperator::MultiwayIntersect,
            PhysicalOperator::Project,
        ],
    }
}

#[test]
fn multiway_incoming_owners_add_new_events_once_and_keep_the_largest_live_peak() {
    let resources = QueryResources::default();
    let mut metrics = QueryMetrics::for_execution_with_resources(
        ExecutionMode::Profile,
        &multiway_plan(),
        &resources,
    )
    .unwrap();
    let first = MultiwayRuntimeMetrics {
        input_lists: 2,
        adjacency_rows: 9,
        seek_operations: 3,
        intersection_steps: 5,
        candidate_nodes: 2,
        node_assignments: 1,
        relationship_candidates: 4,
        memory_peak_bytes: 4096,
    };
    let mut published = MultiwayRuntimeMetrics::default();
    metrics.add_multiway_profile(Some(0), first.take_delta(&mut published));
    // Repeated EOF/publication by the same owner must not replay its events.
    metrics.add_multiway_profile(Some(0), first.take_delta(&mut published));
    let completed = MultiwayRuntimeMetrics {
        input_lists: 4,
        adjacency_rows: 18,
        seek_operations: 6,
        intersection_steps: 10,
        candidate_nodes: 4,
        node_assignments: 2,
        relationship_candidates: 8,
        memory_peak_bytes: 6144,
    };
    metrics.add_multiway_profile(Some(0), completed.take_delta(&mut published));
    // Another incoming row starts its own counters at zero at the same site.
    let mut second_published = MultiwayRuntimeMetrics::default();
    let second = MultiwayRuntimeMetrics {
        memory_peak_bytes: 1024,
        ..first
    };
    metrics.add_multiway_profile(Some(0), second.take_delta(&mut second_published));
    metrics.add_multiway_profile(Some(0), second.take_delta(&mut second_published));
    let expected = MultiwayRuntimeMetrics {
        input_lists: 6,
        adjacency_rows: 27,
        seek_operations: 9,
        intersection_steps: 15,
        candidate_nodes: 6,
        node_assignments: 3,
        relationship_candidates: 12,
        memory_peak_bytes: 6144,
    };
    assert_eq!(
        metrics.operator_profile().unwrap()[0].multiway,
        Some(expected)
    );
    assert_eq!(published, completed);
    assert_eq!(second_published, second);
}

#[test]
fn selected_but_unexecuted_multiway_is_zero_and_other_slots_receive_no_events() {
    let resources = QueryResources::default();
    let mut metrics = QueryMetrics::for_execution_with_resources(
        ExecutionMode::Profile,
        &multiway_plan(),
        &resources,
    )
    .unwrap();
    let delta = MultiwayRuntimeMetrics {
        node_assignments: 7,
        memory_peak_bytes: 1024,
        ..MultiwayRuntimeMetrics::default()
    };
    metrics.add_multiway_profile(None, delta);
    metrics.add_multiway_profile(Some(1), delta);
    metrics.add_multiway_profile(Some(99), delta);
    let operators = metrics.operator_profile().unwrap();
    assert_eq!(
        operators[0].multiway,
        Some(MultiwayRuntimeMetrics::default())
    );
    assert_eq!(operators[0].rows, 0);
    assert!(operators[1].multiway.is_none());
    let mut ordinary = QueryMetrics::for_execution_with_resources(
        ExecutionMode::Execute,
        &multiway_plan(),
        &resources,
    )
    .unwrap();
    ordinary.add_multiway_profile(Some(0), delta);
    assert!(ordinary.operator_profile().is_none());
}
