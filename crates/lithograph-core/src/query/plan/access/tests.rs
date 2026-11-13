use super::*;

#[test]
fn optional_sample_preserves_completion_metadata_without_debiting_it() {
    let resources = crate::query::QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut budget = PlanningBudget::default();
    budget.begin_clause(2);
    let remaining = budget.remaining_bytes();
    budget.reserve_advice_bytes(4096);
    assert!(budget.work(3, remaining - 4096));
    let paid = (budget.states, budget.bytes);
    assert!(
        sampling::claim(1, 1, &mut budget, &control)
            .unwrap()
            .is_none()
    );
    assert_eq!((budget.states, budget.bytes), paid);
    assert!(!budget.exhausted());
    assert_eq!(resources.used(), 0);
    assert!(
        budget.work(1, 4096),
        "real construction uses the protected bytes"
    );
    budget.end_clause();
    assert_eq!((budget.states, budget.bytes), (paid.0 + 1, remaining));
    assert_eq!(budget.remaining_advice_bytes(), budget.remaining_bytes());
}

#[test]
fn soft_eligible_step_refusal_does_not_stop_later_matches() {
    let resources = crate::query::QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let node = NodeSpec {
        variable: Some("n".into()),
        labels: Vec::new(),
        label_names: Vec::new(),
        scan_label: None,
        scan_label_name: None,
        index_seek: None,
        id_seek: None,
        impossible: false,
    };
    let step = MatchStep {
        clause_start: 0,
        optional: false,
        parts: vec![PatternPart {
            path_variable: None,
            start: node,
            relationship: None,
            end: None,
        }],
        predicate: None,
        early_predicates: Vec::new(),
        join: None,
    };
    let schema = storage::SchemaState::default();
    let params = BTreeMap::new();
    let mut budget = PlanningBudget::default();
    budget.begin_clause(2);
    assert!(budget.work(super::super::budget::MAX_STATES / 2, 0));
    assert!(!eligible_step(&step, &schema, &params, &control, &mut budget).unwrap());
    assert!(budget.exhausted());
    assert!(
        !budget.exhausted,
        "the real access guard must preserve a local refusal"
    );
    let retained = (budget.states, budget.bytes, budget.candidates);
    budget.end_clause();
    assert!(!budget.exhausted());
    assert_eq!(retained, (budget.states, budget.bytes, budget.candidates));
    budget.begin_clause(2);
    assert!(eligible_step(&step, &schema, &params, &control, &mut budget).unwrap());
    budget.end_clause();
    assert!(!budget.exhausted());
    assert_eq!(budget.states, retained.0 + 1);
    assert_eq!(resources.used(), 0);
}

#[test]
fn unknown_estimates_do_not_claim_zero_or_override_the_baseline() {
    let unknown = AccessCandidate::scan(AccessPath::Scan, None);
    let empty = AccessCandidate::scan(AccessPath::Label(1), Some(0));
    assert_eq!(unknown.rows, None);
    assert!(!empty.beats(unknown));
    assert!(empty.beats(AccessCandidate::scan(AccessPath::Label(2), Some(100))));
}

#[test]
fn optimizer_limits_are_deterministic_and_separate_from_execution_quota() {
    let resources = crate::query::QueryResources::new(Some(1));
    let mut budget = PlanningBudget::default();
    for _ in 0..super::super::budget::MAX_CANDIDATES {
        assert!(budget.candidate());
    }
    assert!(!budget.candidate());
    assert!(budget.exhausted);
    assert_eq!(resources.used(), 0);
    assert!(resources.reserve(2).is_err());
}

#[test]
fn optional_samples_share_remaining_work_and_release_their_workspace() {
    let resources = crate::query::QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut budget = PlanningBudget::default();
    assert!(
        sampling::claim(usize::MAX, 4096, &mut budget, &control)
            .unwrap()
            .is_none()
    );
    assert_eq!((budget.states, budget.bytes, resources.used()), (0, 0, 0));
    assert!(!budget.exhausted());
    for _ in 0..4 {
        let memory = sampling::claim(1024, 16 * 1024, &mut budget, &control)
            .unwrap()
            .unwrap();
        assert_eq!(resources.used(), 16 * 1024);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }
    assert!(
        sampling::claim(1, 1, &mut budget, &control)
            .unwrap()
            .is_none()
    );
    assert_eq!(budget.states, super::super::budget::MAX_STATES);
    assert_eq!(budget.bytes, 64 * 1024);
}

