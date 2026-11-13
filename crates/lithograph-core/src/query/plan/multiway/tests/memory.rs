//! Compare the prepared resident bound with actual complete PROFILE owners.

use super::*;

fn expected_sequences() -> BTreeMap<(i64, i64, i64), usize> {
    let mut expected = BTreeMap::new();
    for a in 0..16_i64 {
        for b in 0..16_i64 {
            for c in 0..16_i64 {
                if (b - a + 16) % 16 < 8 && (c - b + 16) % 16 < 8 && (a - c + 16) % 16 < 8 {
                    expected.insert((a, b, c), 1usize);
                }
            }
        }
    }
    expected
}

fn swap_first_roots(plan: &MultiwayPlan) -> MultiwayPlan {
    let mut rotated = plan.clone();
    rotated.nodes.swap(0, 1);
    for node in &mut rotated.nodes {
        node.constraints.clear();
    }
    for (index, edge) in rotated.edges.iter_mut().enumerate() {
        if edge.source < 2 {
            edge.source ^= 1;
        }
        if edge.target < 2 {
            edge.target ^= 1;
        }
        rotated.nodes[edge.source.max(edge.target)]
            .constraints
            .push(index);
    }
    rotated
}

fn refusal_is_proved(advice: &Advice, trace: &[crate::query::plan::budget::trace::Entry]) -> bool {
    let cost = advice
        .comparison
        .is_some_and(|(candidate, binary)| binary.is_some_and(|binary| candidate >= binary));
    let budget = advice.before.is_some_and(|before| before.2)
        || advice.stages.iter().any(|stage| stage.3)
        || trace.iter().any(|entry| {
            entry.exhausted
                && (entry.stage == "greedy-refused"
                    || entry.stage == "specialization-complete"
                    || advice.before.is_none() && entry.stage == "read-clause-leave")
        });
    eprintln!("long-alias-refusal proved_budget={budget} proved_cost={cost}");
    cost || budget
}

fn renamed_lowered_plan(
    original: &MultiwayPlan,
    original_names: &[String],
    names: &[String],
    step: &MatchStep,
) -> MultiwayPlan {
    let mut plan = original.clone();
    plan.clause_start = step.clause_start;
    for node in &mut plan.nodes {
        let index = original_names
            .iter()
            .position(|name| name == &node.variable)
            .unwrap();
        let name = &names[index];
        let lowered = step
            .parts
            .iter()
            .flat_map(|part| [Some(&part.start), part.end.as_ref()].into_iter().flatten())
            .find(|spec| spec.variable.as_deref() == Some(name.as_str()))
            .expect("the renamed candidate uses the actual lowered Node");
        assert_eq!(lowered.labels, node.spec.labels);
        node.variable = name.clone();
        node.spec = lowered.clone();
    }
    plan
}

fn assert_bounds(
    step: &MatchStep,
    plan: &MultiwayPlan,
    control: &crate::query::ExecutionControl<'_>,
) -> u64 {
    let bound =
        crate::query::stream::multiway::planning_owner_memory_bytes(step, plan, control).unwrap();
    let provisional = swap_first_roots(plan);
    assert_eq!(
        crate::query::stream::multiway::planning_owner_memory_bytes(step, &provisional, control)
            .unwrap(),
        bound,
        "pre-cost descriptor order must not change its resident upper bound"
    );
    if let Some(binary) = &step.join {
        let prefix = crate::query::stream::multiway::planning_prefix_memory_bytes(
            step, plan, binary, control,
        )
        .unwrap();
        assert!(prefix > bound, "prefix also retains its outer MATCH cursor");
    }
    bound
}

#[test]
fn fresh_owner_resident_bound_covers_complete_profile_with_long_aliases() {
    let (db, cycle) = fixture(&[(0, 1), (1, 2), (2, 0)]);
    let expected = expected_sequences();
    assert_eq!(expected.len(), 352);
    let mut blueprint: Option<(MultiwayPlan, Vec<String>)> = None;
    for padding in [0, 80] {
        let names: Vec<_> = (0..3)
            .map(|id| format!("node_{id}_{}", "x".repeat(padding * (id + 1))))
            .collect();
        let mut pattern = cycle.split(" RETURN ").next().unwrap().to_owned();
        for (id, name) in names.iter().enumerate() {
            pattern = pattern.replace(&format!("n{id}"), name);
        }
        let columns = names
            .iter()
            .map(|name| format!("{name}.seq"))
            .chain((0..3).map(|id| format!("elementId(r{id})")))
            .collect::<Vec<_>>()
            .join(",");
        let text = format!("PROFILE {pattern} RETURN {columns}");
        ADVICE.set(Some(Advice::default()));
        crate::query::plan::budget::trace::start();
        let prepared = prepare(&db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
        let advice = ADVICE.take().unwrap();
        let trace = crate::query::plan::budget::trace::take();
        eprintln!(
            "fresh-component-selection padding={padding} advice={advice:#?} budget={trace:#?} plan={}",
            prepared.physical.explain()
        );
        let step = &prepared.matches[0];
        let selected = step.join.as_ref().and_then(|join| join.multiway.as_ref());
        let actual_multiway = selected.is_some();
        let plan = match selected {
            Some(plan) => plan.as_ref().clone(),
            None => {
                assert!(padding > 0, "normal alias must execute Multiway");
                assert!(
                    refusal_is_proved(&advice, &trace),
                    "unexplained CBO refusal"
                );
                let (original, original_names) = blueprint.as_ref().unwrap();
                // This descriptor is used only by pure sizing. The engine
                // still executes the real selected binary fallback below.
                renamed_lowered_plan(original, original_names, &names, step)
            }
        };
        let _ = blueprint.get_or_insert_with(|| (plan.clone(), names.clone()));
        let resources = crate::query::QueryResources::default();
        let control = crate::query::ExecutionControl::new(&resources, &|| false);
        let bound = assert_bounds(step, &plan, &control);
        let mut cursor = QueryCursor::new(prepared);
        let mut actual = BTreeMap::new();
        loop {
            let batch = cursor.next_batch(&db, 37).unwrap();
            for row in batch.rows {
                let [
                    Value::Integer(a),
                    Value::Integer(b),
                    Value::Integer(c),
                    Value::String(r0),
                    Value::String(r1),
                    Value::String(r2),
                ] = row.as_slice()
                else {
                    panic!("complete typed cyclic output: {row:?}");
                };
                assert!(r0.starts_with("r:") && r1.starts_with("r:") && r2.starts_with("r:"));
                assert!(r0 != r1 && r0 != r2 && r1 != r2);
                *actual.entry((*a, *b, *c)).or_insert(0usize) += 1;
            }
            if batch.done {
                break;
            }
        }
        let summary = cursor.complete(&db).unwrap();
        assert_eq!(actual, expected);
        let work = summary
            .metrics
            .operator_profile()
            .unwrap()
            .iter()
            .find_map(|operator| operator.multiway);
        match work {
            Some(work) => {
                assert!(actual_multiway);
                eprintln!(
                    "fresh-component-bound padding={padding} bound={bound} observed={}",
                    work.memory_peak_bytes
                );
                assert!(work.memory_peak_bytes > 0 && work.memory_peak_bytes <= bound);
            }
            None => {
                assert!(!actual_multiway);
                eprintln!(
                    "fresh-component-bound padding={padding} selected_binary=true pure_bound={bound}"
                );
            }
        }
    }
}
