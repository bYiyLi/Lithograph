use super::*;

#[test]
fn dynamic_filter_optional_claim_cannot_make_a_resident_hash_require_temp() {
    let row = BindingRow::default();
    let row_bytes = expression::binding_row_bytes(&row).unwrap();
    let key_bytes = std::mem::size_of::<Value>() as u64;
    let probe_resources = QueryResources::default();
    let probe_control = ExecutionControl::new(&probe_resources, &|| false);
    let table = HashTable::new(&probe_control).unwrap();
    let projected = table.projected_bytes(row_bytes, key_bytes).unwrap();
    let headroom = row_headroom(row_bytes).unwrap();
    drop(table);
    assert_eq!(probe_resources.used(), 0);

    let limit = checked_add(projected, headroom).unwrap();
    let resources = QueryResources::new(Some(limit));
    let control = ExecutionControl::new(&resources, &|| false);
    let key = EvaluatedHashKey {
        digest: [0; 32],
        value: key::HashKeyValue::Scalar(Value::Integer(7)),
        memory: control.reserve(key_bytes).unwrap(),
    };
    let input = (row, control.reserve(row_bytes).unwrap());
    let mut state = HashBuildState::new();
    state.storage = BuildStorage::Memory(HashTable::new(&control).unwrap());
    let mut filter = dynamic_filter::KeyCollector::new(&control)
        .unwrap()
        .unwrap();
    assert!(filter.observe(&key, &control).unwrap());
    let incoming = checked_add(row_bytes, key_bytes).unwrap();
    assert!(hash_resident_target(&resources, 0, incoming, headroom) < projected);
    assert_eq!(
        hash_resident_target(&resources, filter.owned_bytes(), incoming, headroom),
        projected,
        "only the optional allocation crosses the original resident boundary"
    );
    state.filter = Some(filter);
    state.keep_build_row(input, key, &control).unwrap();
    assert!(
        state.filter.is_none(),
        "an incomplete advisory set is discarded"
    );
    let BuildStorage::Memory(table) = &state.storage else {
        panic!("optional state must not force a new TEMP requirement")
    };
    assert_eq!(table.len(), 1);
    assert!(resources.peak() <= limit);
    drop(state);
    assert_eq!(resources.used(), 0);
}

fn finite_build_fixture() -> (rusqlite::Connection, crate::storage::HashId, Vec<i64>) {
    use crate::storage;
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT NOT NULL,\
         database_id TEXT NOT NULL,storage_format INTEGER NOT NULL)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO _lithograph_meta VALUES(1,'lithograph-format-v1',\
         '00000000-0000-4000-8000-000000000027',?1)",
        [storage::STORAGE_FORMAT],
    )
    .unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    let head = storage::branch_head(&db, "main").unwrap();
    let label = storage::intern_label(&db, "Build").unwrap();
    let mut layer = storage::LayerBuilder::default();
    let mut ids = Vec::with_capacity(64);
    for _ in 0..64 {
        let id = storage::allocate_node_id(&db).unwrap();
        layer.add_node(id).unwrap();
        layer.add_label(id, label).unwrap();
        ids.push(id);
    }
    let commit = storage::commit_layer(
        &db,
        "main",
        head,
        None,
        &layer,
        &storage::CommitMetadata {
            author: None,
            message: None,
            committed_at: 1,
        },
    )
    .unwrap();
    storage::create_checkpoint(&db, commit).unwrap();
    (db, commit, ids)
}

fn finite_identity_step(db: &rusqlite::Connection) -> MatchStep {
    use crate::query::plan::join::{PartOrientation, StageAccess};
    let mut prepared = crate::query::prepare(
        db,
        "MATCH(a:Build),(d:Build) WHERE a=d RETURN elementId(d)",
        std::collections::BTreeMap::new(),
        crate::query::ExecutionOptions::default(),
    )
    .unwrap();
    let mut step = prepared.matches.remove(0);
    drop(prepared);
    let plan = std::sync::Arc::get_mut(step.join.as_mut().unwrap()).unwrap();
    assert_eq!(plan.hashes.len(), 1);
    assert_eq!(plan.hashes[0].family, HashFamily::NodeIdentity);
    let inner_side = if plan.hashes[0].left.variable == "d" {
        HashOperandSide::Left
    } else {
        HashOperandSide::Right
    };
    assert_eq!(plan.hashes[0].keys(inner_side).0.variable, "d");
    assert_eq!(step.parts[0].start.variable.as_deref(), Some("a"));
    assert_eq!(step.parts[1].start.variable.as_deref(), Some("d"));
    plan.stages[0].part = 0;
    plan.stages[0].orientation = PartOrientation::Written;
    plan.stages[0].access = StageAccess::Existing;
    plan.stages[0].cartesian = false;
    plan.stages[0].predicates_after.clear();
    plan.stages[1].part = 1;
    plan.stages[1].orientation = PartOrientation::Written;
    plan.stages[1].access = StageAccess::Hash {
        equality: 0,
        inner_side,
        dynamic_filter: None,
    };
    plan.stages[1].cartesian = false;
    step
}

