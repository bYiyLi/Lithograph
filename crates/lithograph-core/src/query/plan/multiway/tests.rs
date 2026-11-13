//! Real CBO regression: cyclic specialization shares the one prepare budget.

use super::*;
use crate::query::{ExecutionOptions, QueryCursor, prepare};
use std::cell::RefCell;

#[derive(Debug, Default)]
struct Advice {
    before: Option<(usize, usize, bool, Option<u64>)>,
    candidates: Vec<(usize, Option<u64>, usize, bool)>,
    comparison: Option<(u64, Option<u64>)>,
    stages: Vec<(&'static str, usize, usize, bool)>,
}

thread_local! { static ADVICE: RefCell<Option<Advice>> = const { RefCell::new(None) }; }

mod component;
mod memory;
mod mixed;
mod public_hash;

pub(super) fn record_before(budget: &PlanningBudget, old: Option<u64>) {
    ADVICE.with_borrow_mut(|advice| {
        if let Some(advice) = advice {
            advice.before = Some((budget.states, budget.bytes, budget.exhausted(), old));
        }
    });
}

pub(super) fn record_stage(label: &'static str, required: usize, budget: &PlanningBudget) {
    ADVICE.with_borrow_mut(|advice| {
        if let Some(advice) = advice {
            advice.stages.push((
                label,
                required,
                budget.remaining_states(),
                budget.exhausted(),
            ));
        }
    });
}

pub(super) fn record_candidate(root: usize, work: Option<u64>, budget: &PlanningBudget) {
    ADVICE.with_borrow_mut(|advice| {
        if let Some(advice) = advice {
            advice
                .candidates
                .push((root, work, budget.states, budget.exhausted()));
        }
    });
}

pub(super) fn record_comparison(specialized: u64, binary: Option<u64>) {
    ADVICE.with_borrow_mut(|advice| {
        if let Some(advice) = advice {
            advice.comparison = Some((specialized, binary));
        }
    });
}

fn run(db: &Connection, text: &str) -> usize {
    run_params(db, text, BTreeMap::new())
}

fn run_params(db: &Connection, text: &str, params: BTreeMap<String, Value>) -> usize {
    let prepared = prepare(db, text, params, ExecutionOptions::default()).unwrap();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = 0;
    loop {
        let batch = cursor.next_batch(db, 128).unwrap();
        rows += batch.rows.len();
        if batch.done {
            break;
        }
    }
    cursor.complete(db).unwrap();
    rows
}

fn checkpoint(db: &Connection) {
    storage::create_checkpoint(db, storage::branch_head(db, "main").unwrap()).unwrap();
}

fn fixture(edges: &[(usize, usize)]) -> (Connection, String) {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    for node in 0..4 {
        assert_eq!(
            run(
                &db,
                &format!(
                    "UNWIND range(0,15) AS i CREATE(n:S8Private{node} {{seq:i}}) RETURN elementId(n)"
                )
            ),
            16
        );
    }
    let mut patterns = Vec::new();
    for (part, (source, target)) in edges.iter().copied().enumerate() {
        assert_eq!(
            run(
                &db,
                &format!(
                    "MATCH(a:S8Private{source}),(b:S8Private{target}) WHERE (b.seq-a.seq+16)%16<8 CREATE(a)-[r:S8PrivateR{part}]->(b) RETURN elementId(r)"
                )
            ),
            128
        );
        patterns.push(format!("(n{source}:S8Private{source})-[r{part}:S8PrivateR{part}]->(n{target}:S8Private{target})"));
    }
    checkpoint(&db);
    (
        db,
        format!("MATCH {} RETURN elementId(n0)", patterns.join(",")),
    )
}

#[test]
fn cycle_specialization_competes_with_binary_within_shared_budget() {
    let shapes: &[(&str, &[(usize, usize)])] = &[
        ("four-cycle", &[(0, 1), (1, 2), (2, 3), (3, 0)]),
        ("clique", &[(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)]),
        (
            "shared-cycles",
            &[(0, 1), (1, 2), (2, 0), (0, 2), (2, 3), (3, 0)],
        ),
    ];
    let mut failures = Vec::new();
    for (name, edges) in shapes {
        let (db, text) = fixture(edges);
        db.execute_batch("SAVEPOINT multiway_budget_regression")
            .unwrap();
        ADVICE.set(Some(Advice::default()));
        let prepared = prepare(&db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
        let advice = ADVICE.take().unwrap();
        let plan = prepared.physical.explain();
        eprintln!("multiway-shared-budget shape={name} advice={advice:?} plan={plan}");
        if !plan.contains("MultiwayIntersect") {
            failures.push(format!("{name}: {advice:?}: {plan}"));
        } else {
            let (specialized, binary) = advice
                .comparison
                .expect("final comparison uses a completed binary Cost");
            assert!(
                specialized < binary.expect("unknown binary costs cannot select a specialization")
            );
        }
        assert!(advice.before.unwrap().0 <= super::super::budget::MAX_STATES);
        assert!(advice.before.unwrap().1 <= super::super::budget::MAX_BYTES);
        assert!(
            advice
                .candidates
                .iter()
                .all(|(_, _, states, _)| *states <= super::super::budget::MAX_STATES)
        );
        db.execute_batch("RELEASE multiway_budget_regression")
            .unwrap();
    }
    assert!(
        failures.is_empty(),
        "safe beneficial cyclic candidates must retain shared-budget opportunity: {failures:#?}"
    );
}

#[test]
fn node_only_matches_do_not_spend_specialization_budget() {
    let (db, _) = fixture(&[]);
    db.execute_batch("SAVEPOINT multiway_node_only_budget")
        .unwrap();
    let prepared = prepare(&db,
        "MATCH(a:S8Private0),(b:S8Private1),(c:S8Private2) RETURN elementId(a),elementId(b),elementId(c)",
        BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let step = &prepared.matches[0];
    let original = step.join.as_ref().expect("completed ordinary binary plan");
    let snapshot =
        storage::Snapshot::from_resolved_state(&db, prepared.resolved_state.as_ref().unwrap());
    let schema = snapshot.schema_state().unwrap();
    let resources = crate::query::QueryResources::default();
    let control = crate::query::ExecutionControl::new(&resources, &|| false);
    let context = join::JoinPlanningContext {
        snapshot: &snapshot,
        schema: &schema,
        view: &prepared.graph_view,
        params: &prepared.params,
        control: &control,
    };
    let scope = join::JoinClauseScope {
        complete_consumer: true,
        clause_start: step.clause_start,
        snapshot_identity: snapshot.cache_identity(),
        incoming: &[],
        declared_columns: &original.declared_columns,
    };
    let mut budget = PlanningBudget::default();
    let analysis = join::analyze_join(step, &scope, &context, &mut budget)
        .unwrap()
        .unwrap();
    let estimates = join::collect_join_estimates(
        step,
        &analysis,
        &context,
        &prepared.statistics,
        &mut budget,
        &scope,
    )
    .unwrap();
    let before = (
        budget.states,
        budget.bytes,
        budget.candidates,
        budget.exhausted(),
    );
    // A known work value ensures the shape guard, not unknown-cost refusal,
    // is what protects this already collected ordinary MATCH's budget.
    let candidate = price(
        step,
        CompletedJoin {
            plan: original,
            work: Some(1),
        },
        &scope,
        &context,
        &prepared.statistics,
        &estimates,
        &mut budget,
    )
    .unwrap();
    assert!(candidate.is_none());
    assert_eq!(
        before,
        (
            budget.states,
            budget.bytes,
            budget.candidates,
            budget.exhausted()
        )
    );
    db.execute_batch("RELEASE multiway_node_only_budget")
        .unwrap();
}
