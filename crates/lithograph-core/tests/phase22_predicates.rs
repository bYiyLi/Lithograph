use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, PhysicalOperator, QueryCursor, QueryError, QueryErrorKind, QuerySummary,
    SnapshotSelector, prepare,
};
use lithograph_core::storage::{
    self, CommitMetadata, LayerBuilder, OwnerKind, PropertyValue, RelationshipRecord,
};
use rusqlite::Connection;

fn execute(
    db: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), QueryError> {
    let prepared = prepare(db, text, params, options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(db, 7)?;
        rows.extend(batch.rows);
        if batch.done {
            return Ok((rows, batch.summary.expect("terminal summary")));
        }
    }
}

fn query(db: &Connection, text: &str) -> Vec<Vec<Value>> {
    execute(db, text, BTreeMap::new(), ExecutionOptions::default())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .0
}

fn counted(db: &Connection, text: &str) -> (Vec<Vec<Value>>, QuerySummary, u64) {
    let steps = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&steps);
    db.progress_handler(
        1,
        Some(move || {
            observed.fetch_add(1, Ordering::Relaxed);
            false
        }),
    )
    .unwrap();
    let result = execute(db, text, BTreeMap::new(), ExecutionOptions::default());
    db.progress_handler(0, None::<fn() -> bool>).unwrap();
    let (rows, summary) = result.unwrap();
    (rows, summary, steps.load(Ordering::Relaxed))
}

fn explain(db: &Connection, text: &str, options: ExecutionOptions) -> String {
    explain_params(db, text, BTreeMap::new(), options)
}

fn explain_params(
    db: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> String {
    let (rows, _) = execute(db, &format!("EXPLAIN {text}"), params, options).unwrap();
    let Value::String(plan) = &rows[0][0] else {
        panic!("plan string")
    };
    plan.clone()
}

fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY, magic TEXT, database_id TEXT, storage_format INTEGER);").unwrap();
    storage::create_storage_schema(&db).unwrap();
    let root = storage::initialize_root(&db).unwrap().root;
    let item = storage::intern_label(&db, "Item").unwrap();
    let fan = storage::intern_label(&db, "Fan").unwrap();
    let edge = storage::intern_relationship_type(&db, "EDGE").unwrap();
    let v = storage::intern_property_key(&db, "v").unwrap();
    let g = storage::intern_property_key(&db, "g").unwrap();
    let f = storage::intern_property_key(&db, "f").unwrap();
    let mut layer = LayerBuilder::default();
    let mut fans = Vec::new();
    for value in 0..8 {
        let id = storage::allocate_node_id(&db).unwrap();
        layer.add_node(id).unwrap();
        layer.add_label(id, fan).unwrap();
        layer
            .set_property(OwnerKind::Node, id, f, PropertyValue::Integer(value))
            .unwrap();
        fans.push(id);
    }
    for value in 0..256 {
        let id = storage::allocate_node_id(&db).unwrap();
        layer.add_node(id).unwrap();
        layer.add_label(id, item).unwrap();
        let relationship = storage::allocate_relationship_id(&db).unwrap();
        layer
            .add_relationship(RelationshipRecord {
                id: relationship,
                source: id,
                target: fans[value as usize % 8],
                type_id: edge,
            })
            .unwrap();
        for (kind, owner) in [
            (OwnerKind::Node, id),
            (OwnerKind::Relationship, relationship),
        ] {
            layer
                .set_property(kind, owner, v, PropertyValue::Integer(value))
                .unwrap();
            layer
                .set_property(kind, owner, g, PropertyValue::Integer(value % 8))
                .unwrap();
        }
    }
    // A missing indexed value makes NULL AND a throwing residual observable.
    let missing = storage::allocate_node_id(&db).unwrap();
    layer.add_node(missing).unwrap();
    layer.add_label(missing, item).unwrap();
    layer
        .set_property(OwnerKind::Node, missing, g, PropertyValue::Integer(0))
        .unwrap();
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
    for ddl in [
        "CREATE CONSTRAINT item_v FOR (n:Item) REQUIRE n.v IS :: INTEGER",
        "CREATE CONSTRAINT item_g FOR (n:Item) REQUIRE n.g IS :: INTEGER",
        "CREATE CONSTRAINT fan_f FOR (n:Fan) REQUIRE n.f IS :: INTEGER",
        "CREATE CONSTRAINT edge_v FOR ()-[r:EDGE]-() REQUIRE r.v IS :: INTEGER",
        "CREATE CONSTRAINT edge_g FOR ()-[r:EDGE]-() REQUIRE r.g IS :: INTEGER",
        "CREATE RANGE INDEX item_values FOR (n:Item) ON (n.v)",
        "CREATE RANGE INDEX edge_values FOR ()-[r:EDGE]-() ON (r.v)",
    ] {
        query(&db, ddl);
    }
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    db
}

fn assert_oracle(db: &Connection, pattern: &str, predicate: &str, projection: &str) {
    let optimized = format!("MATCH {pattern} WHERE {predicate} RETURN {projection}");
    // OR is deliberately outside the optimizer's supported rewrite shape.
    // Adding FALSE preserves values, NULL, errors and the complete row bag.
    let original = format!("MATCH {pattern} WHERE ({predicate}) OR false RETURN {projection}");
    assert!(!explain(db, &original, ExecutionOptions::default()).contains("IndexSeek"));
    let run = |text: &str| execute(db, text, BTreeMap::new(), ExecutionOptions::default());
    match (run(&optimized), run(&original)) {
        (Ok((actual, _)), Ok((expected, _))) => assert_eq!(actual, expected, "{optimized}"),
        (Err(actual), Err(expected)) => assert_eq!(actual.kind, expected.kind, "{optimized}"),
        (actual, expected) => panic!("oracle mismatch for {optimized}: {actual:?} / {expected:?}"),
    }
}