#[test]
fn optional_workspace_shortage_is_unknown_and_cancellation_still_propagates() {
    let resources = crate::query::QueryResources::new(Some(1));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut budget = PlanningBudget::default();
    assert!(
        sampling::claim(256, 4096, &mut budget, &control)
            .unwrap()
            .is_none()
    );
    assert_eq!((budget.states, budget.bytes, resources.used()), (0, 0, 0));
    let interrupted = ExecutionControl::new(&resources, &|| true);
    let error = sampling::claim(256, 4096, &mut budget, &interrupted).unwrap_err();
    assert_eq!(error.kind, crate::query::QueryErrorKind::Interrupted);
    assert_eq!(resources.used(), 0);
}

#[test]
fn parameter_capacity_rejects_before_inspecting_children_or_charging_execution() {
    let resources = crate::query::QueryResources::new(Some(1));
    let control = ExecutionControl::new(&resources, &|| false);
    for capacity in [10_000, 100_000] {
        let mut values = Vec::with_capacity(capacity);
        values.push(Value::Integer(1));
        let params = BTreeMap::from([("x".to_owned(), Value::List(values))]);
        let mut budget = PlanningBudget::default();
        assert_eq!(budget.parameter_bytes(&params, &control).unwrap(), None);
        assert!(budget.exhausted());
        // One root map, one parameter key and its List header, regardless of
        // how much capacity or how many children the collection could retain.
        assert_eq!(budget.states, 3);
        assert!(budget.bytes <= super::super::budget::MAX_BYTES);
        assert_eq!(resources.used(), 0);
    }
}

#[test]
fn parameter_maps_depth_and_remaining_states_share_the_planning_bound() {
    let resources = crate::query::QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let many = (0..10_000)
        .map(|index| (format!("p{index}"), Value::Null))
        .collect::<BTreeMap<_, _>>();
    let mut budget = PlanningBudget::default();
    assert_eq!(budget.parameter_bytes(&many, &control).unwrap(), None);
    assert!(budget.exhausted());
    assert_eq!(
        budget.states, 0,
        "map capacity is checked before its entries"
    );

    let mut nested = Value::Null;
    for _ in 0..super::super::budget::MAX_DEPTH + 8 {
        nested = Value::Map(BTreeMap::from([("x".to_owned(), nested)]));
    }
    let params = BTreeMap::from([("nested".to_owned(), nested)]);
    let mut budget = PlanningBudget::default();
    assert_eq!(budget.parameter_bytes(&params, &control).unwrap(), None);
    assert!(budget.exhausted());
    assert_eq!(budget.depth, super::super::budget::MAX_DEPTH + 1);
    assert!(budget.states <= super::super::budget::MAX_STATES);

    let params = BTreeMap::from([("x".to_owned(), Value::Integer(1))]);
    let mut budget = PlanningBudget::default();
    assert!(budget.work(super::super::budget::MAX_STATES - 2, 0));
    assert_eq!(budget.parameter_bytes(&params, &control).unwrap(), None);
    assert!(budget.exhausted());
    assert_eq!(budget.states, super::super::budget::MAX_STATES - 2);
    assert_eq!(resources.used(), 0);
}

#[test]
fn nested_map_clone_workspace_is_bounded_separately_from_the_borrowed_parameter_map() {
    let values = Value::List(
        (0..500)
            .map(|_| Value::Map(BTreeMap::from([("v".to_owned(), Value::Integer(1))])))
            .collect(),
    );
    assert!(
        crate::query::resources::value_bytes(&values).unwrap()
            > super::super::budget::MAX_BYTES as u64
    );
    let params = BTreeMap::from([("x".to_owned(), values)]);
    let resources = crate::query::QueryResources::new(Some(1));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut budget = PlanningBudget::default();
    assert_eq!(budget.parameter_bytes(&params, &control).unwrap(), None);
    assert!(budget.exhausted());
    assert!(
        budget.states < 500,
        "reject before walking all cloned Map nodes"
    );
    assert!(budget.bytes <= super::super::budget::MAX_BYTES);
    assert_eq!(resources.used(), 0);
}

fn consume(
    connection: &rusqlite::Connection,
    prepared: crate::query::PreparedQuery,
) -> QueryResult<Vec<Vec<Value>>> {
    let mut cursor = crate::query::QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 16)?;
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection)?;
            return Ok(rows);
        }
    }
}

