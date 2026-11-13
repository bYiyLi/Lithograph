use super::*;
use crate::query::{ExecutionOptions, QueryCursor, QueryResources, prepare};
use rusqlite::Connection;
use std::collections::BTreeSet;

fn run(db: &Connection, query: &str) {
    let prepared = prepare(db, query, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let mut cursor = QueryCursor::new(prepared);
    while !cursor.next_batch(db, 32).unwrap().done {}
    cursor.complete(db).unwrap();
}

fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    crate::storage::create_storage_schema(&db).unwrap();
    crate::storage::initialize_root(&db).unwrap();
    for query in [
        "CREATE (:Hub {k:1}),(:Hub {k:2}) FINISH",
        "MATCH (h:Hub) WHERE h.k=1 UNWIND range(1,3) AS i CREATE (h)-[:B]->(:Leaf) FINISH",
        "MATCH (h:Hub) WHERE h.k=2 CREATE (h)-[:B]->(:Leaf) FINISH",
        "MATCH (h:Hub) WHERE h.k=1 UNWIND range(1,8) AS i CREATE (h)-[:R]->(:Leaf) FINISH",
        "MATCH (h:Hub) WHERE h.k=2 UNWIND range(1,2) AS i CREATE (h)-[:R]->(:Leaf) FINISH",
        "UNWIND range(1,40) AS i CREATE (:Noise) FINISH",
    ] {
        run(&db, query);
    }
    crate::storage::create_checkpoint(&db, crate::storage::branch_head(&db, "main").unwrap())
        .unwrap();
    db
}