#[test]
fn typed_unindexed_residuals_allow_node_relationship_and_end_node_index_access() {
    let db = fixture();
    for (pattern, predicate, projection) in [
        ("(n:Item)", "n.v=5 AND n.g>=5", "n.v"),
        ("(n:Item)", "n.g>=5 AND n.v=5", "n.v"),
        ("()-[r:EDGE]->()", "r.v=5 AND r.g=5", "r.v"),
        (
            "(n:Item)-[r:EDGE]->(f:Fan)",
            "n.v=5 AND f.f=5 AND r.g=5",
            "n.v",
        ),
    ] {
        let text = format!("MATCH {pattern} WHERE {predicate} RETURN {projection}");
        let plan = explain(&db, &text, ExecutionOptions::default());
        assert!(plan.contains("IndexSeek"), "{text}: {plan}");
        assert_eq!(query(&db, &text), vec![vec![Value::Integer(5)]]);
        assert_oracle(&db, pattern, predicate, projection);
    }
}

#[test]
fn range_in_and_nullable_residual_combinations_keep_the_exact_row_bag() {
    let db = fixture();
    for (predicate, expected) in [
        ("n.v>=5 AND n.v<=7 AND n.g IN [5,5.0,6,null]", vec![5, 6]),
        ("n.v IN [5,5,6] AND n.g>=5 AND n.g<=6", vec![5, 6]),
        ("n.v=5 AND n.g IN []", vec![]),
        ("n.v=5 AND n.g IN [null]", vec![]),
        ("n.v=5 AND n.g IN null", vec![]),
        ("n.v=5 AND n.missing IS NULL", vec![5]),
        ("n.v=5 AND n.missing IS NOT NULL", vec![]),
        ("n.v=5 AND n.g=null", vec![]),
        ("n.v=5 AND true", vec![5]),
        ("n.v=5 AND null", vec![]),
    ] {
        let text = format!("MATCH (n:Item) WHERE {predicate} RETURN n.v ORDER BY n.v");
        let plan = explain(&db, &text, ExecutionOptions::default());
        assert!(plan.contains("IndexSeek"), "{text}: {plan}");
        assert_eq!(
            query(&db, &text),
            expected
                .into_iter()
                .map(|value| vec![Value::Integer(value)])
                .collect::<Vec<_>>()
        );
        assert_oracle(&db, "(n:Item)", predicate, "n.v ORDER BY n.v");
    }
}

#[test]
fn independent_type_proof_covers_present_property_families() {
    let db = fixture();
    for (property, property_type, value) in [
        ("s", "STRING", "'hello'"),
        ("b", "BOOLEAN", "true"),
        ("d", "DATE", "date('2026-01-01')"),
        ("p", "POINT", "point({x:1,y:2})"),
        ("l", "LIST<INTEGER NOT NULL>", "[1,2]"),
        ("t", "DURATION", "duration('P1D')"),
        ("u", "UUID", "uuid('550e8400-e29b-41d4-a716-446655440000')"),
    ] {
        query(
            &db,
            &format!("MATCH (n:Item) WHERE n.v=5 SET n.{property}={value}"),
        );
        query(
            &db,
            &format!(
                "CREATE CONSTRAINT item_{property} FOR (n:Item) REQUIRE n.{property} IS :: {property_type}"
            ),
        );
        let predicate = format!("n.v=5 AND n.{property}={value}");
        let text = format!("MATCH (n:Item) WHERE {predicate} RETURN n.v");
        let plan = explain(&db, &text, ExecutionOptions::default());
        assert!(plan.contains("IndexSeek"), "{property}: {plan}");
        assert_eq!(query(&db, &text), vec![vec![Value::Integer(5)]]);
        assert_oracle(&db, "(n:Item)", &predicate, "n.v");
    }
    // A String predicate is total for missing and other property families.
    assert_oracle(&db, "(n:Item)", "n.v=5 AND n.g CONTAINS 'x'", "n.v");
}

#[test]
fn partial_index_proof_never_suppresses_type_or_residual_errors() {
    let db = fixture();
    for predicate in [
        "n.v=99999 AND n.g IN ['bad']",
        "n.g IN ['bad'] AND n.v=99999",
        "n.v=99999 AND 1/0=0",
        "n.v IN [0,'bad'] AND n.g>=0",
        "n.v=99999 AND n.g='bad'",
    ] {
        let text = format!("MATCH (n:Item) WHERE {predicate} RETURN n.v");
        assert!(!explain(&db, &text, ExecutionOptions::default()).contains("IndexSeek"));
        assert_eq!(
            execute(&db, &text, BTreeMap::new(), ExecutionOptions::default())
                .unwrap_err()
                .kind,
            QueryErrorKind::Type,
            "{text}"
        );
        assert_oracle(&db, "(n:Item)", predicate, "n.v");
    }
}