fn finite_build_metrics(step: &MatchStep, resources: &QueryResources) -> QueryMetrics {
    use crate::query::plan::{OperatorSite, PhysicalOperator, PhysicalPlan, ProfileBinding};
    let physical = PhysicalPlan {
        operators: vec![
            PhysicalOperator::NodeIdSeek {
                variable: "d".into(),
            },
            PhysicalOperator::HashJoin,
        ],
    };
    let mut metrics = QueryMetrics::for_execution_with_resources(
        crate::cypher::ExecutionMode::Profile,
        &physical,
        resources,
    )
    .unwrap();
    metrics
        .install_profile_sites(
            [
                ProfileBinding {
                    site: OperatorSite {
                        clause_start: step.clause_start,
                        part: Some(1),
                        role: ProfileRole::NodeAccess,
                    },
                    physical_slot: 0,
                },
                ProfileBinding {
                    site: OperatorSite {
                        clause_start: step.clause_start,
                        part: Some(1),
                        role: ProfileRole::Join,
                    },
                    physical_slot: 1,
                },
            ]
            .into_iter(),
        )
        .unwrap();
    metrics
}

#[test]
fn asp_hash_mid_build_interrupts_after_a_real_complete_id_page() {
    use std::cell::Cell;
    let (db, commit, ids) = finite_build_fixture();
    let step = finite_identity_step(&db);
    let resources = QueryResources::default();
    let cancelled = Cell::new(false);
    let interrupt = || cancelled.get();
    let control = ExecutionControl::new(&resources, &interrupt);
    let snapshot =
        crate::query::graph::resolve_snapshot_with_control(&db, commit, &control).unwrap();
    let mut metrics = finite_build_metrics(&step, &resources);
    let before = metrics.activate_profile_slot(None);
    metrics.restore_access_operator(before);
    let page_memory = control
        .reserve(allocation_bytes::<i64>(ids.capacity()).unwrap())
        .unwrap();
    let calls = Cell::new(0);
    let mut state = HashBuildState::new();
    let error = {
        let mut first_page = Some((ids, page_memory));
        let mut pages = || {
            calls.set(calls.get() + 1);
            if let Some(page) = first_page.take() {
                return Ok(Some(page));
            }
            cancelled.set(true);
            control.check()?;
            Ok(None)
        };
        state
            .ensure_build_from_id_pages(
                &step,
                1,
                &mut pages,
                (&snapshot, &ResolvedGraphView::default()),
                &mut metrics,
                &control,
            )
            .unwrap_err()
    };
    assert_eq!(error.kind, crate::query::QueryErrorKind::Interrupted);
    assert_eq!(
        calls.get(),
        2,
        "the second request follows the complete first page"
    );
    assert_eq!(state.runtime_stats().build_rows, 64);
    assert_eq!(state.runtime_stats().probe_rows, 0);
    assert!(
        !state.built,
        "no completed build is published after source interruption"
    );
    let BuildStorage::Memory(table) = &state.storage else {
        panic!("resident test build")
    };
    assert_eq!(table.len(), 64);
    let profile = metrics.operator_profile().unwrap();
    assert_eq!(profile[0].rows, 64);
    assert!(profile[0].db_hits > 0, "actual canonical Node point reads");
    assert_eq!(
        profile.iter().map(|operator| operator.db_hits).sum::<u64>(),
        metrics.db_hits
    );
    assert!(profile.iter().all(|operator| operator.semi_mask.is_none()));
    let restored = metrics.activate_profile_slot(None);
    assert_eq!(restored, before);
    metrics.restore_access_operator(restored);
    drop(state);
    drop(snapshot);
    drop(metrics);
    assert_eq!(resources.used(), 0);
}
