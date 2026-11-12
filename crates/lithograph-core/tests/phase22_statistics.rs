use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QuerySummary, prepare};
use lithograph_core::storage;
use rusqlite::Connection;

fn run(
    db: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> (Vec<Vec<Value>>, QuerySummary) {
    let prepared = prepare(db, text, params, options).expect(text);
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(db, 32).expect(text);
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(db).unwrap();
            return (rows, batch.summary.unwrap());
        }
    }
}

fn query(db: &Connection, text: &str) {
    run(db, text, BTreeMap::new(), ExecutionOptions::default());
}

fn database() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    db
}

fn checkpoint(db: &Connection) {
    storage::create_checkpoint(db, storage::branch_head(db, "main").unwrap()).unwrap();
}

fn explain(db: &Connection, text: &str, value: i64, options: ExecutionOptions) -> String {
    let (rows, _) = run(
        db,
        &format!("EXPLAIN {text}"),
        BTreeMap::from([("x".to_owned(), Value::Integer(value))]),
        options,
    );
    let Value::String(plan) = &rows[0][0] else {
        panic!("expected plan text")
    };
    plan.clone()
}

fn skewed_database() -> Connection {
    let db = database();
    query(
        &db,
        "UNWIND range(0,383) AS i CREATE (:Common {k:i,v:CASE WHEN i<368 THEN 0 ELSE i END}) FINISH",
    );
    query(
        &db,
        "MATCH(n:Common) WHERE n.k<15 OR n.k=383 SET n:Rare FINISH",
    );
    query(
        &db,
        "CREATE CONSTRAINT value_type FOR(n:Common) REQUIRE n.v IS :: INTEGER",
    );
    query(&db, "CREATE RANGE INDEX values_range FOR(n:Common) ON(n.v)");
    checkpoint(&db);
    db
}

#[test]
fn hot_and_cold_parameters_choose_different_real_access_paths() {
    let db = skewed_database();
    let options =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Rare"]}}"#).unwrap();
    let text = "MATCH(n:Common) WHERE n.v=$x RETURN n.k ORDER BY n.k";
    let mut cold_hits = 0;
    for (value, access, expected) in [
        (0, "LabelIndexScan", (0..15).collect::<Vec<_>>()),
        (383, "IndexSeek", vec![383]),
        (0, "LabelIndexScan", (0..15).collect::<Vec<_>>()),
    ] {
        let plan = explain(&db, text, value, options.clone());
        assert!(plan.contains(access), "{plan}");
        let (rows, summary) = run(
            &db,
            &format!("PROFILE {text}"),
            BTreeMap::from([("x".to_owned(), Value::Integer(value))]),
            options.clone(),
        );
        assert_eq!(
            rows,
            expected
                .into_iter()
                .map(|value| vec![Value::Integer(value)])
                .collect::<Vec<_>>()
        );
        let operators = summary.metrics.operator_profile().unwrap();
        let operator = operators.iter().find(|op| op.operator == access).unwrap();
        assert!(
            operator.db_hits > 0 && operator.db_hits < 64,
            "{operators:?}"
        );
        if value == 383 {
            cold_hits = operator.db_hits;
        }
    }
    query(&db, "DROP INDEX values_range");
    let (rows, summary) = run(
        &db,
        &format!("PROFILE {text}"),
        BTreeMap::from([("x".to_owned(), Value::Integer(383))]),
        options,
    );
    assert_eq!(rows, vec![vec![Value::Integer(383)]]);
    let operators = summary.metrics.operator_profile().unwrap();
    let scan_hits = operators
        .iter()
        .find(|op| op.operator == "LabelIndexScan")
        .unwrap()
        .db_hits;
    assert!(
        scan_hits >= cold_hits * 4,
        "cold index={cold_hits}, label={scan_hits}"
    );
}

#[test]
fn typed_low_degree_start_uses_adjacency_instead_of_global_type_index() {
    let db = database();
    query(
        &db,
        "UNWIND range(0,511) AS i CREATE (:Noise)-[:T]->(:Noise) FINISH",
    );
    query(
        &db,
        "UNWIND range(0,1) AS i CREATE (a:Small {k:i})-[:T]->(b:Sink {k:i}), (b)-[:T]->(a) FINISH",
    );
    query(
        &db,
        "CREATE LOOKUP INDEX relationship_types FOR ()-[r]-() ON EACH type(r)",
    );
    checkpoint(&db);
    for text in [
        "MATCH(a:Small)-[r:T]->(b) RETURN a.k,b.k ORDER BY a.k",
        "MATCH(a:Small)<-[r:T]-(b) RETURN a.k,b.k ORDER BY a.k",
    ] {
        let plan = explain(&db, text, 0, ExecutionOptions::default());
        assert!(plan.contains("AdjacencySeek"), "{plan}");
        assert!(!plan.contains("relationship_types"), "{plan}");
        let (rows, summary) = run(
            &db,
            &format!("PROFILE {text}"),
            BTreeMap::new(),
            ExecutionOptions::default(),
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(0), Value::Integer(0)],
                vec![Value::Integer(1), Value::Integer(1)]
            ]
        );
        let operators = summary.metrics.operator_profile().unwrap();
        assert!(
            operators.iter().any(|op| op.operator == "AdjacencySeek"),
            "{operators:?}"
        );
        assert!(
            operators.iter().map(|op| op.db_hits).sum::<u64>() < 32,
            "{operators:?}"
        );
    }
}

#[test]
fn absent_derived_statistics_preserve_complete_execution() {
    let db = skewed_database();
    let text = "MATCH(n:Common) WHERE n.v=$x RETURN n.k ORDER BY n.k";
    let expected = run(
        &db,
        text,
        BTreeMap::from([("x".to_owned(), Value::Integer(0))]),
        ExecutionOptions::default(),
    )
    .0;
    db.execute("DELETE FROM _lithograph_index_entries", [])
        .unwrap();
    db.execute("DELETE FROM _lithograph_index_generations", [])
        .unwrap();
    db.execute("UPDATE _lithograph_checkpoints SET metadata=X'00'", [])
        .unwrap();
    let (actual, _) = run(
        &db,
        text,
        BTreeMap::from([("x".to_owned(), Value::Integer(0))]),
        ExecutionOptions::default(),
    );
    assert_eq!(expected.len(), 368);
    assert_eq!(actual, expected);
}

#[test]
fn fixed_node_and_relationship_identity_skip_unused_total_and_degree_collection() {
    let db = database();
    query(&db, "CREATE (:Small)-[:T]->(:Sink) FINISH");
    query(
        &db,
        "CREATE LOOKUP INDEX relationship_types FOR ()-[r]-() ON EACH type(r)",
    );
    checkpoint(&db);
    for text in [
        "MATCH(a:Small)-[r:T]->(b) WHERE elementId(a)='n:1' RETURN 1",
        "MATCH(a:Small)-[r:T]->(b) WHERE elementId(r)='r:1' RETURN 1",
    ] {
        let prepared = prepare(&db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
        assert_eq!(prepared.statistics, Default::default());
        let mut cursor = QueryCursor::new(prepared);
        let batch = cursor.next_batch(&db, 32).unwrap();
        assert!(batch.done);
        assert_eq!(batch.rows, vec![vec![Value::Integer(1)]]);
        cursor.complete(&db).unwrap();
    }
}