#[test]
fn parameter_in_proof_handles_duplicates_null_numeric_coercion_and_invalid_families() {
    let db = fixture();
    let text = "MATCH (n:Item) WHERE n.v=$v AND n.g IN $groups RETURN n.v";
    for (groups, expected) in [
        (
            Value::List(vec![Value::Integer(5), Value::Float(5.0), Value::Null]),
            vec![vec![Value::Integer(5)]],
        ),
        (Value::List(vec![]), vec![]),
        (Value::List(vec![Value::Null]), vec![]),
        (Value::Null, vec![]),
        (Value::List(vec![Value::Float(f64::NAN)]), vec![]),
    ] {
        let params = BTreeMap::from([
            ("v".to_owned(), Value::Float(5.0)),
            ("groups".to_owned(), groups),
        ]);
        assert!(
            explain_params(&db, text, params.clone(), ExecutionOptions::default())
                .contains("IndexSeek")
        );
        let actual = execute(&db, text, params.clone(), ExecutionOptions::default())
            .unwrap()
            .0;
        let oracle = execute(
            &db,
            "MATCH (n:Item) WHERE (n.v=$v AND n.g IN $groups) OR false RETURN n.v",
            params,
            ExecutionOptions::default(),
        )
        .unwrap()
        .0;
        assert_eq!(actual, expected);
        assert_eq!(actual, oracle);
    }
    for groups in [
        Value::Integer(42),
        Value::List(vec![Value::Integer(5), Value::String("bad".to_owned())]),
    ] {
        let params = BTreeMap::from([
            ("v".to_owned(), Value::Integer(99999)),
            ("groups".to_owned(), groups),
        ]);
        assert_eq!(
            execute(&db, text, params, ExecutionOptions::default())
                .unwrap_err()
                .kind,
            QueryErrorKind::Type
        );
    }
}

#[test]
fn unsupported_boolean_volatile_and_cross_scope_shapes_keep_the_original_path() {
    let db = fixture();
    for text in [
        "MATCH (n:Item) WHERE n.v=5 OR n.g=6 RETURN n.v",
        "MATCH (n:Item) WHERE n.v=5 AND n.g=rand() RETURN n.v",
        "MATCH (n:Item) WHERE n.v=rand() AND n.g=5 RETURN n.v",
        "MATCH (n:Item) WHERE n.v=5 AND n.g IN range(0,1000000) RETURN n.v",
        "WITH 5 AS value MATCH (n:Item) WHERE n.v=value AND n.g=5 RETURN n.v",
        "MATCH (n:Item) WHERE n.v=5 AND EXISTS { MATCH (f:Fan) WHERE f.f=n.g } RETURN n.v",
    ] {
        assert!(
            !explain(&db, text, ExecutionOptions::default()).contains("IndexSeek"),
            "{text}"
        );
    }
    assert_oracle(
        &db,
        "(n:Item),(f:Fan)",
        "n.v IN [1,null] AND n.g='bad'",
        "n.v",
    );
    assert_oracle(&db, "(n:Item),(f:Fan)", "n.v IN [] AND n.g='bad'", "n.v");
    query(&db, "DROP CONSTRAINT item_g");
    query(
        &db,
        "CREATE CONSTRAINT item_g_union FOR (n:Item) REQUIRE n.g IS :: INTEGER | STRING",
    );
    let text = "MATCH (n:Item) WHERE n.v=5 AND n.g=5 RETURN n.v";
    assert!(!explain(&db, text, ExecutionOptions::default()).contains("IndexSeek"));
    assert_eq!(query(&db, text), vec![vec![Value::Integer(5)]]);
}

#[test]
fn range_and_in_prefix_routing_reduces_unindexed_cartesian_work() {
    let db = fixture();
    query(&db, "DROP INDEX item_values");
    query(&db, "DROP INDEX edge_values");
    for predicate in [
        "n.v>252 AND n.g<=7",
        "n.v IN [1,1.0,1] AND n.g>=0",
        "253<n.v AND n.g IN [6,7]",
        "n.v IN [-1,1]",
    ] {
        let optimized = format!(
            "PROFILE MATCH (n:Item),(f:Fan) WHERE {predicate} RETURN n.v,f.f ORDER BY n.v,f.f"
        );
        let original = format!(
            "PROFILE MATCH (n:Item),(f:Fan) WHERE ({predicate}) OR false RETURN n.v,f.f ORDER BY n.v,f.f"
        );
        let (actual, optimized_summary, optimized_vm) = counted(&db, &optimized);
        let (expected, original_summary, original_vm) = counted(&db, &original);
        eprintln!(
            "{predicate}: VM {optimized_vm}/{original_vm}, db_hits {}/{} (optimized/oracle)",
            optimized_summary.metrics.db_hits, original_summary.metrics.db_hits
        );
        assert_eq!(actual, expected, "{predicate}");
        assert!(
            original_summary.metrics.db_hits > optimized_summary.metrics.db_hits * 2,
            "{predicate}: optimized={}, original={}, plan={}",
            optimized_summary.metrics.db_hits,
            original_summary.metrics.db_hits,
            explain(
                &db,
                optimized.strip_prefix("PROFILE ").unwrap(),
                ExecutionOptions::default()
            )
        );
        assert!(
            original_vm > optimized_vm * 2,
            "{predicate}: optimized VM={optimized_vm}, original VM={original_vm}"
        );
    }
    assert_oracle(
        &db,
        "(n:Item),(f:Fan)",
        "n.v IN [1,1.0,null] AND n.g>=0",
        "n.v,f.f ORDER BY n.v,f.f",
    );
    assert_oracle(
        &db,
        "()-[r:EDGE]->(),(f:Fan)",
        "r.v>253 AND r.g IN [6,7]",
        "r.v,f.f ORDER BY r.v,f.f",
    );
}