fn indexed_fixture() -> rusqlite::Connection {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY, magic TEXT, database_id TEXT, storage_format INTEGER)").unwrap();
    storage::create_storage_schema(&connection).unwrap();
    storage::initialize_root(&connection).unwrap();
    for query in [
        "CREATE (:Common {v:1}),(:Common {v:2}) FINISH",
        "CREATE CONSTRAINT value_type FOR (n:Common) REQUIRE n.v IS :: INTEGER",
        "CREATE RANGE INDEX values_range FOR (n:Common) ON (n.v)",
    ] {
        let prepared = crate::query::prepare(
            &connection,
            query,
            BTreeMap::new(),
            crate::query::ExecutionOptions::default(),
        )
        .unwrap();
        consume(&connection, prepared).unwrap();
    }
    connection
}

fn large_list_params(value: i64) -> BTreeMap<String, Value> {
    let mut values = Vec::with_capacity(10_000);
    values.push(Value::Integer(value));
    BTreeMap::from([("x".to_owned(), Value::List(values))])
}

#[test]
fn oversized_parameters_fall_back_without_changing_rows_errors_or_execution_quota() {
    let connection = indexed_fixture();
    let query = "MATCH (n:Common) WHERE n.v IN $x RETURN n.v";
    for (params, expected) in [
        (large_list_params(1), vec![vec![Value::Integer(1)]]),
        (large_list_params(99), Vec::new()),
        (
            (0..10_000)
                .map(|index| (format!("p{index}"), Value::Null))
                .chain([("x".to_owned(), Value::List(vec![Value::Integer(1)]))])
                .collect(),
            vec![vec![Value::Integer(1)]],
        ),
    ] {
        let prepared = crate::query::prepare(
            &connection,
            query,
            params,
            crate::query::ExecutionOptions::default(),
        )
        .unwrap();
        assert!(!prepared.physical.explain().contains("IndexSeek"));
        assert_eq!(consume(&connection, prepared).unwrap(), expected);
    }
    let prepared = crate::query::prepare(
        &connection,
        "MATCH (n:Common) WHERE n.v IN $x AND 1/0=1 RETURN n.v",
        large_list_params(1),
        crate::query::ExecutionOptions::default(),
    )
    .unwrap();
    assert_eq!(
        consume(&connection, prepared).unwrap_err().kind,
        crate::query::QueryErrorKind::Type
    );
    let options = crate::query::ExecutionOptions {
        memory_limit_bytes: Some(1024),
        ..Default::default()
    };
    assert_eq!(
        crate::query::prepare(&connection, query, large_list_params(1), options)
            .unwrap_err()
            .kind,
        crate::query::QueryErrorKind::Resource
    );
}

#[cfg(feature = "test-support")]
#[test]
fn transaction_prefix_reuses_immutable_state_while_batches_observe_new_heads() {
    for nodes in [12_i64, 24] {
        let connection = indexed_fixture();
        for query in [
            format!("UNWIND range(1,{nodes}) AS i CREATE (:Probe {{k:i}}) FINISH"),
            "CREATE (:Counter {v:0}) FINISH".to_owned(),
        ] {
            let prepared = crate::query::prepare(
                &connection,
                &query,
                BTreeMap::new(),
                crate::query::ExecutionOptions::default(),
            )
            .unwrap();
            consume(&connection, prepared).unwrap();
        }
        storage::create_checkpoint(
            &connection,
            storage::branch_head(&connection, "main").unwrap(),
        )
        .unwrap();
        crate::performance::set_enabled(true);
        crate::performance::reset();
        let prepared = crate::query::prepare(
            &connection,
            "MATCH (n:Probe) CALL (n) { MATCH (c:Counter) SET c.v=c.v+1 SET n.hit=1 } \
             IN TRANSACTIONS OF 4 ROWS FINISH",
            BTreeMap::new(),
            crate::query::ExecutionOptions::default(),
        )
        .unwrap();
        assert_eq!(crate::performance::snapshot().resolved_state_builds, 1);
        consume(&connection, prepared).unwrap();
        let builds = crate::performance::snapshot().resolved_state_builds;
        crate::performance::set_enabled(false);
        // One prepared prefix, one batch-option evaluation, and one fresh
        // immutable base for each independently committed four-row batch.
        assert_eq!(builds, (nodes / 4 + 2) as u64);
        let prepared = crate::query::prepare(
            &connection,
            "MATCH (c:Counter) RETURN c.v",
            BTreeMap::new(),
            crate::query::ExecutionOptions::default(),
        )
        .unwrap();
        assert_eq!(
            consume(&connection, prepared).unwrap(),
            vec![vec![Value::Integer(nodes)]]
        );
    }
}