fn inspect(
    db: &Connection,
    limit: Option<u64>,
    interrupted: &dyn Fn() -> bool,
    test: impl FnOnce(
        &MatchStep,
        &ParameterTemplate,
        &JoinPlanningContext<'_, '_, '_>,
        &PlannerStatistics,
    ),
) {
    let prepared = prepare(
        db,
        "MATCH (h)-[b:B]->(leaf),(h)-[r:R]->(other) RETURN 0",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    let snapshot = Snapshot::from_resolved_state(db, prepared.resolved_state.as_ref().unwrap());
    let schema = snapshot.schema_state().unwrap();
    let types = ["B", "R"]
        .into_iter()
        .map(|name| {
            crate::storage::find_relationship_type(db, name)
                .unwrap()
                .unwrap()
        })
        .collect();
    let statistics =
        PlannerStatistics::bounded(&snapshot, &BTreeSet::new(), &types, true, 4096).unwrap();
    let resources = QueryResources::new(limit);
    let control = ExecutionControl::new(&resources, interrupted);
    let context = JoinPlanningContext {
        snapshot: &snapshot,
        schema: &schema,
        view: &prepared.graph_view,
        params: &prepared.params,
        control: &control,
    };
    let template = ParameterTemplate {
        part: 1,
        target: ProbeTarget::StartNode,
        source_guard: PartSet(1),
        access: ProbeAccess::Adjacency {
            source_variable: "h".into(),
        },
    };
    test(&prepared.matches[0], &template, &context, &statistics);
    assert_eq!(resources.used(), 0);
}

#[test]
fn actual_endpoints_keep_parallel_weights_and_reuse_one_bounded_observation() {
    inspect(
        &fixture(),
        None,
        &|| false,
        |step, template, context, statistics| {
            let mut budget = PlanningBudget::default();
            let mut cache = BTreeMap::new();
            collect(
                step,
                template,
                context,
                statistics,
                &mut budget,
                64,
                &mut cache,
            )
            .unwrap();
            assert_eq!(
                cache[&key(step, template).unwrap()],
                Some(expectation(7, 8))
            );
            assert!((1..MAX_WORK + 1).contains(&budget.states));
            assert!(!budget.exhausted());
            let before = (budget.states, budget.bytes);
            collect(
                step,
                template,
                context,
                statistics,
                &mut budget,
                64,
                &mut cache,
            )
            .unwrap();
            assert_eq!((budget.states, budget.bytes), before);
            assert_eq!(cache.len(), 1);
        },
    );
}

#[test]
fn uncertain_population_and_large_type_do_not_invent_advice() {
    inspect(
        &fixture(),
        None,
        &|| false,
        |step, template, context, statistics| {
            let source = step.parts[0]
                .relationship
                .as_ref()
                .unwrap()
                .type_id
                .unwrap();
            for total in [0, 3, 5, 33] {
                let mut statistics = statistics.clone();
                statistics.type_counts.insert(source, total);
                let mut cache = BTreeMap::new();
                collect(
                    step,
                    template,
                    context,
                    &statistics,
                    &mut PlanningBudget::default(),
                    64,
                    &mut cache,
                )
                .unwrap();
                assert!(
                    cache.values().all(Option::is_none),
                    "declared source total {total}"
                );
            }
        },
    );
}

#[test]
fn incoming_source_orientation_preserves_the_physical_endpoint() {
    inspect(&fixture(), None, &|| false, |step, template, _, _| {
        let expected = key(step, template);
        let mut reversed = step.clone();
        let part = &mut reversed.parts[0];
        std::mem::swap(&mut part.start, part.end.as_mut().unwrap());
        part.relationship.as_mut().unwrap().direction = Direction::Incoming;
        assert_eq!(key(&reversed, template), expected);
        reversed.parts[0].relationship.as_mut().unwrap().direction = Direction::Undirected;
        assert!(key(&reversed, template).is_none());
        let mut mixed = template.clone();
        mixed.source_guard = PartSet(3);
        assert!(key(step, &mixed).is_none());
    });
}

#[test]
fn capped_degree_has_unknown_upper_instead_of_a_false_exact_bound() {
    let db = fixture();
    run(
        &db,
        "MATCH (h:Hub) WHERE h.k=1 UNWIND range(1,40) AS i CREATE (h)-[:R]->(:Leaf) FINISH",
    );
    crate::storage::create_checkpoint(&db, crate::storage::branch_head(&db, "main").unwrap())
        .unwrap();
    inspect(
        &db,
        None,
        &|| false,
        |step, template, context, statistics| {
            let mut cache = BTreeMap::new();
            collect(
                step,
                template,
                context,
                statistics,
                &mut PlanningBudget::default(),
                64,
                &mut cache,
            )
            .unwrap();
            let advice = cache[&key(step, template).unwrap()].unwrap();
            assert_eq!(advice.expected, (33_u64 * 3 + 2).div_ceil(4));
            assert_eq!(advice.upper, u64::MAX);
            assert_eq!(advice.lower, 0);
        },
    );
}

#[test]
fn small_memory_and_planner_headroom_keep_execution_fallback_available() {
    let db = fixture();
    inspect(
        &db,
        Some(1024),
        &|| false,
        |step, template, context, statistics| {
            let mut cache = BTreeMap::new();
            let mut budget = PlanningBudget::default();
            collect(
                step,
                template,
                context,
                statistics,
                &mut budget,
                64,
                &mut cache,
            )
            .unwrap();
            assert!(cache.values().all(Option::is_none));
            assert!(!budget.exhausted());
        },
    );
    inspect(
        &db,
        None,
        &|| false,
        |step, template, context, statistics| {
            let mut cache = BTreeMap::new();
            let mut budget = PlanningBudget::default();
            collect(
                step,
                template,
                context,
                statistics,
                &mut budget,
                4096,
                &mut cache,
            )
            .unwrap();
            assert!(cache.is_empty());
            assert_eq!(budget.states, 0);
            assert!(!budget.exhausted());
        },
    );
}

#[test]
fn sampler_propagates_interruption_without_leaking_scratch() {
    inspect(
        &fixture(),
        None,
        &|| true,
        |step, template, context, statistics| {
            let error = collect(
                step,
                template,
                context,
                statistics,
                &mut PlanningBudget::default(),
                64,
                &mut BTreeMap::new(),
            )
            .unwrap_err();
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
        },
    );
}