#[test]
fn routed_errors_wait_for_complete_bindings_and_optional_scope_stays_intact() {
    let db = fixture();
    for predicate in ["n.g IN ['bad']", "n.g<['bad']"] {
        assert_oracle(&db, "(n:Item),(f:Empty)", predicate, "n.v");
        assert!(
            query(
                &db,
                &format!("MATCH (n:Item),(f:Empty) WHERE {predicate} RETURN n.v")
            )
            .is_empty()
        );
        assert_oracle(&db, "(n:Item),(f:Fan)", predicate, "n.v");
    }
    assert_eq!(
        query(
            &db,
            "WITH 1 AS seed OPTIONAL MATCH (n:Item),(f:Fan) WHERE n.v>99999 RETURN seed,n.v,f.f"
        ),
        vec![vec![Value::Integer(1), Value::Null, Value::Null]]
    );
    let text = "WITH 1 AS x MATCH (n:Item) WHERE n.v=5 AND x=1 RETURN n.v,x";
    assert!(!explain(&db, text, ExecutionOptions::default()).contains("IndexSeek"));
    assert_eq!(
        query(&db, text),
        vec![vec![Value::Integer(5), Value::Integer(1)]]
    );
    assert_oracle(&db, "(n:Item)", "n.v=5 OR n.g=6", "n.v ORDER BY n.v");
}

#[test]
fn historical_type_proof_uses_the_selected_schema_and_current_changes_fall_back() {
    let db = fixture();
    let old = storage::branch_head(&db, "main").unwrap();
    let text = "MATCH (n:Item) WHERE n.v=5 AND n.g=5 RETURN n.v";
    query(&db, "DROP CONSTRAINT item_g");
    query(&db, "MATCH (n:Item) WHERE n.v=5 SET n.g='bad'");
    let mut historical = ExecutionOptions::default();
    historical.snapshot = SnapshotSelector::Commit(old.to_hex());
    assert!(explain(&db, text, historical.clone()).contains("IndexSeek"));
    assert_eq!(
        execute(&db, text, BTreeMap::new(), historical).unwrap().0,
        vec![vec![Value::Integer(5)]]
    );
    assert!(!explain(&db, text, ExecutionOptions::default()).contains("IndexSeek"));
    assert_eq!(
        execute(&db, text, BTreeMap::new(), ExecutionOptions::default())
            .unwrap_err()
            .kind,
        QueryErrorKind::Type
    );
}

#[test]
fn proof_does_not_bypass_query_access_and_shared_read_write_program_cursors_agree() {
    let db = fixture();
    let denied =
        ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["g"]}}}"#)
            .unwrap();
    let text = "MATCH (n:Item) WHERE n.v=5 AND n.g>=0 RETURN n.v";
    assert!(explain(&db, text, denied.clone()).contains("item_values"));
    assert!(
        !explain(&db, "MATCH (n:Item) WHERE n.g=5 RETURN n.v", denied.clone())
            .contains("IndexSeek")
    );
    for prefix in ["", "PROFILE "] {
        let text = format!("{prefix}MATCH (n:Item) WHERE n.v=5 AND n.g>=0 RETURN n.v");
        assert!(
            execute(&db, &text, BTreeMap::new(), denied.clone())
                .unwrap()
                .0
                .is_empty()
        );
    }
    assert_eq!(
        execute(
            &db,
            "MATCH (n:Item) WHERE n.v=5 AND n.g IS NULL RETURN n.v",
            BTreeMap::new(),
            denied
        )
        .unwrap()
        .0,
        vec![vec![Value::Integer(5)]]
    );
    let result_only =
        ExecutionOptions::parse_text(r#"{"readAccess":{"result":{"denyProperties":["g"]}}}"#)
            .unwrap();
    assert!(explain(&db, text, result_only.clone()).contains("IndexSeek"));
    assert_eq!(
        execute(&db, text, BTreeMap::new(), result_only).unwrap().0,
        vec![vec![Value::Integer(5)]]
    );
    for text in [
        "MATCH (n:Item) WHERE n.v=5 AND n.g IN [5,null] RETURN n.v",
        "WITH 1 AS x MATCH (n:Item) WHERE n.v=5 AND n.g IN [5,null] RETURN n.v",
        "MATCH (n:Item) WHERE n.v=5 AND n.g IN [5,null] SET n.touched=true RETURN n.v",
        "WITH 1 AS x MATCH (n:Item) WHERE n.v=5 AND n.g IN [5,null] SET n.touched=true RETURN n.v",
    ] {
        assert!(
            explain(&db, text, ExecutionOptions::default()).contains("IndexSeek"),
            "{text}"
        );
        assert_eq!(query(&db, text), vec![vec![Value::Integer(5)]]);
    }
}

