//! Narrow, private evidence: selected prefix estimates alongside real PROFILE.
use super::*;
use crate::query::{ExecutionOptions, QueryCursor, QueryResources, prepare};
use rusqlite::Connection;

fn run(db: &Connection, text: &str) -> (usize, crate::query::QuerySummary) {
    let prepared = prepare(db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = 0;
    loop {
        let batch = cursor.next_batch(db, 32).unwrap();
        rows += batch.rows.len();
        if batch.done {
            return (rows, cursor.complete(db).unwrap());
        }
    }
}

pub(super) fn empty_graph() -> (Connection, crate::storage::HashId) {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    crate::storage::create_storage_schema(&db).unwrap();
    let root = crate::storage::initialize_root(&db).unwrap().root;
    (db, root)
}

pub(super) fn prepared_context<'q, 'db, 'ctl>(
    snapshot: &'q crate::storage::Snapshot<'db>,
    schema: &'q crate::storage::SchemaState,
    prepared: &'q crate::query::plan::PreparedQuery,
    control: &'q ExecutionControl<'ctl>,
) -> JoinPlanningContext<'q, 'db, 'ctl> {
    JoinPlanningContext {
        snapshot,
        schema,
        view: &prepared.graph_view,
        params: &prepared.params,
        control,
    }
}

fn fixture() -> Connection {
    let (db, _) = empty_graph();
    run(
        &db,
        "UNWIND range(0,63) AS i CREATE (:Item{k:i%8,seq:i}) FINISH",
    );
    run(&db, "MATCH(i:Item) WHERE i.seq=0 SET i:Rare FINISH");
    run(&db, "UNWIND [0,0,9] AS k CREATE (:Source{k:k}) FINISH");
    run(
        &db,
        "CREATE CONSTRAINT item_k FOR(i:Item) REQUIRE i.k IS :: INTEGER",
    );
    run(
        &db,
        "CREATE CONSTRAINT source_k FOR(s:Source) REQUIRE s.k IS :: INTEGER",
    );
    crate::storage::create_checkpoint(&db, crate::storage::branch_head(&db, "main").unwrap())
        .unwrap();
    run(&db, "CREATE RANGE INDEX item_key FOR(i:Item) ON(i.k)");
    db
}

fn estimates_for_prepared(
    db: &Connection,
    prepared: &crate::query::plan::PreparedQuery,
) -> (JoinEstimates, JoinAnalysis) {
    let step = &prepared.matches[0];
    let plan = step.join.as_ref().expect("actual cost-selected Join");
    let snapshot = crate::storage::Snapshot::from_resolved_state(
        db,
        prepared.resolved_state.as_ref().unwrap(),
    );
    let schema = snapshot.schema_state().unwrap();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let context = prepared_context(&snapshot, &schema, prepared, &control);
    let analysis = JoinAnalysis {
        hashes: plan.hashes.clone(),
        predicates: plan.predicates.clone(),
        templates: plan.templates.clone(),
        edges: Vec::new(),
    };
    let mut budget = PlanningBudget::default();
    let scope = JoinClauseScope {
        complete_consumer: true,
        clause_start: plan.clause_start,
        snapshot_identity: plan.snapshot_identity,
        incoming: &[],
        declared_columns: &plan.declared_columns,
    };
    let estimates = collect_join_estimates(
        step,
        &analysis,
        &context,
        &prepared.statistics,
        &mut budget,
        &scope,
    )
    .unwrap();
    (estimates, analysis)
}

fn inspect(db: &Connection, query: &str, expected_rows: usize) {
    db.execute_batch("SAVEPOINT prefix_estimate_read").unwrap();
    let prepared = prepare(db, query, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let step = &prepared.matches[0];
    let plan = step.join.as_ref().expect("real cost-selected join");
    let (estimates, _) = estimates_for_prepared(db, &prepared);
    let mut prefix = 0;
    for stage in &plan.stages {
        prefix |= 1_u128 << stage.part;
        eprintln!(
            "S4 prefix={prefix:b} part={} orientation={:?} access={:?} rows={:?}",
            stage.part,
            stage.orientation,
            stage.access,
            estimates.rows(prefix)
        );
    }
    let final_estimate = estimates.rows(prefix).unwrap();
    let (actual, profile) = run(db, &format!("PROFILE {query}"));
    assert_eq!(actual, expected_rows);
    assert!(actual as u64 >= final_estimate.lower && actual as u64 <= final_estimate.upper);
    eprintln!(
        "S4 full estimated={final_estimate:?} actual_rows={actual} actual_db_hits={} actual_operators={:?}",
        profile.metrics.db_hits,
        profile.metrics.operator_profile()
    );
    db.execute_batch("RELEASE prefix_estimate_read").unwrap();
}

#[test]
fn connected_three_part_and_skew_index_loop_report_estimates_and_actual_work() {
    let db = fixture();
    inspect(
        &db,
        "MATCH(i:Item),(s:Source),(i:Rare) WHERE i.k=s.k RETURN i.seq",
        2,
    );
    inspect(
        &db,
        "MATCH(i:Item),(s:Source) WHERE i.k=s.k RETURN i.seq",
        16,
    );
}

#[test]
fn physical_label_prefix_is_distinct_from_logical_multilabel_output() {
    let db = fixture();
    let snapshot =
        crate::storage::Snapshot::resolve(&db, crate::storage::branch_head(&db, "main").unwrap())
            .unwrap();
    let schema = snapshot.schema_state().unwrap();
    let view = ResolvedGraphView::default();
    let params = BTreeMap::new();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let context = JoinPlanningContext {
        snapshot: &snapshot,
        schema: &schema,
        view: &view,
        params: &params,
        control: &control,
    };
    let item = crate::storage::find_label(&db, "Item").unwrap().unwrap();
    let rare = crate::storage::find_label(&db, "Rare").unwrap().unwrap();
    let stats = PlannerStatistics {
        node_count: 67,
        label_counts: BTreeMap::from([(item, 64), (rare, 1)]),
        ..PlannerStatistics::default()
    };
    let node = NodeSpec {
        variable: Some("n".into()),
        labels: vec![item, rare],
        label_names: vec!["Item".into(), "Rare".into()],
        scan_label: Some(item),
        scan_label_name: Some("Item".into()),
        index_seek: None,
        id_seek: None,
        impossible: false,
    };
    let part = PatternPart {
        path_variable: None,
        start: node,
        relationship: None,
        end: None,
    };
    let access = existing(
        &part,
        false,
        &context,
        &stats,
        &[],
        &DegreeSamples::default(),
    );
    assert_eq!(node_domain(&part.start, &context, &stats), Some(1));
    assert_eq!(node_scan_domain(&part.start, &context, &stats), Some(64));
    assert_eq!(access.io, Some(64));
    assert_eq!(access.rows.unwrap().upper, 1);
    assert_eq!(
        access.rows.unwrap().lower,
        0,
        "label intersection is not exact min(counts)"
    );
}

mod bound_source;
