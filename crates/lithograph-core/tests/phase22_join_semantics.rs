use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, SnapshotSelector, prepare,
};
use lithograph_core::storage;
use rusqlite::Connection;

#[path = "phase22_join_semantics/boundaries.rs"]
mod boundaries;

fn empty_db() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    db
}

fn execute(
    db: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Result<Vec<Vec<Value>>, QueryError> {
    let prepared = prepare(db, text, params, options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(db, 2)?;
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(db)?;
            return Ok(rows);
        }
    }
}

fn run(db: &Connection, text: &str) -> Vec<Vec<Value>> {
    execute(db, text, BTreeMap::new(), ExecutionOptions::default())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn canonical(text: &str) -> String {
    let (prefix, output) = text.rsplit_once(" RETURN ").expect("RETURN fixture");
    if let Some((pattern, predicate)) = prefix.split_once(" WHERE ") {
        format!("{pattern} WHERE ({predicate}) OR false RETURN {output}")
    } else {
        format!("{prefix} WHERE true OR false RETURN {output}")
    }
}

fn bag(rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut rows = rows
        .into_iter()
        .map(|row| format!("{row:?}"))
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

fn assert_same_bag(db: &Connection, text: &str) {
    same_bag_options(db, text, ExecutionOptions::default());
}

fn assert_join_enabled(db: &Connection, text: &str) {
    let plan = prepare(db, text, BTreeMap::new(), ExecutionOptions::default())
        .unwrap()
        .physical
        .explain();
    assert!(
        plan.contains("NestedLoop"),
        "expected an actual selected join: {text}\n{plan}"
    );
    let oracle = prepare(
        db,
        &canonical(text),
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap()
    .physical
    .explain();
    assert!(
        !oracle.contains("NestedLoop"),
        "canonical oracle must retain original order: {oracle}"
    );
}

fn same_bag_options(db: &Connection, text: &str, options: ExecutionOptions) {
    let actual = execute(db, text, BTreeMap::new(), options.clone()).unwrap();
    let expected = execute(db, &canonical(text), BTreeMap::new(), options).unwrap();
    assert_eq!(bag(actual), bag(expected), "{text}");
}

fn fixture() -> Connection {
    let db = empty_db();
    run(
        &db,
        "CREATE (:Source:Visible {k:1}), (:Source:Visible {k:1}), (:Source:Visible {k:2}), \
        (:Source:Visible), (:Source:Visible {k:9}), (:Target:Visible {k:1}), \
        (:Target:Visible {k:1}), (:Target:Visible {k:2}), (:Target:Visible), (:Target:Hidden {k:1})",
    );
    for ddl in [
        "CREATE CONSTRAINT source_k FOR(n:Source) REQUIRE n.k IS :: INTEGER",
        "CREATE CONSTRAINT target_k FOR(n:Target) REQUIRE n.k IS :: INTEGER",
        "CREATE RANGE INDEX target_k_index FOR(n:Target) ON(n.k)",
    ] {
        run(&db, ddl);
    }
    db
}

#[test]
fn typed_property_join_preserves_duplicate_outer_and_inner_keys_and_nulls() {
    let db = fixture();
    let text = "MATCH(s:Source),(t:Target) WHERE s.k=t.k RETURN s.k,t.k";
    assert_join_enabled(&db, text);
    assert_same_bag(&db, text);
    assert_eq!(run(&db, text).len(), 7);
    for residual in [
        "s.k IN [1,2]",
        "s.k IS NOT NULL",
        "t.k>=1 AND t.k<=2",
        "true",
    ] {
        assert_same_bag(
            &db,
            &format!("MATCH(s:Source),(t:Target) WHERE s.k=t.k AND {residual} RETURN s.k,t.k"),
        );
    }
    let star = "MATCH(s:Source),(t:Target) WHERE s.k=t.k RETURN *";
    let columns = prepare(&db, star, BTreeMap::new(), ExecutionOptions::default())
        .unwrap()
        .columns;
    assert_eq!(columns, vec!["s", "t"]);
    assert_same_bag(&db, star);
}

#[test]
fn numeric_equality_keeps_integer_float_boundaries_nan_and_signed_zero() {
    let db = empty_db();
    run(
        &db,
        "UNWIND [0.0,-0.0,1.0,9007199254740992.0] AS k CREATE (:FloatKey {k:k})",
    );
    run(
        &db,
        "UNWIND [0,1,9007199254740992,9007199254740993] AS k CREATE (:IntKey {k:k})",
    );
    for ddl in [
        "CREATE CONSTRAINT float_k FOR(n:FloatKey) REQUIRE n.k IS :: FLOAT",
        "CREATE CONSTRAINT int_k FOR(n:IntKey) REQUIRE n.k IS :: INTEGER",
        "CREATE RANGE INDEX int_keys FOR(n:IntKey) ON(n.k)",
    ] {
        run(&db, ddl);
    }
    let text = "MATCH(f:FloatKey),(i:IntKey) WHERE f.k=i.k RETURN f.k,i.k";
    assert_join_enabled(&db, text);
    assert_same_bag(&db, text);
    assert_eq!(run(&db, text).len(), 4);
    let params = BTreeMap::from([("nan".into(), Value::Float(f64::NAN))]);
    execute(
        &db,
        "CREATE (:FloatKey {k:$nan})",
        params,
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_same_bag(&db, text);
    assert_eq!(run(&db, text).len(), 4);
}

#[test]
fn admitted_scalar_families_and_string_residuals_use_real_join_plans() {
    for (property_type, value, index_kind) in [
        ("BOOLEAN", "true", "RANGE"),
        ("STRING", "'hello'", "TEXT"),
        ("DATE", "date('2026-01-01')", "RANGE"),
        ("LOCAL TIME", "localtime('12:34:56')", "RANGE"),
        ("ZONED TIME", "time('12:34:56+01:00')", "RANGE"),
        (
            "LOCAL DATETIME",
            "localdatetime('2026-01-01T12:34:56')",
            "RANGE",
        ),
        (
            "ZONED DATETIME",
            "datetime('2026-01-01T12:34:56+01:00')",
            "RANGE",
        ),
        ("DURATION", "duration('P1D')", "RANGE"),
        ("POINT", "point({x:1,y:2})", "POINT"),
        (
            "UUID",
            "uuid('550e8400-e29b-41d4-a716-446655440000')",
            "RANGE",
        ),
    ] {
        let db = empty_db();
        run(
            &db,
            &format!("CREATE (:N {{k:{value}}}),(:N),(:M {{k:{value}}}),(:M {{k:{value}}})"),
        );
        for (name, label) in [("nk", "N"), ("mk", "M")] {
            run(
                &db,
                &format!(
                    "CREATE CONSTRAINT {name} FOR(n:{label}) REQUIRE n.k IS :: {property_type}"
                ),
            );
        }
        run(
            &db,
            &format!("CREATE {index_kind} INDEX nk_index FOR(n:N) ON(n.k)"),
        );
        let text = "MATCH(a:N),(b:M) WHERE a.k=b.k RETURN a.k,b.k";
        assert_join_enabled(&db, text);
        assert_same_bag(&db, text);
        assert_eq!(run(&db, text).len(), 2, "{property_type}");
        if property_type == "STRING" {
            for residual in [
                "a.k STARTS WITH 'he'",
                "a.k ENDS WITH 'lo'",
                "a.k CONTAINS 'ell'",
            ] {
                let text = format!("MATCH(a:N),(b:M) WHERE a.k=b.k AND {residual} RETURN a.k,b.k");
                assert_join_enabled(&db, &text);
                assert_same_bag(&db, &text);
                let masked = ExecutionOptions::parse_text(
                    r#"{"readAccess":{"query":{"denyProperties":["k"]}}}"#,
                )
                .unwrap();
                same_bag_options(&db, &text, masked.clone());
                assert!(
                    execute(&db, &text, BTreeMap::new(), masked)
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }
}

#[test]
fn date_range_keys_keep_a_real_scan_join_for_sparse_duplicate_keys() {
    let db = empty_db();
    run(
        &db,
        "UNWIND range(1,256) AS i CREATE (:DateInner {k:date({year:2026,ordinalDay:i})})",
    );
    run(
        &db,
        "UNWIND [5,5,211,211] AS i CREATE (:DateOuter {k:date({year:2026,ordinalDay:i})})",
    );
    run(&db, "CREATE (:DateOuter)");
    for ddl in [
        "CREATE CONSTRAINT date_inner_type FOR(n:DateInner) REQUIRE n.k IS :: DATE",
        "CREATE CONSTRAINT date_outer_type FOR(n:DateOuter) REQUIRE n.k IS :: DATE",
        "CREATE RANGE INDEX date_inner_keys FOR(n:DateInner) ON(n.k)",
    ] {
        run(&db, ddl);
    }
    let head = storage::branch_head(&db, "main").unwrap();
    storage::create_checkpoint(&db, head).unwrap();
    let ready: i64 = db
        .query_row(
            "SELECT count(*) FROM _lithograph_index_generations WHERE complete=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(ready > 0);
    db.execute_batch("SAVEPOINT date_dynamic_join_read")
        .unwrap();
    let text = "MATCH(o:DateOuter),(i:DateInner) WHERE o.k=i.k RETURN o.k,i.k";
    let expected = run(&db, &canonical(text));
    assert_eq!(expected.len(), 4);
    eprintln!("DATE canonical oracle returned {} rows", expected.len());
    let plan = prepare(&db, text, BTreeMap::new(), ExecutionOptions::default())
        .unwrap()
        .physical
        .explain();
    assert!(plan.contains("NestedLoop"), "{plan}");
    assert!(!plan.contains("IndexNestedLoop"), "{plan}");
    assert!(!plan.contains("IndexSeek"), "{plan}");
    assert!(plan.contains("DateInner"), "{plan}");
    eprintln!("DATE fallback physical plan:\n{plan}");
    assert_eq!(bag(run(&db, text)), bag(expected));
    db.execute_batch("RELEASE date_dynamic_join_read").unwrap();
}

#[test]
fn shared_nodes_parallel_relationships_self_loops_and_direction_keep_bags() {
    let db = empty_db();
    run(
        &db,
        "CREATE (a:N {k:1}), (b:N {k:2}), (c:N {k:3}), \
        (a)-[:E]->(b), (a)-[:E]->(b), (b)-[:E]->(b), (b)-[:F]->(c), (c)-[:F]->(b)",
    );
    for text in [
        "MATCH(a:N)-[r:E]->(b:N),(b)-[s:F]->(c:N) RETURN a.k,b.k,c.k",
        "MATCH(c:N)<-[s:F]-(b:N),(a:N)-[r:E]->(b) RETURN a.k,b.k,c.k",
        "MATCH(a:N)-[r:E]-(b:N),(b)-[s:E]-(c:N) RETURN a.k,b.k,c.k",
        "MATCH(a:N),(a)-[r:E]->(b:N),(b)-[s:F]-(c:N) RETURN a.k,b.k,c.k",
        "MATCH(n:N)-[r:E]->(),(n)-[s:F]->(),(n)-[t:F]-() RETURN n.k,r,s,t",
        "MATCH p=(a:N)-[r:E]->(b:N),(b)-[s:F]->(c:N) RETURN p,c.k",
    ] {
        assert_same_bag(&db, text);
    }
    assert_eq!(
        run(
            &db,
            "MATCH(a:N)-[r:E]->(b:N),(b)-[s:F]->(c:N) RETURN a.k,b.k,c.k"
        )
        .len(),
        3
    );
}

#[test]
fn identity_equality_uses_entity_kind_and_keeps_relationship_uniqueness() {
    let db = empty_db();
    run(
        &db,
        "CREATE (a:N {k:1}), (b:N {k:2}), (a)-[:E]->(b), (a)-[:E]->(b), (b)-[:E]->(b)",
    );
    for text in [
        "MATCH(a:N),(b:N) WHERE a=b RETURN a.k,b.k",
        "MATCH(a:N)-[r:E]->(b:N),(c:N) WHERE c=b RETURN a.k,b.k,c.k",
        "MATCH()-[r:E]->(),()-[s:E]->() WHERE r=s RETURN r,s",
    ] {
        assert_same_bag(&db, text);
    }
    assert!(run(&db, "MATCH()-[r:E]->(),()-[s:E]->() WHERE r=s RETURN r,s").is_empty());
}

#[test]
fn graph_view_and_query_property_masking_are_part_of_probe_visibility() {
    let db = fixture();
    let text = "MATCH(s:Source),(t:Target) WHERE s.k=t.k RETURN s.k,t.k";
    let visible =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#).unwrap();
    same_bag_options(&db, text, visible.clone());
    assert_eq!(
        execute(&db, text, BTreeMap::new(), visible).unwrap().len(),
        5
    );
    let masked =
        ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["k"]}}}"#)
            .unwrap();
    same_bag_options(&db, text, masked.clone());
    assert!(
        execute(&db, text, BTreeMap::new(), masked)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn query_only_preserves_dynamic_join_results_and_shows_the_actual_fallback() {
    let db = fixture();
    db.pragma_update(None, "query_only", true).unwrap();
    let text = "MATCH(s:Source),(t:Target) WHERE s.k=t.k RETURN s.k,t.k";
    assert_same_bag(&db, text);
    assert_eq!(run(&db, text).len(), 7);
    let prepared = prepare(&db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let plan = prepared.physical.explain();
    assert!(!plan.contains("IndexNestedLoop"), "{plan}");
    assert!(!plan.contains("IndexSeek"), "{plan}");
}

#[test]
fn historical_schema_family_and_index_definition_follow_the_pinned_snapshot() {
    let db = fixture();
    let historical = storage::branch_head(&db, "main").unwrap();
    run(&db, "DROP CONSTRAINT source_k");
    run(&db, "DROP CONSTRAINT target_k");
    run(&db, "MATCH(n:Source) SET n.k='same'");
    run(&db, "MATCH(n:Target) SET n.k='same'");
    run(
        &db,
        "CREATE CONSTRAINT source_k FOR(n:Source) REQUIRE n.k IS :: STRING",
    );
    run(
        &db,
        "CREATE CONSTRAINT target_k FOR(n:Target) REQUIRE n.k IS :: STRING",
    );
    let text = "MATCH(s:Source),(t:Target) WHERE s.k=t.k RETURN s.k,t.k";
    assert_same_bag(&db, text);
    assert_eq!(run(&db, text).len(), 25);
    let current = storage::branch_head(&db, "main").unwrap();
    let mut options = ExecutionOptions::default();
    options.snapshot = SnapshotSelector::Commit(historical.to_hex());
    same_bag_options(&db, text, options.clone());
    assert_eq!(
        execute(&db, text, BTreeMap::new(), options).unwrap().len(),
        7
    );
    assert_eq!(storage::branch_head(&db, "main").unwrap(), current);
}

#[test]
fn staged_writes_and_failed_join_mutation_keep_visibility_and_rollback() {
    let db = fixture();
    let before = storage::branch_head(&db, "main").unwrap();
    let staged = "CREATE (:Target {k:9}) WITH 1 AS unused MATCH(s:Source),(t:Target) WHERE s.k=t.k RETURN s.k,t.k";
    assert_eq!(run(&db, staged).len(), 8);
    let after_create = storage::branch_head(&db, "main").unwrap();
    assert_ne!(before, after_create);
    let error = execute(
        &db,
        "MATCH(s:Source),(t:Target) WHERE s.k=t.k CREATE (:NeverCommitted {v:1/0})",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    assert_eq!(storage::branch_head(&db, "main").unwrap(), after_create);
    assert_eq!(
        run(&db, "MATCH(n:NeverCommitted) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
    assert_same_bag(
        &db,
        "MATCH(s:Source),(t:Target) WHERE s.k=t.k RETURN s.k,t.k",
    );
    run(&db, "MATCH(t:Target) WHERE t.k=9 DELETE t");
    assert_eq!(
        run(
            &db,
            "MATCH(s:Source),(t:Target) WHERE s.k=t.k RETURN s.k,t.k"
        )
        .len(),
        7
    );
}