#[test]
fn predicate_rewrites_preserve_failed_write_and_staged_type_rollback() {
    let db = fixture();
    for prefix in ["", "WITH 1 AS seed "] {
        let before = storage::branch_head(&db, "main").unwrap();
        let text = format!(
            "{prefix}MATCH (n:Item) WHERE n.v=5 AND n.g IN [5,null] SET n.touched=42 RETURN 1/0"
        );
        assert_eq!(
            execute(&db, &text, BTreeMap::new(), ExecutionOptions::default())
                .unwrap_err()
                .kind,
            QueryErrorKind::Type
        );
        assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
        assert_eq!(
            query(&db, "MATCH (n:Item) WHERE n.v=5 RETURN n.touched"),
            vec![vec![Value::Null]]
        );
    }
    let before = storage::branch_head(&db, "main").unwrap();
    let run = |predicate: &str| {
        let text = format!(
            "CREATE (:Item {{v:99999,g:'bad'}}) WITH 1 AS seed MATCH (n:Item) WHERE {predicate} RETURN n.v"
        );
        execute(&db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap_err()
    };
    let optimized = run("n.g=5 AND n.v=5");
    assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
    let original = run("(n.g=5 AND n.v=5) OR false");
    assert_eq!(optimized.kind, original.kind);
    assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
    assert!(query(&db, "MATCH (n:Item) WHERE n.v=99999 RETURN n.v").is_empty());
}

#[cfg(feature = "test-support")]
#[test]
fn repeated_constraint_in_proofs_share_one_query_budget_and_fall_back_correctly() {
    let db = fixture();
    let labels = (0..63)
        .map(|index| format!("Proof{index}"))
        .collect::<Vec<_>>();
    query(&db, &format!("MATCH (n:Item) SET n:{}", labels.join(":")));
    for (index, label) in labels.iter().enumerate() {
        query(
            &db,
            &format!("CREATE CONSTRAINT extra_g_{index} FOR (n:{label}) REQUIRE n.g IS :: INTEGER"),
        );
    }
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let pattern = format!("(n:Item:{})", labels.join(":"));
    let text = format!("MATCH {pattern} WHERE n.v=5 AND n.g IN $groups RETURN n.v");
    let params = |start| {
        let mut values = (start..start + 32).map(Value::Integer).collect::<Vec<_>>();
        values.push(Value::String("incompatible last member".to_owned()));
        BTreeMap::from([("groups".to_owned(), Value::List(values))])
    };
    lithograph_core::performance::set_enabled(true);
    lithograph_core::performance::reset();
    let prepared = prepare(&db, &text, params(0), ExecutionOptions::default()).unwrap();
    let counters = lithograph_core::performance::snapshot();
    lithograph_core::performance::set_enabled(false);
    assert_eq!(counters.planner_fallbacks, 1, "{counters:?}");
    assert_eq!(
        counters.planner_states, 4096,
        "type proof spends the shared work budget: {counters:?}"
    );
    assert!(counters.planner_bytes <= 256 * 1024, "{counters:?}");
    assert!(!prepared.physical.explain().contains("IndexSeek"));
    let mut cursor = QueryCursor::new(prepared);
    assert_eq!(
        cursor.next_batch(&db, 7).unwrap().rows,
        vec![vec![Value::Integer(5)]]
    );
    drop(cursor);
    let oracle = format!("MATCH {pattern} WHERE (n.v=5 AND n.g IN $groups) OR false RETURN n.v");
    assert_eq!(
        execute(&db, &oracle, params(0), ExecutionOptions::default())
            .unwrap()
            .0,
        vec![vec![Value::Integer(5)]]
    );
    for text in [&text, &oracle] {
        assert_eq!(
            execute(&db, text, params(64), ExecutionOptions::default())
                .unwrap_err()
                .kind,
            QueryErrorKind::Type
        );
    }
    let resources = lithograph_core::query::QueryResources::default();
    let interrupted = lithograph_core::query::prepare_with_resources_and_interrupt(
        &db,
        &text,
        params(0),
        ExecutionOptions::default(),
        resources.clone(),
        &|| true,
    )
    .unwrap_err();
    assert_eq!(interrupted.kind, QueryErrorKind::Interrupted);
    assert_eq!(resources.used(), 0);
}

#[test]
fn unindexed_empty_input_does_not_fold_large_parameters_before_matching() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY, magic TEXT, database_id TEXT, storage_format INTEGER);").unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = Some(700 * 1024);
    for predicate in [
        "n.v=$large",
        "(n.v=$large) OR false",
        "false AND n.v=$large",
        "(false AND n.v=$large) OR false",
    ] {
        let params = BTreeMap::from([("large".to_owned(), Value::String("x".repeat(512 * 1024)))]);
        let text = format!("MATCH (n:Empty) WHERE {predicate} RETURN 1");
        assert!(
            execute(&db, &text, params, options.clone())
                .unwrap_or_else(|error| panic!("{predicate}: {error:?}"))
                .0
                .is_empty()
        );
    }
}

