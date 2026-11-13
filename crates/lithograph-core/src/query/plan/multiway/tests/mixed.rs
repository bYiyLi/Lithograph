//! Both MATCH orders must retain useful candidates in one query-wide budget.

use super::*;
use crate::cypher::Value;
use crate::query::plan::budget::{self, trace};

fn seed(db: &Connection, label: &str, keys: Vec<Value>) {
    let text = format!("UNWIND $keys AS k CREATE(n:{label} {{k:k}}) RETURN elementId(n)");
    let expected = keys.len();
    let count = run_params(
        db,
        &text,
        BTreeMap::from([("keys".into(), Value::List(keys))]),
    );
    assert_eq!(count, expected);
    run(
        db,
        &format!("CREATE CONSTRAINT {label}_type FOR(n:{label}) REQUIRE n.k IS :: INTEGER"),
    );
}

#[test]
fn mixed_hash_and_multiway_share_one_query_planning_budget() {
    let (db, cycle) = fixture(&[(0, 1), (1, 2), (2, 0)]);
    seed(
        &db,
        "BudgetBuild",
        (0..256).map(|i| Value::Integer(i % 32)).collect(),
    );
    seed(
        &db,
        "BudgetProbe",
        (0..256)
            .map(|i| match i {
                0 | 1 => Value::Integer(5),
                2 => Value::Integer(13),
                3 => Value::Null,
                _ => Value::Integer(100_000 + i),
            })
            .collect(),
    );
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let cycle_match = cycle.split(" RETURN ").next().unwrap();
    let texts = [
        format!(
            "MATCH(u:BudgetBuild),(v:BudgetProbe) WHERE u.k=v.k WITH elementId(u) AS uid,elementId(v) AS vid {cycle} "
        ),
        format!(
            "{cycle_match} WITH DISTINCT n0 AS anchor MATCH(u:BudgetBuild),(v:BudgetProbe) WHERE u.k=v.k RETURN elementId(anchor),elementId(u),elementId(v)"
        ),
    ];
    let mut failures = Vec::new();
    for text in texts {
        db.execute_batch("SAVEPOINT mixed_budget_regression")
            .unwrap();
        trace::start();
        let prepared = prepare(&db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
        let observed = trace::take();
        let plan = prepared.physical.explain();
        eprintln!("mixed-query-budget text={text} trace={observed:#?} plan={plan}");
        assert!(!observed.is_empty());
        assert!(
            observed
                .iter()
                .all(|entry| entry.states <= budget::MAX_STATES
                    && entry.bytes <= budget::MAX_BYTES
                    && entry.candidates <= budget::MAX_CANDIDATES
                    && entry.remaining_states <= budget::MAX_STATES
                    && entry.remaining_bytes <= budget::MAX_BYTES)
        );
        // Read every trace field in assertions as well as Debug output.
        assert!(
            observed
                .iter()
                .all(|entry| entry.clause_start < text.len() && !entry.stage.is_empty())
        );
        if !plan.contains("HashJoin") || !plan.contains("MultiwayIntersect") {
            failures.push(format!("{text}: {observed:#?}: {plan}"));
        }
        let exhausted = observed.iter().filter(|entry| entry.exhausted).count();
        eprintln!("mixed-query-budget exhausted_observations={exhausted}");
        db.execute_batch("RELEASE mixed_budget_regression").unwrap();
    }
    assert!(
        failures.is_empty(),
        "both actual algorithms must remain eligible: {failures:#?}"
    );
}
