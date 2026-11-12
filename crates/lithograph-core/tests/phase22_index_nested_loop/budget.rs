use super::*;

#[test]
fn complete_greedy_budget_fallback_keeps_incoming_suffix_and_values() {
    let db = fixture();
    write(&db, "MATCH (a:Item) WHERE a.seq=5 SET a:Rare FINISH");
    let projection = "elementId(pinned),pinned.k,pinned.seq,elementId(r),r.seq,elementId(b),b.tag";
    let expected = query(
        &db,
        &format!("MATCH (pinned:Item)-[r:EDGE]->(b) WHERE pinned.seq=5 RETURN {projection}"),
    );
    // The common fixture has both its ordinary edge and its parallel edge.
    assert_eq!(expected.len(), 2);
    let parts = std::iter::repeat_n("(a:Item)", 7)
        .chain(["(a:Rare)"])
        .collect::<Vec<_>>()
        .join(",");
    let suffix = "WITH a AS pinned MATCH (b)<-[r:EDGE]-(pinned)";
    let text = format!("MATCH {parts} {suffix} RETURN {projection}");
    let oracle = format!(
        "MATCH {parts} WHERE true OR false {suffix} \
        WHERE true OR false RETURN {projection}"
    );
    lithograph_core::performance::set_enabled(true);
    lithograph_core::performance::reset();
    let actual = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
    let counters = lithograph_core::performance::snapshot();
    lithograph_core::performance::set_enabled(false);
    eprintln!(
        "budget prefix/suffix counters = {counters:?}; physical = {}; profile = {:?}",
        actual.physical.explain(),
        actual.summary.metrics.operator_profile()
    );
    assert!(counters.planner_states <= 4_096, "{counters:?}");
    assert!(counters.planner_candidates <= 1_024, "{counters:?}");
    assert!(counters.planner_bytes <= 256 * 1_024, "{counters:?}");
    assert!(
        counters.planner_fallbacks > 0,
        "exact must exhaust the shared budget: {counters:?}"
    );
    assert!(
        counters.join_greedy_fallbacks > 0,
        "a completed Greedy must survive: {counters:?}"
    );
    assert_eq!(counters.join_exact_completed, 0, "{counters:?}");
    assert_bag(&actual.rows, &expected);
    assert_bag(&actual.rows, &query(&db, &oracle));
    assert_complete_prefix_and_suffix(&actual);
}

fn assert_complete_prefix_and_suffix(result: &ResultSet) {
    let profile = result.summary.metrics.operator_profile().unwrap();
    let rare = result
        .physical
        .operators
        .iter()
        .position(|operator| {
            matches!(operator, PhysicalOperator::LabelIndexScan { variable, label }
            if variable == "a" && label == "Rare")
        })
        .expect("the completed Greedy starts at its Rare singleton");
    assert_eq!(profile[rare].rows, 1, "{profile:?}");
    let joins = result
        .physical
        .operators
        .iter()
        .filter(|operator| {
            matches!(
                operator,
                PhysicalOperator::NestedLoop | PhysicalOperator::IndexNestedLoop
            )
        })
        .count();
    assert_eq!(
        joins, 7,
        "all eight prefix Parts remain in the completed plan: {:?}",
        result.physical
    );
    assert!(
        !result
            .physical
            .operators
            .iter()
            .any(|operator| matches!(operator, PhysicalOperator::Cartesian))
    );
    let adjacency = result
        .physical
        .operators
        .iter()
        .enumerate()
        .filter_map(|(slot, operator)| {
            if let PhysicalOperator::AdjacencySeek {
                from,
                relationship_type,
                direction,
            } = operator
            {
                Some((slot, from, relationship_type, format!("{direction:?}")))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(adjacency.len(), 1, "{:?}", result.physical);
    let (slot, from, relationship_type, direction) = &adjacency[0];
    assert_eq!(from.as_str(), "b");
    assert_eq!(relationship_type.as_deref(), Some("EDGE"));
    assert_eq!(direction, "Incoming");
    assert_eq!(profile[*slot].operator, "AdjacencySeek");
    assert_eq!(profile[*slot].rows, 2);
    assert!(profile[*slot].db_hits > 0);
    assert_profile_accounting(result);
}