#[cfg(feature = "test-support")]
#[test]
fn long_property_names_fall_back_before_optional_optimizer_clones() {
    let db = fixture();
    let property = "x".repeat(300 * 1024);
    let predicate = format!("n.v=5 AND n.`{property}`=0");
    let text = format!("MATCH (n:Item) WHERE {predicate} RETURN n.v");
    lithograph_core::performance::set_enabled(true);
    lithograph_core::performance::reset();
    let prepared = prepare(&db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let counters = lithograph_core::performance::snapshot();
    lithograph_core::performance::set_enabled(false);
    assert_eq!(counters.planner_fallbacks, 1, "{counters:?}");
    assert!(counters.planner_bytes <= 256 * 1024, "{counters:?}");
    assert!(!prepared.physical.explain().contains("IndexSeek"));
    assert_oracle(&db, "(n:Item)", &predicate, "n.v");
}

fn assert_scope_fallback_plan(
    db: &Connection,
    text: &str,
    oracle: &str,
    params: BTreeMap<String, Value>,
    bound_node: Option<&str>,
) {
    let physical = prepare(db, text, params.clone(), ExecutionOptions::default())
        .unwrap()
        .physical;
    assert!(
        physical.operators.iter().all(|operator| !matches!(
            operator,
            PhysicalOperator::IndexSeek { .. }
                | PhysicalOperator::RelationshipIdSeek { .. }
                | PhysicalOperator::NestedLoop
                | PhysicalOperator::IndexNestedLoop
        )),
        "{text}: {physical:?}"
    );
    let points = physical
        .operators
        .iter()
        .filter_map(|operator| match operator {
            PhysicalOperator::NodeIdSeek { variable } => Some(variable.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        points,
        bound_node.into_iter().collect::<Vec<_>>(),
        "{text}: {physical:?}"
    );
    let expected = prepare(db, oracle, params, ExecutionOptions::default())
        .unwrap()
        .physical;
    assert_eq!(
        physical, expected,
        "scope fallback and its OR oracle must have the same accesses and Filters: {text}"
    );
}

fn assert_bound_node_residuals(db: &Connection) {
    let matched = query(db, "MATCH(n:Item) WHERE n.v=5 RETURN elementId(n)")[0][0].clone();
    let other = query(db, "MATCH(n:Item) WHERE n.v=6 RETURN elementId(n)")[0][0].clone();
    let cases = [
        ("n.v=5", BTreeMap::new(), vec![vec![Value::Integer(5)]]),
        ("n.v=6", BTreeMap::new(), vec![]),
        (
            "elementId(n)=$identity",
            BTreeMap::from([("identity".to_owned(), matched)]),
            vec![vec![Value::Integer(5)]],
        ),
        (
            "elementId(n)=$identity",
            BTreeMap::from([("identity".to_owned(), other)]),
            vec![],
        ),
        (
            "elementId(n)=$identity",
            BTreeMap::from([("identity".to_owned(), Value::String("n:999999".to_owned()))]),
            vec![],
        ),
    ];
    for (predicate, params, expected_rows) in cases {
        let prefix = "MATCH(n:Item) WHERE n.v=5 WITH n MATCH(n:Item)";
        let text = format!("{prefix} WHERE {predicate} RETURN n.v");
        let oracle = format!("{prefix} WHERE ({predicate}) OR false RETURN n.v");
        let physical = prepare(db, &text, params.clone(), ExecutionOptions::default())
            .unwrap()
            .physical;
        let expected = prepare(db, &oracle, params.clone(), ExecutionOptions::default())
            .unwrap()
            .physical;
        assert_eq!(physical, expected, "{text}");
        let points = physical
            .operators
            .iter()
            .enumerate()
            .filter_map(|(slot, operator)| {
                matches!(operator, PhysicalOperator::NodeIdSeek { variable } if variable == "n")
                    .then_some(slot)
            })
            .collect::<Vec<_>>();
        assert_eq!(points.len(), 1, "{text}: {physical:?}");
        assert!(
            physical.operators[points[0] + 1..]
                .iter()
                .all(|operator| !matches!(
                    operator,
                    PhysicalOperator::IndexSeek { .. }
                        | PhysicalOperator::NodeIdSeek { .. }
                        | PhysicalOperator::RelationshipIdSeek { .. }
                        | PhysicalOperator::NestedLoop
                        | PhysicalOperator::IndexNestedLoop
                )),
            "{text}: {physical:?}"
        );
        for query in [&text, &oracle] {
            assert_bound_point_profile(db, query, params.clone(), &expected_rows, 1);
        }
    }
    assert_bound_node_parameter_and_chain(db);
}

fn assert_bound_point_profile(
    db: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    expected_rows: &[Vec<Value>],
    expected_points: usize,
) -> QuerySummary {
    let physical = prepare(db, text, params.clone(), ExecutionOptions::default())
        .unwrap()
        .physical;
    let points = physical
        .operators
        .iter()
        .enumerate()
        .filter_map(|(slot, operator)| {
            matches!(operator, PhysicalOperator::NodeIdSeek { variable } if variable == "n")
                .then_some(slot)
        })
        .collect::<Vec<_>>();
    assert_eq!(points.len(), expected_points, "{text}: {physical:?}");
    let (rows, summary) = execute(
        db,
        &format!("PROFILE {text}"),
        params,
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_eq!(rows.as_slice(), expected_rows, "{text}");
    let profile = summary.metrics.operator_profile().unwrap();
    assert_eq!(
        profile.iter().map(|operator| operator.db_hits).sum::<u64>(),
        summary.metrics.db_hits,
        "{text}: {profile:?}"
    );
    for slot in points {
        let point = &profile[slot];
        assert_eq!(point.id, slot as u64, "{text}: {profile:?}");
        assert_eq!(point.operator, "NodeIdSeek", "{text}: {profile:?}");
        // A rejecting residual still validates the real incoming n. Each
        // standalone producer records that row once, including a first MATCH
        // seeded by a Node parameter and repeated same-name MATCH clauses.
        assert_eq!(point.rows, 1, "{text}: {profile:?}");
        assert!(point.db_hits > 0, "{text}: {profile:?}");
    }
    summary
}

fn assert_bound_node_parameter_and_chain(db: &Connection) {
    let node = query(db, "MATCH(n:Item) WHERE n.v=5 RETURN n")[0][0].clone();
    assert!(matches!(node, Value::Node(_)));
    let params = BTreeMap::from([("node".to_owned(), node)]);
    for (predicate, expected) in [("n.v=5", vec![vec![Value::Integer(5)]]), ("n.v=6", vec![])] {
        let text = format!("WITH $node AS n MATCH(n:Item) WHERE {predicate} RETURN n.v");
        let oracle =
            format!("WITH $node AS n MATCH(n:Item) WHERE ({predicate}) OR false RETURN n.v");
        assert_scope_fallback_plan(db, &text, &oracle, params.clone(), Some("n"));
        for query in [&text, &oracle] {
            assert_bound_point_profile(db, query, params.clone(), &expected, 1);
        }
    }
    let chain = "MATCH(n:Item) WHERE n.v=5 WITH n MATCH(n:Item) WITH n MATCH(n:Item)";
    for suffix in ["RETURN n.v", "WHERE true OR false RETURN n.v"] {
        assert_bound_point_profile(
            db,
            &format!("{chain} {suffix}"),
            BTreeMap::new(),
            &[vec![Value::Integer(5)]],
            2,
        );
    }
    let aggregate = "MATCH(n:Item) WHERE n.v=5 WITH n,count(*) AS c MATCH(n:Item)";
    for predicate in ["n.v=6", "(n.v=6) OR false"] {
        assert_bound_point_profile(
            db,
            &format!("{aggregate} WHERE {predicate} RETURN n.v"),
            BTreeMap::new(),
            &[],
            1,
        );
    }
    assert_aggregate_bound_groups(db);
    assert_union_bound_node_points(db);
}

fn assert_union_bound_node_points(db: &Connection) {
    for predicate in ["n.v=6", "(n.v=6) OR false"] {
        let branch =
            format!("MATCH(n:Item) WHERE n.v=5 WITH n MATCH(n:Item) WHERE {predicate} RETURN n.v");
        let text = format!("{branch} UNION ALL {branch}");
        assert_bound_point_profile(db, &text, BTreeMap::new(), &[], 2);
    }
}

fn assert_aggregate_bound_groups(db: &Connection) {
    let aggregate = "MATCH(n:Item) WHERE n.v=5 WITH n,count(*) AS c";
    let baseline = assert_bound_point_profile(
        db,
        &format!("{aggregate} MATCH(n:Item) WHERE n.v=6 RETURN n.v"),
        BTreeMap::new(),
        &[],
        1,
    );
    let point_hits = baseline
        .metrics
        .operator_profile()
        .unwrap()
        .iter()
        .find(|operator| operator.operator == "NodeIdSeek")
        .unwrap()
        .db_hits;
    assert_eq!(point_hits, 1);
    for predicate in ["n.v=6", "(n.v=6) OR false"] {
        let text =
            format!("{aggregate} MATCH(n:Item),(n:Item),(f:Fan) WHERE {predicate} RETURN n.v");
        let summary = assert_bound_point_profile(db, &text, BTreeMap::new(), &[], 2);
        for point in summary
            .metrics
            .operator_profile()
            .unwrap()
            .iter()
            .filter(|operator| operator.operator == "NodeIdSeek")
        {
            assert_eq!(point.db_hits, point_hits, "{text}: {point:?}");
        }
    }
    assert_bound_group_fan_slot(db, point_hits);
}

fn assert_bound_group_fan_slot(db: &Connection, point_hits: u64) {
    let baseline = "MATCH(x:Fan) RETURN 5,x.f";
    let (expected, baseline_summary) = execute(
        db,
        &format!("PROFILE {baseline}"),
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_eq!(expected.len(), 8);
    let baseline_profile = baseline_summary.metrics.operator_profile().unwrap();
    assert_eq!(
        baseline_profile.iter().map(|op| op.db_hits).sum::<u64>(),
        baseline_summary.metrics.db_hits
    );
    let fan = baseline_profile
        .iter()
        .find(|op| op.operator == "LabelIndexScan")
        .unwrap();
    assert_eq!(fan.rows, 8);
    assert_eq!(fan.db_hits, 16);
    let normalize = |rows: Vec<Vec<Value>>| {
        let mut bag = rows
            .into_iter()
            .map(|row| format!("{row:?}"))
            .collect::<Vec<_>>();
        bag.sort();
        bag
    };
    let expected = normalize(expected);
    for predicate in ["n.v=5", "(n.v=5) OR false"] {
        let text = format!(
            "MATCH(n:Item) WHERE {predicate} WITH n,count(*) AS c \
            MATCH(n:Item),(n)-[:EDGE]->(f) WITH n MATCH(x:Fan) RETURN n.v,x.f"
        );
        let physical = prepare(db, &text, BTreeMap::new(), ExecutionOptions::default())
            .unwrap()
            .physical;
        let points = physical
            .operators
            .iter()
            .enumerate()
            .filter_map(|(slot, op)| {
                matches!(op, PhysicalOperator::NodeIdSeek { variable } if variable == "n")
                    .then_some(slot)
            })
            .collect::<Vec<_>>();
        assert_eq!(points.len(), 1, "{text}: {physical:?}");
        let fans = physical.operators.iter().enumerate().filter_map(|(slot, op)|
            matches!(op, PhysicalOperator::LabelIndexScan { variable, label } if variable == "x" && label == "Fan")
                .then_some(slot)).collect::<Vec<_>>();
        assert_eq!(fans.len(), 1, "{text}: {physical:?}");
        let (rows, summary) = execute(
            db,
            &format!("PROFILE {text}"),
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .unwrap();
        assert_eq!(normalize(rows), expected, "{text}");
        let profile = summary.metrics.operator_profile().unwrap();
        assert_eq!(
            profile.iter().map(|op| op.db_hits).sum::<u64>(),
            summary.metrics.db_hits,
            "{text}: {profile:?}"
        );
        let point = &profile[points[0]];
        assert_eq!(point.id, points[0] as u64);
        assert_eq!(point.operator, "NodeIdSeek");
        assert_eq!(point.rows, 1, "{text}: {profile:?}");
        assert_eq!(point.db_hits, point_hits, "{text}: {profile:?}");
        let actual_fan = &profile[fans[0]];
        assert_eq!(actual_fan.id, fans[0] as u64);
        assert_eq!(actual_fan.operator, "LabelIndexScan");
        assert_eq!(actual_fan.rows, fan.rows, "{text}: {profile:?}");
        assert_eq!(actual_fan.db_hits, fan.db_hits, "{text}: {profile:?}");
    }
}

#[test]
fn incoming_pattern_bindings_keep_errors_before_index_and_early_where_filtering() {
    let db = fixture();
    for indexes in [true, false] {
        if !indexes {
            query(&db, "DROP INDEX item_values");
            query(&db, "DROP INDEX edge_values");
        }
        for (variable, pattern, predicate) in [
            ("r", "(n:Item)-[r:EDGE]->()", "n.v>99999 AND n.g=0"),
            ("r", "(n:Item), ()-[r:EDGE]->()", "n.v>99999 AND n.g=0"),
            ("r", "(n:Item), ()-[r:EDGE]->()", "n.v IN [] AND n.g=0"),
            ("p", "p=(n:Item)-[:EDGE]->()", "n.v>99999 AND n.g=0"),
            ("p", "(n:Item), p=()-[:EDGE]->()", "n.v>99999 AND n.g=0"),
            ("p", "(n:Item), p=()-[:EDGE]->()", "n.v IN [] AND n.g=0"),
            ("n", "(n:Item)", "n.v>99999 AND n.g=0"),
            ("f", "(n:Item)-[:EDGE]->(f:Fan)", "n.v>99999 AND n.g=0"),
            ("r", "(n:Item), ()-[r:EDGE]->()", "elementId(r)='r:999999'"),
            ("n", "(n:Item)", "elementId(n)='n:999999'"),
        ] {
            let text = format!("WITH $x AS {variable} MATCH {pattern} WHERE {predicate} RETURN 1");
            let oracle = format!(
                "WITH $x AS {variable} MATCH {pattern} WHERE ({predicate}) OR false RETURN 1"
            );
            let bound_node = (variable == "n" && pattern == "(n:Item)").then_some("n");
            assert_scope_fallback_plan(
                &db,
                &text,
                &oracle,
                BTreeMap::from([("x".to_owned(), Value::Integer(0))]),
                bound_node,
            );
            for value in [Value::Integer(0), Value::Null] {
                let params = BTreeMap::from([("x".to_owned(), value.clone())]);
                let actual = execute(&db, &text, params.clone(), ExecutionOptions::default());
                let expected = execute(&db, &oracle, params, ExecutionOptions::default());
                if value == Value::Integer(0) && matches!(variable, "r" | "p") {
                    assert!(
                        matches!(&actual, Err(error) if error.kind == QueryErrorKind::Semantic),
                        "{text}: {actual:?}"
                    );
                }
                match (actual, expected) {
                    (Ok((actual, _)), Ok((expected, _))) => assert_eq!(actual, expected, "{text}"),
                    (Err(actual), Err(expected)) => {
                        assert_eq!(actual.kind, expected.kind, "{text}");
                        assert_eq!(actual.message, expected.message, "{text}");
                    }
                    (actual, expected) => panic!("{text}: {actual:?} / {expected:?}"),
                }
            }
        }
        assert_bound_node_residuals(&db);
        let before = storage::branch_head(&db, "main").unwrap();
        let text = "WITH $x AS r MATCH (n:Item), ()-[r:EDGE]->() WHERE n.v>99999 AND n.g=0 SET n.touched=1 RETURN 1";
        assert_eq!(
            execute(
                &db,
                text,
                BTreeMap::from([("x".to_owned(), Value::Integer(0))]),
                ExecutionOptions::default()
            )
            .unwrap_err()
            .kind,
            QueryErrorKind::Semantic
        );
        assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
        for predicate in ["n.v>99999", "(n.v>99999) OR false"] {
            let text =
                format!("WITH $x AS r MATCH (n:Empty), ()-[r:EDGE]->() WHERE {predicate} RETURN 1");
            assert!(
                execute(
                    &db,
                    &text,
                    BTreeMap::from([("x".to_owned(), Value::Integer(0))]),
                    ExecutionOptions::default()
                )
                .unwrap()
                .0
                .is_empty()
            );
        }
    }
}

#[test]
fn long_incoming_pattern_names_keep_scope_fallback_bounded() {
    let db = fixture();
    let variable = "r".repeat(300 * 1024);
    let prefix = format!("WITH $x AS `{variable}` MATCH (n:Item), ()-[`{variable}`:EDGE]->()");
    for predicate in ["n.v>99999 AND n.g=0", "(n.v>99999 AND n.g=0) OR false"] {
        let text = format!("{prefix} WHERE {predicate} RETURN 1");
        let params = BTreeMap::from([("x".to_owned(), Value::Integer(0))]);
        let plan = explain_params(&db, &text, params.clone(), ExecutionOptions::default());
        assert!(!plan.contains("IndexSeek"));
        assert_eq!(
            execute(&db, &text, params, ExecutionOptions::default())
                .unwrap_err()
                .kind,
            QueryErrorKind::Semantic
        );
    }
}
