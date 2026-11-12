//! A small real graph diagnoses whether an already sampled hot source is
//! hidden behind a different estimate for the very same bound adjacency.
use super::*;
use crate::storage::{CommitMetadata, LayerBuilder, OwnerKind, PropertyValue, RelationshipRecord};

const FANOUT: i64 = 32;

fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    crate::storage::create_storage_schema(&db).unwrap();
    let root = crate::storage::initialize_root(&db).unwrap().root;
    let rare = crate::storage::intern_label(&db, "Rare").unwrap();
    let b_label = crate::storage::intern_label(&db, "B").unwrap();
    let chosen = crate::storage::intern_label(&db, "Chosen").unwrap();
    let r_type = crate::storage::intern_relationship_type(&db, "R").unwrap();
    let key = crate::storage::intern_property_key(&db, "k").unwrap();
    let mut layer = LayerBuilder::default();
    let hub = crate::storage::allocate_node_id(&db).unwrap();
    layer.add_node(hub).unwrap();
    layer.add_label(hub, rare).unwrap();
    for number in 0..FANOUT {
        let b = crate::storage::allocate_node_id(&db).unwrap();
        layer.add_node(b).unwrap();
        layer.add_label(b, b_label).unwrap();
        layer
            .set_property(OwnerKind::Node, b, key, PropertyValue::Integer(number))
            .unwrap();
        let id = crate::storage::allocate_relationship_id(&db).unwrap();
        layer
            .add_relationship(RelationshipRecord {
                id,
                source: hub,
                target: b,
                type_id: r_type,
            })
            .unwrap();
    }
    let c = crate::storage::allocate_node_id(&db).unwrap();
    layer.add_node(c).unwrap();
    layer.add_label(c, chosen).unwrap();
    layer
        .set_property(OwnerKind::Node, c, key, PropertyValue::Integer(0))
        .unwrap();
    crate::storage::commit_layer(
        &db,
        "main",
        root,
        None,
        &layer,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 1,
        },
    )
    .unwrap();
    run(
        &db,
        "CREATE CONSTRAINT b_key FOR(b:B) REQUIRE b.k IS :: INTEGER",
    );
    run(
        &db,
        "CREATE CONSTRAINT chosen_key FOR(c:Chosen) REQUIRE c.k IS :: INTEGER",
    );
    crate::storage::create_checkpoint(&db, crate::storage::branch_head(&db, "main").unwrap())
        .unwrap();
    run(&db, "CREATE RANGE INDEX b_values FOR(b:B) ON(b.k)");
    db
}

struct Evidence {
    selected_bound: bool,
    existing: u64,
    sampled: u64,
    hits: u64,
}

fn inspect(db: &Connection, patterns: &str, rare_part: usize) -> Evidence {
    let text = format!(
        "MATCH {patterns} WHERE b.k=c.k RETURN elementId(a),elementId(r),elementId(b),elementId(c)"
    );
    db.execute_batch("SAVEPOINT bound_source_read").unwrap();
    let prepared = prepare(db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let step = &prepared.matches[0];
    let plan = step.join.as_ref().expect("actual cost-selected Join");
    let snapshot = crate::storage::Snapshot::from_resolved_state(
        db,
        prepared.resolved_state.as_ref().unwrap(),
    );
    let schema = snapshot.schema_state().unwrap();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let context = JoinPlanningContext {
        snapshot: &snapshot,
        schema: &schema,
        view: &prepared.graph_view,
        params: &prepared.params,
        control: &control,
    };
    let analysis = JoinAnalysis {
        hashes: plan.hashes.clone(),
        predicates: plan.predicates.clone(),
        templates: plan.templates.clone(),
        edges: Vec::new(),
    };
    let estimates = collect_join_estimates(
        step,
        &analysis,
        &context,
        &prepared.statistics,
        &mut PlanningBudget::default(),
        &JoinClauseScope {
            clause_start: plan.clause_start,
            snapshot_identity: plan.snapshot_identity,
            incoming: &[],
            declared_columns: &plan.declared_columns,
        },
    )
    .unwrap();
    let prefix = 1_u128 << rare_part;
    let template = analysis
        .templates
        .iter()
        .position(|template| {
            template.part == 1
                && template.target == ProbeTarget::StartNode
                && matches!(template.access, ProbeAccess::Adjacency { .. })
                && template.source_guard.0 & prefix != 0
        })
        .unwrap();
    let existing = estimates
        .access(1, PartOrientation::Written, StageAccess::Existing, prefix)
        .unwrap();
    let sampled = estimates
        .access(
            1,
            PartOrientation::Written,
            StageAccess::Probe(template),
            prefix,
        )
        .unwrap();
    assert_eq!(
        sampled.rows.unwrap().expected,
        FANOUT as u64,
        "actual complete Rare cohort"
    );
    let selected_bound = selected_bound(plan, &estimates, patterns, prefix);
    let (rows, profile) = profile_rows(db, &format!("PROFILE {text}"));
    assert_eq!(
        rows,
        vec![vec![
            Value::String("n:1".into()),
            Value::String("r:1".into()),
            Value::String("n:2".into()),
            Value::String("n:34".into())
        ]]
    );
    eprintln!(
        "bound-source patterns={patterns} existing={existing:?} known_source={sampled:?} selected_bound={selected_bound} actual_rows={rows:?} hits={} operators={:?}",
        profile.metrics.db_hits,
        profile.metrics.operator_profile()
    );
    db.execute_batch("RELEASE bound_source_read").unwrap();
    Evidence {
        selected_bound,
        existing: existing.rows.unwrap().expected,
        sampled: sampled.rows.unwrap().expected,
        hits: profile.metrics.db_hits,
    }
}

fn selected_bound(
    plan: &JoinPlan,
    estimates: &JoinEstimates,
    patterns: &str,
    prefix: u128,
) -> bool {
    let mut mask = 0;
    let mut selected_bound = false;
    for stage in &plan.stages {
        if stage.part == 1
            && stage.orientation == PartOrientation::Written
            && stage.access == StageAccess::Existing
            && mask & prefix != 0
        {
            selected_bound = true;
        }
        mask |= 1_u128 << stage.part;
        eprintln!(
            "bound-source patterns={patterns} prefix={mask:b} stage={stage:?} rows={:?}",
            estimates.rows(mask)
        );
    }
    selected_bound
}

fn profile_rows(db: &Connection, text: &str) -> (Vec<Vec<Value>>, crate::query::QuerySummary) {
    let prepared = prepare(db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(db, 32).unwrap();
        rows.extend(batch.rows);
        if batch.done {
            return (rows, cursor.complete(db).unwrap());
        }
    }
}

#[test]
fn bound_source_cohort_diagnostic_uses_the_selected_physical_producer() {
    let db = fixture();
    let written = inspect(&db, "(a:Rare),(a)-[r:R]->(b:B),(c:Chosen)", 0);
    let rotated = inspect(&db, "(c:Chosen),(a)-[r:R]->(b:B),(a:Rare)", 2);
    for evidence in [written, rotated] {
        assert_eq!(
            evidence.existing, evidence.sampled,
            "native bound adjacency must consume its already sampled source cohort"
        );
        assert!(
            !evidence.selected_bound,
            "the measured reverse Index route avoids the hot hub expansion"
        );
        assert!(
            evidence.hits < 227 / 4,
            "actual work must improve the frozen 227-hit counterexample: {}",
            evidence.hits
        );
    }
}
