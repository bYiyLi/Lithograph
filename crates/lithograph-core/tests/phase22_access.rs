use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryErrorKind, QuerySummary, SnapshotSelector, prepare,
};
use lithograph_core::storage::{self, CommitMetadata, LayerBuilder, OwnerKind, PropertyValue};
use rusqlite::Connection;

fn run(
    db: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> (Vec<Vec<Value>>, QuerySummary) {
    let prepared = prepare(db, query, params, options).expect(query);
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    let summary = loop {
        let batch = cursor.next_batch(db, 32).expect(query);
        rows.extend(batch.rows);
        if batch.done {
            break batch.summary.expect("terminal summary");
        }
    };
    drop(cursor);
    // The returned PROFILE summary owns its own bounded metrics lease.
    assert!(resources.used() <= resources.peak());
    (rows, summary)
}

fn query(db: &Connection, text: &str) -> Vec<Vec<Value>> {
    run(db, text, BTreeMap::new(), ExecutionOptions::default()).0
}

fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY, magic TEXT, database_id TEXT, storage_format INTEGER);").unwrap();
    storage::create_storage_schema(&db).unwrap();
    let root = storage::initialize_root(&db).unwrap().root;
    let common = storage::intern_label(&db, "Common").unwrap();
    let rare = storage::intern_label(&db, "Rare").unwrap();
    let value = storage::intern_property_key(&db, "v").unwrap();
    let mut layer = LayerBuilder::default();
    for index in 0..256 {
        let id = storage::allocate_node_id(&db).unwrap();
        layer.add_node(id).unwrap();
        layer.add_label(id, common).unwrap();
        if index < 2 {
            layer.add_label(id, rare).unwrap();
        }
        layer
            .set_property(OwnerKind::Node, id, value, PropertyValue::Integer(index))
            .unwrap();
    }
    storage::commit_layer(
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
    query(
        &db,
        "CREATE CONSTRAINT value_type FOR (n:Common) REQUIRE n.v IS :: INTEGER",
    );
    query(
        &db,
        "CREATE RANGE INDEX values_range FOR (n:Common) ON (n.v)",
    );
    checkpoint(&db);
    db
}

fn checkpoint(db: &Connection) {
    storage::create_checkpoint(db, storage::branch_head(db, "main").unwrap()).unwrap();
}

fn params(value: Value) -> BTreeMap<String, Value> {
    BTreeMap::from([("x".to_owned(), value)])
}

fn view() -> ExecutionOptions {
    ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Rare"]}}"#).unwrap()
}

fn explain(db: &Connection, text: &str, value: Value, options: ExecutionOptions) -> String {
    let rows = run(db, &format!("EXPLAIN {text}"), params(value), options).0;
    let Value::String(plan) = &rows[0][0] else {
        panic!("plan string")
    };
    plan.clone()
}

fn assert_access(summary: &QuerySummary, name: &str) {
    let operators = summary.metrics.operator_profile().expect("PROFILE metrics");
    assert!(
        operators
            .iter()
            .any(|op| op.operator == name && op.db_hits > 0),
        "{operators:?}"
    );
}

#[test]
fn cost_selects_real_index_or_visible_label_and_profile_matches() {
    let db = fixture();
    let text = "MATCH (n:Common) WHERE n.v=$x RETURN n.v";
    for (options, expected) in [
        (ExecutionOptions::default(), "IndexSeek"),
        (view(), "LabelIndexScan"),
    ] {
        let plan = explain(&db, text, Value::Integer(1), options.clone());
        assert!(plan.contains(expected), "{plan}");
        if expected == "LabelIndexScan" {
            assert!(
                plan.contains("Rare") && !plan.contains("IndexSeek"),
                "{plan}"
            );
        }
        let (rows, profile) = run(
            &db,
            &format!("PROFILE {text}"),
            params(Value::Integer(1)),
            options,
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
        assert_access(&profile, expected);
    }
}

#[test]
fn parameter_selectivity_is_replanned_deterministically() {
    let db = fixture();
    let text = "MATCH (n:Common) WHERE n.v IN $x RETURN n.v ORDER BY n.v";
    for count in [1, 64, 1, 64] {
        let value = Value::List((0..count).map(Value::Integer).collect());
        let plan = explain(&db, text, value.clone(), ExecutionOptions::default());
        assert!(
            plan.contains(if count == 1 {
                "IndexSeek"
            } else {
                "LabelIndexScan"
            }),
            "{plan}"
        );
        assert_eq!(
            run(&db, text, params(value), ExecutionOptions::default()).0,
            (0..count)
                .map(|n| vec![Value::Integer(n)])
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn program_read_and_profile_share_the_costed_match_cursor() {
    let db = fixture();
    let text = "WITH 1 AS seed MATCH (n:Common) WHERE n.v=$x RETURN n.v, seed";
    for (options, expected) in [
        (ExecutionOptions::default(), "IndexSeek"),
        (view(), "LabelIndexScan"),
    ] {
        let plan = explain(&db, text, Value::Integer(1), options.clone());
        assert!(plan.contains(expected), "{plan}");
        let ordinary = run(&db, text, params(Value::Integer(1)), options.clone()).0;
        let (rows, profile) = run(
            &db,
            &format!("PROFILE {text}"),
            params(Value::Integer(1)),
            options,
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(1)]]);
        assert_eq!(ordinary, rows);
        assert_access(&profile, expected);
    }
}

#[test]
fn write_match_uses_the_displayed_costed_access_and_rolls_back_on_error() {
    let db = fixture();
    for (options, expected) in [
        (ExecutionOptions::default(), "IndexSeek"),
        (view(), "LabelIndexScan"),
    ] {
        let text = "MATCH (n:Common) WHERE n.v=$x SET n.touched=1 RETURN n.v";
        assert!(explain(&db, text, Value::Integer(1), options.clone()).contains(expected));
        let (rows, summary) = run(
            &db,
            &format!("PROFILE {text}"),
            params(Value::Integer(1)),
            options.clone(),
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
        assert_access(&summary, expected);
        let before = storage::branch_head(&db, "main").unwrap();
        let bad = prepare(
            &db,
            "MATCH (n:Common) WHERE n.v=$x SET n.touched=2 RETURN 1/0",
            params(Value::Integer(1)),
            options,
        )
        .unwrap();
        let mut cursor = QueryCursor::new(bad);
        assert!(cursor.next_batch(&db, 1).is_err());
        drop(cursor);
        assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
        assert_eq!(
            query(&db, "MATCH (n:Common) WHERE n.v=1 RETURN n.touched"),
            vec![vec![Value::Integer(1)]]
        );
    }
}

#[test]
fn historical_schema_type_and_missing_statistics_never_supply_false_proofs() {
    let db = fixture();
    let old = storage::branch_head(&db, "main").unwrap();
    query(&db, "DROP CONSTRAINT value_type");
    query(&db, "CREATE (:Common {v:'mixed'})");
    let text = "MATCH (n:Common) WHERE n.v=$x RETURN n.v";
    let mut options = ExecutionOptions::default();
    options.snapshot = SnapshotSelector::Commit(old.to_hex());
    assert!(explain(&db, text, Value::Integer(1), options.clone()).contains("IndexSeek"));
    assert_eq!(
        run(&db, text, params(Value::Integer(1)), options).0,
        vec![vec![Value::Integer(1)]]
    );
    let prepared = prepare(
        &db,
        text,
        params(Value::Integer(1)),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert!(!prepared.physical.explain().contains("IndexSeek"));
    let mut cursor = QueryCursor::new(prepared);
    let error = loop {
        match cursor.next_batch(&db, 32) {
            Err(error) => break error,
            Ok(batch) => assert!(!batch.done),
        }
    };
    assert_eq!(error.kind, QueryErrorKind::Type);
    drop(cursor);
    db.execute("UPDATE _lithograph_checkpoints SET metadata=NULL", [])
        .unwrap();
    let mut historical_view = view();
    historical_view.snapshot = SnapshotSelector::Commit(old.to_hex());
    assert_eq!(
        run(&db, text, params(Value::Integer(1)), historical_view).0,
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n:Common) WHERE elementId(n)=$x RETURN n.v",
            params(Value::String("n:2".into())),
            view()
        )
        .0,
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn index_candidates_do_not_suppress_errors_before_or_after_nullable_predicates() {
    let db = fixture();
    query(&db, "CREATE (:Common)");
    for text in [
        "MATCH (n:Common) WHERE 1/0=0 AND n.v=$x RETURN n.v",
        "MATCH (n:Common) WHERE n.v=$x AND 1/0=0 RETURN n.v",
        "MATCH (n:Common) WHERE n.missing=$x AND 1/0=0 RETURN n.v",
    ] {
        let prepared = prepare(
            &db,
            text,
            params(Value::Integer(99999)),
            ExecutionOptions::default(),
        )
        .unwrap();
        let mut cursor = QueryCursor::new(prepared);
        assert!(
            cursor.next_batch(&db, 1).is_err(),
            "index suppressed an observable error: {text}"
        );
    }
}

#[test]
fn selected_access_releases_resources_and_unpublished_writes_on_close_or_cancel() {
    let db = fixture();
    for options in [ExecutionOptions::default(), view()] {
        for (write, program) in [(false, false), (true, false), (false, true), (true, true)] {
            let before = storage::branch_head(&db, "main").unwrap();
            let text = if write {
                "MATCH (n:Common) WHERE n.v=$x SET n.closed=1 RETURN n.v"
            } else {
                "MATCH (n:Common) WHERE n.v=$x RETURN n.v"
            };
            let text = if program {
                format!("WITH 1 AS seed {text}")
            } else {
                text.to_owned()
            };
            let prepared = prepare(&db, &text, params(Value::Integer(1)), options.clone()).unwrap();
            let resources = prepared.resources.clone();
            let mut cursor = QueryCursor::new(prepared);
            let batch = cursor.next_batch(&db, 1).unwrap();
            assert_eq!(batch.rows, vec![vec![Value::Integer(1)]]);
            drop(batch);
            cursor.cancel(&db).unwrap();
            drop(cursor);
            assert_eq!(resources.used(), 0);
            if write {
                assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
            }
            let prepared = prepare(&db, &text, params(Value::Integer(1)), options.clone()).unwrap();
            let resources = prepared.resources.clone();
            let mut cursor = QueryCursor::new(prepared);
            assert_eq!(
                cursor
                    .next_batch_with_interrupt(&db, 1, &|| true)
                    .unwrap_err()
                    .kind,
                QueryErrorKind::Interrupted
            );
            drop(cursor);
            assert_eq!(resources.used(), 0);
            assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
        }
    }
}

#[cfg(feature = "test-support")]
#[test]
fn recursive_analysis_limit_and_planning_cancellation_remain_distinct_from_execution_quota() {
    let db = fixture();
    lithograph_core::performance::set_enabled(true);
    lithograph_core::performance::reset();
    let text = "MATCH (n:Common) WHERE ".to_owned()
        + &(0..96).map(|_| "n.v=$x").collect::<Vec<_>>().join(" AND ")
        + " RETURN n.v";
    let prepared = prepare(
        &db,
        &text,
        params(Value::Integer(1)),
        ExecutionOptions::default(),
    )
    .unwrap();
    let counters = lithograph_core::performance::snapshot();
    assert_eq!(counters.planner_fallbacks, 1);
    assert_eq!(counters.planner_depth, 65);
    assert!(!prepared.physical.explain().contains("IndexSeek"));
    let mut cursor = QueryCursor::new(prepared);
    assert_eq!(
        cursor.next_batch(&db, 1).unwrap().rows,
        vec![vec![Value::Integer(1)]]
    );
    drop(cursor);
    let resources = lithograph_core::query::QueryResources::new(Some(1024 * 1024));
    let result = lithograph_core::query::prepare_with_resources_and_interrupt(
        &db,
        &text,
        params(Value::Integer(1)),
        ExecutionOptions::default(),
        resources.clone(),
        &|| true,
    );
    assert_eq!(result.unwrap_err().kind, QueryErrorKind::Interrupted);
    assert_eq!(resources.used(), 0);
    lithograph_core::performance::set_enabled(false);
}

#[test]
fn many_independent_patterns_fall_back_without_inventing_empty_results() {
    let db = fixture();
    query(&db, "CREATE (:Singleton)");
    for count in [1, 16, 128, 256] {
        let parts = (0..count)
            .map(|i| format!("(n{i}:Singleton)"))
            .collect::<Vec<_>>()
            .join(",");
        let text = format!("MATCH {parts} RETURN 7");
        let (rows, _) = run(&db, &text, BTreeMap::new(), ExecutionOptions::default());
        assert_eq!(rows, vec![vec![Value::Integer(7)]]);
    }
}

#[cfg(feature = "test-support")]
#[test]
fn planner_reports_actual_bounded_search_and_fallback() {
    let db = fixture();
    query(&db, "CREATE (:Singleton)");
    lithograph_core::performance::set_enabled(true);
    for count in [16, 128, 256, 600] {
        lithograph_core::performance::reset();
        let text = if count == 600 {
            (0..count)
                .map(|i| format!("MATCH (n{i}:Singleton) "))
                .collect::<String>()
                + "RETURN 7"
        } else {
            let parts = (0..count)
                .map(|i| format!("(n{i}:Singleton)"))
                .collect::<Vec<_>>()
                .join(",");
            format!("MATCH {parts} RETURN 7")
        };
        let started = std::time::Instant::now();
        let prepared = prepare(&db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
        let micros = started.elapsed().as_micros();
        let counters = lithograph_core::performance::snapshot();
        assert!(
            counters.planner_states <= 4096
                && counters.planner_candidates <= 1024
                && counters.planner_bytes <= 262144,
            "{counters:?}"
        );
        // Optional Join advice may exhaust bytes before the 128-Part cap.
        if count == 16 {
            assert_eq!(counters.planner_fallbacks, 0, "{counters:?}");
        } else if count > 128 {
            assert!(counters.planner_fallbacks > 0, "{counters:?}");
        }
        let mut cursor = QueryCursor::new(prepared);
        let mut rows = Vec::new();
        loop {
            let batch = cursor.next_batch(&db, 1).unwrap();
            rows.extend(batch.rows);
            if batch.done {
                assert!(batch.summary.is_some());
                break;
            }
        }
        cursor.complete(&db).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(7)]]);
        println!("patterns={count} plan_us={micros} counters={counters:?}");
    }
    lithograph_core::performance::set_enabled(false);
}

#[test]
fn planned_program_writes_use_the_same_match_access() {
    let db = fixture();
    let text = "WITH 1 AS seed MATCH (n:Common) WHERE n.v=$x SET n.hit=seed RETURN n.v";
    for (options, expected) in [
        (ExecutionOptions::default(), "IndexSeek"),
        (view(), "LabelIndexScan"),
    ] {
        assert!(explain(&db, text, Value::Integer(1), options.clone()).contains(expected));
        let (rows, summary) = run(
            &db,
            &format!("PROFILE {text}"),
            params(Value::Integer(1)),
            options,
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
        assert_access(&summary, expected);
    }
}

#[cfg(feature = "test-support")]
#[test]
fn read_and_write_programs_reuse_one_immutable_base_through_validation() {
    let db = fixture();
    lithograph_core::performance::set_enabled(true);
    for text in [
        "MATCH (n:Common) WHERE n.v=1 RETURN n.v",
        "WITH 1 AS seed MATCH (n:Common) WHERE n.v=seed RETURN n.v",
        "MATCH (n:Common) WHERE n.v=1 SET n.hit=1 RETURN n.v",
        "WITH 1 AS seed MATCH (n:Common) WHERE n.v=seed SET n.hit=seed RETURN n.v",
        "CREATE (:NewStage {v:1}) WITH 1 AS seed MATCH (n:NewStage) RETURN n.v",
    ] {
        lithograph_core::performance::reset();
        assert_eq!(query(&db, text), vec![vec![Value::Integer(1)]], "{text}");
        let work = lithograph_core::performance::snapshot();
        assert_eq!(work.resolved_state_builds, 1, "{text}: {work:?}");
    }
    lithograph_core::performance::set_enabled(false);
}

#[test]
fn optional_access_preparation_never_evaluates_volatile_or_expanding_functions() {
    let db = fixture();
    for predicate in ["n.v=rand()", "n.v IN range(0,1000000)"] {
        let mut options = ExecutionOptions::default();
        options.memory_limit_bytes = Some(1024 * 1024);
        let prepared = prepare(
            &db,
            &format!("EXPLAIN MATCH (n:Common) WHERE {predicate} RETURN n.v"),
            BTreeMap::new(),
            options,
        )
        .unwrap();
        assert!(!prepared.physical.explain().contains("IndexSeek"));
        assert!(prepared.resources.peak() < 512 * 1024);
    }
}

#[test]
fn unsupported_graph_predicates_keep_the_graph_executor_for_read_and_profile() {
    let db = fixture();
    query(
        &db,
        "CREATE (a:A {prop:1})-[:R]->(b:B), (a)-[:R]->(:C), (a)-[:R]->(d:D), (b)-[:R]->(d)",
    );
    let text = "MATCH (n) WHERE EXISTS { MATCH (n)-->(m) WITH n, count(*) AS numConnections WHERE numConnections=3 RETURN true } RETURN n.prop";
    assert_eq!(query(&db, text), vec![vec![Value::Integer(1)]]);
    assert_eq!(
        query(&db, &format!("PROFILE {text}")),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn bound_aliases_do_not_advertise_an_unused_index_seek() {
    let db = fixture();
    let text = "MATCH (n:Common) WHERE elementId(n)='n:1' WITH n AS m MATCH (m:Common) WHERE m.v=$x RETURN m.v";
    let plan = explain(&db, text, Value::Integer(0), ExecutionOptions::default());
    assert!(!plan.contains("IndexSeek"), "{plan}");
    for prefix in ["", "PROFILE "] {
        assert_eq!(
            run(
                &db,
                &(prefix.to_owned() + text),
                params(Value::Integer(0)),
                ExecutionOptions::default()
            )
            .0,
            vec![vec![Value::Integer(0)]]
        );
    }
}

#[test]
fn program_write_prefix_spills_and_preserves_atomic_errors_under_small_quota() {
    let db = fixture();
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = Some(8 * 1024 * 1024);
    let text = "WITH 1 AS seed MATCH (a:Common),(b:Common) SET a.hit=seed RETURN b.v";
    let prepared = prepare(&db, text, BTreeMap::new(), options.clone()).unwrap();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let mut count = 0;
    let mut checksum = 0;
    loop {
        let batch = cursor.next_batch(&db, 32).unwrap();
        for row in batch.rows {
            let [Value::Integer(value)] = row.as_slice() else {
                panic!("{row:?}")
            };
            count += 1;
            checksum += value;
        }
        if batch.done {
            break;
        }
    }
    assert_eq!((count, checksum), (256 * 256, 256 * (255 * 256 / 2)));
    drop(cursor);
    assert_eq!(resources.used(), 0);
    assert!(resources.peak() < 8 * 1024 * 1024);
    assert_eq!(
        query(&db, "MATCH (n:Common) WHERE n.hit=1 RETURN count(*)"),
        vec![vec![Value::Integer(256)]]
    );
    let before = storage::branch_head(&db, "main").unwrap();
    let prepared = prepare(
        &db,
        "WITH 1 AS seed MATCH (n:Common) SET n.hit=2 RETURN 1/0",
        BTreeMap::new(),
        options,
    )
    .unwrap();
    let mut cursor = QueryCursor::new(prepared);
    assert_eq!(
        cursor.next_batch(&db, 1).unwrap_err().kind,
        QueryErrorKind::Type
    );
    drop(cursor);
    assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
    assert_eq!(
        query(&db, "MATCH (n:Common) WHERE n.hit=1 RETURN count(*)"),
        vec![vec![Value::Integer(256)]]
    );
}

#[test]
fn relationship_identity_seek_is_real_and_keeps_direction_type_and_history() {
    let db = fixture();
    let created = query(
        &db,
        "CREATE (a:Source {v:401})-[r:CONNECTS]->(b:Target {v:402}) RETURN elementId(r)",
    );
    let Value::String(id) = &created[0][0] else {
        panic!("relationship identity must be a String");
    };
    let old = storage::branch_head(&db, "main").unwrap();
    query(
        &db,
        "UNWIND range(0,511) AS i CREATE (:Noise)-[:CONNECTS]->(:Noise)",
    );
    let text = "MATCH (a:Source)-[r:CONNECTS]->(b:Target) WHERE elementId(r)=$x RETURN a.v,b.v";
    let plan = explain(
        &db,
        text,
        Value::String(id.clone()),
        ExecutionOptions::default(),
    );
    assert!(plan.contains("RelationshipIdSeek"), "{plan}");
    assert!(!plan.contains("AdjacencySeek"), "{plan}");
    let (rows, summary) = run(
        &db,
        &format!("PROFILE {text}"),
        params(Value::String(id.clone())),
        ExecutionOptions::default(),
    );
    assert_eq!(rows, vec![vec![Value::Integer(401), Value::Integer(402)]]);
    assert_access(&summary, "RelationshipIdSeek");
    assert!(summary.metrics.db_hits < 50, "{:?}", summary.metrics);

    let reversed = "MATCH (a:Target)<-[r:CONNECTS]-(b:Source) WHERE $x=elementId(r) RETURN a.v,b.v";
    assert!(
        explain(
            &db,
            reversed,
            Value::String(id.clone()),
            ExecutionOptions::default()
        )
        .contains("RelationshipIdSeek")
    );
    assert_eq!(
        run(
            &db,
            reversed,
            params(Value::String(id.clone())),
            ExecutionOptions::default()
        )
        .0,
        vec![vec![Value::Integer(402), Value::Integer(401)]]
    );
    let wrong_type = "MATCH (a)-[r:Other]->(b) WHERE elementId(r)=$x RETURN r";
    assert!(
        run(
            &db,
            wrong_type,
            params(Value::String(id.clone())),
            ExecutionOptions::default()
        )
        .0
        .is_empty()
    );
    let hidden =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Noise"]}}"#).unwrap();
    assert!(
        run(&db, text, params(Value::String(id.clone())), hidden)
            .0
            .is_empty()
    );

    let mut old_options = ExecutionOptions::default();
    old_options.snapshot = SnapshotSelector::Commit(old.to_hex());
    assert_eq!(
        run(
            &db,
            text,
            params(Value::String(id.clone())),
            old_options.clone()
        )
        .0,
        vec![vec![Value::Integer(401), Value::Integer(402)]]
    );
    query(
        &db,
        &format!("MATCH ()-[r:CONNECTS]->() WHERE elementId(r)='{id}' DELETE r"),
    );
    assert!(
        run(
            &db,
            text,
            params(Value::String(id.clone())),
            ExecutionOptions::default()
        )
        .0
        .is_empty()
    );
    assert_eq!(
        run(&db, text, params(Value::String(id.clone())), old_options).0,
        vec![vec![Value::Integer(401), Value::Integer(402)]]
    );
}

#[test]
fn invalid_relationship_identity_keeps_null_and_type_error_semantics() {
    let db = fixture();
    query(&db, "CREATE (a:Source)-[:CONNECTS]->(b:Target)");
    let text = "MATCH (a)-[r:CONNECTS]->(b) WHERE elementId(r)=$x RETURN r";
    for (value, can_seek) in [
        (Value::Null, false),
        (Value::String("r:0".to_owned()), false),
        (Value::String("r:0001".to_owned()), false),
        (Value::String("r:99999999".to_owned()), true),
        (Value::String("n:1".to_owned()), false),
    ] {
        assert!(
            run(
                &db,
                text,
                params(value.clone()),
                ExecutionOptions::default()
            )
            .0
            .is_empty()
        );
        assert_eq!(
            explain(&db, text, value, ExecutionOptions::default()).contains("RelationshipIdSeek"),
            can_seek
        );
    }
    let prepared = prepare(
        &db,
        text,
        params(Value::Integer(42)),
        ExecutionOptions::default(),
    )
    .unwrap();
    let mut cursor = QueryCursor::new(prepared);
    assert_eq!(
        cursor.next_batch(&db, 8).unwrap_err().kind,
        QueryErrorKind::Type
    );
}

#[test]
fn relationship_seek_preserves_parallel_bag_undirected_self_loop_and_error_precedence() {
    let db = fixture();
    let created = query(
        &db,
        "CREATE (a:Pair {v:11})-[r1:PAIR]->(b:Pair {v:12}), (a)-[r2:PAIR]->(b) RETURN elementId(r1),elementId(r2)",
    );
    let [Value::String(first), Value::String(second)] = created[0].as_slice() else {
        panic!("parallel relationship identities");
    };
    assert_ne!(first, second);

    let undirected =
        "MATCH (a:Pair)-[r:PAIR]-(b:Pair) WHERE elementId(r)=$x RETURN a.v,b.v ORDER BY a.v,b.v";
    for id in [first, second] {
        let plan = explain(
            &db,
            undirected,
            Value::String(id.clone()),
            ExecutionOptions::default(),
        );
        assert!(plan.contains("RelationshipIdSeek"), "{plan}");
        let (rows, profile) = run(
            &db,
            &format!("PROFILE {undirected}"),
            params(Value::String(id.clone())),
            ExecutionOptions::default(),
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(11), Value::Integer(12)],
                vec![Value::Integer(12), Value::Integer(11)]
            ]
        );
        assert_access(&profile, "RelationshipIdSeek");
    }
    assert_eq!(
        query(
            &db,
            "MATCH (a:Pair)-[r:PAIR]-(b:Pair) RETURN a.v,b.v ORDER BY a.v,b.v"
        ),
        vec![
            vec![Value::Integer(11), Value::Integer(12)],
            vec![Value::Integer(11), Value::Integer(12)],
            vec![Value::Integer(12), Value::Integer(11)],
            vec![Value::Integer(12), Value::Integer(11)]
        ],
        "parallel relationships must remain separate bag elements"
    );

    let loop_id = query(
        &db,
        "CREATE (a:Loop {v:13})-[r:LOOPS]->(a) RETURN elementId(r)",
    )[0][0]
        .clone();
    let loop_query = "MATCH (a:Loop)-[r:LOOPS]-(b:Loop) WHERE elementId(r)=$x RETURN a.v,b.v";
    assert_eq!(
        run(
            &db,
            loop_query,
            params(loop_id.clone()),
            ExecutionOptions::default()
        )
        .0,
        vec![vec![Value::Integer(13), Value::Integer(13)]],
        "undirected self-loop must not be duplicated"
    );
    assert!(
        explain(&db, loop_query, loop_id, ExecutionOptions::default())
            .contains("RelationshipIdSeek")
    );

    let throwing = "MATCH (a:Pair)-[r:PAIR]->(b:Pair) WHERE a.v/0=1 AND elementId(r)=$x RETURN r";
    let nonexistent = Value::String("r:99999999".to_owned());
    assert!(
        !explain(
            &db,
            throwing,
            nonexistent.clone(),
            ExecutionOptions::default()
        )
        .contains("RelationshipIdSeek"),
        "a late equality cannot skip an earlier error"
    );
    let prepared = prepare(
        &db,
        throwing,
        params(nonexistent),
        ExecutionOptions::default(),
    )
    .unwrap();
    let mut cursor = QueryCursor::new(prepared);
    assert_eq!(
        cursor.next_batch(&db, 8).unwrap_err().kind,
        QueryErrorKind::Type
    );
}
