//! Relationship ID access across graph-write and staged clause boundaries.

use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, QuerySummary, SnapshotSelector,
    prepare,
};
use lithograph_core::storage::{self, STORAGE_FORMAT};
use rusqlite::Connection;

fn run(
    db: &Connection,
    text: &str,
    id: Option<&str>,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), QueryError> {
    let params = id.map_or_else(BTreeMap::new, |id| {
        BTreeMap::from([("id".to_owned(), Value::String(id.to_owned()))])
    });
    let mut cursor = QueryCursor::new(prepare(db, text, params, options)?);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(db, 16)?;
        rows.extend(batch.rows);
        if batch.done {
            return Ok((rows, cursor.complete(db)?));
        }
    }
}

fn query(db: &Connection, text: &str) -> Vec<Vec<Value>> {
    run(db, text, None, ExecutionOptions::default())
        .unwrap_or_else(|error| panic!("{text}: {error}"))
        .0
}

fn database() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT NOT NULL,\
         database_id TEXT NOT NULL,storage_format INTEGER NOT NULL)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO main._lithograph_meta VALUES(1,'lithograph-format-v1',\
         '00000000-0000-4000-8000-000000000022',?1)",
        [STORAGE_FORMAT],
    )
    .unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    db
}

fn fixture() -> (Connection, String, String) {
    let db = database();
    let ids = query(
        &db,
        "CREATE (a:Visible {k:11})-[first:LINK {v:1}]->(b:Visible {k:12}),\
         (a)-[second:LINK {v:2}]->(b) RETURN elementId(first),elementId(second)",
    );
    let [Value::String(first), Value::String(second)] = ids[0].as_slice() else {
        panic!("Relationship identities");
    };
    let first = first.clone();
    let second = second.clone();
    query(
        &db,
        "UNWIND range(1,31) AS i CREATE (:Noise)-[:LINK {v:99}]->(:Noise) FINISH",
    );
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    (db, first, second)
}

fn state(db: &Connection) -> (storage::HashId, i64) {
    (
        storage::branch_head(db, "main").unwrap(),
        db.query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .unwrap(),
    )
}

fn value(db: &Connection, id: &str) -> Vec<Vec<Value>> {
    run(
        db,
        "MATCH ()-[r:LINK]->() WHERE elementId(r)=$id RETURN r.v",
        Some(id),
        ExecutionOptions::default(),
    )
    .unwrap()
    .0
}

fn assert_seek(summary: &QuerySummary) {
    let profile = summary.metrics.operator_profile().expect("PROFILE metrics");
    assert!(
        profile
            .iter()
            .any(|operator| { operator.operator == "RelationshipIdSeek" && operator.db_hits > 0 }),
        "PROFILE omitted the RelationshipIdSeek: {profile:?}"
    );
}

#[test]
fn relationship_seek_set_delete_publish_exactly_once_and_preserve_history() {
    let (db, first, second) = fixture();
    let (before, commits) = state(&db);
    let (rows, set) = run(
        &db,
        "PROFILE MATCH (a)-[r:LINK]->(b) WHERE elementId(r)=$id \
         SET r.v=17 RETURN r.v,a.k,b.k",
        Some(&first),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(17),
            Value::Integer(11),
            Value::Integer(12)
        ]]
    );
    assert_seek(&set);
    assert_eq!(set.counters.properties_set, 1);
    assert_eq!(state(&db).1, commits + 1);
    assert_ne!(state(&db).0, before);
    assert_eq!(value(&db, &second), vec![vec![Value::Integer(2)]]);
    let mut historical = ExecutionOptions::default();
    historical.snapshot = SnapshotSelector::Commit(before.to_hex());
    assert_eq!(
        run(
            &db,
            "MATCH ()-[r:LINK]->() WHERE elementId(r)=$id RETURN r.v",
            Some(&first),
            historical.clone()
        )
        .unwrap()
        .0,
        vec![vec![Value::Integer(1)]]
    );
    let (set_head, set_commits) = state(&db);
    let (rows, deleted) = run(
        &db,
        "PROFILE MATCH ()-[r:LINK]->() WHERE elementId(r)=$id DELETE r FINISH",
        Some(&first),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert!(rows.is_empty());
    assert_seek(&deleted);
    assert_eq!(deleted.counters.relationships_deleted, 1);
    assert_ne!(state(&db).0, set_head);
    assert_eq!(state(&db).1, set_commits + 1);
    assert!(value(&db, &first).is_empty());
    assert_eq!(value(&db, &second), vec![vec![Value::Integer(2)]]);
    assert_eq!(
        run(
            &db,
            "MATCH ()-[r:LINK]->() WHERE elementId(r)=$id RETURN r.v",
            Some(&first),
            historical
        )
        .unwrap()
        .0,
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn relationship_seek_write_and_late_projection_errors_leave_no_commit_or_value() {
    let (db, first, second) = fixture();
    for text in [
        "PROFILE MATCH ()-[r:LINK]->() WHERE elementId(r)=$id SET r.v=1/0 FINISH",
        "PROFILE MATCH ()-[r:LINK]->() WHERE elementId(r)=$id SET r.v=88 RETURN 1/0",
        "PROFILE MATCH ()-[r:LINK]->() WHERE elementId(r)=$id DELETE r WITH 1 AS x RETURN 1/0",
    ] {
        let before = state(&db);
        let error = run(&db, text, Some(&first), ExecutionOptions::default()).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Type, "{text}");
        assert_eq!(state(&db), before, "{text}");
        assert_eq!(value(&db, &first), vec![vec![Value::Integer(1)]]);
        assert_eq!(value(&db, &second), vec![vec![Value::Integer(2)]]);
    }
    let before = state(&db);
    let hidden =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Noise"]}}"#).unwrap();
    assert!(
        run(
            &db,
            "MATCH ()-[r:LINK]->() WHERE elementId(r)=$id SET r.v=9 RETURN r.v",
            Some(&first),
            hidden
        )
        .unwrap()
        .0
        .is_empty()
    );
    // A successful mutating query preserves write intent even when visibility
    // leaves it with an empty effective delta.
    assert_eq!(state(&db).1, before.1 + 1);
    assert_ne!(state(&db).0, before.0);
    assert_eq!(value(&db, &first), vec![vec![Value::Integer(1)]]);
}

#[test]
fn relationship_bindings_observe_each_completed_write_clause() {
    let (db, first, _) = fixture();
    let before = state(&db);
    let (rows, summary) = run(
        &db,
        "PROFILE MATCH ()-[r:LINK]->() WHERE elementId(r)=$id \
         SET r.v=23 WITH r MATCH ()-[same:LINK]->() WHERE elementId(same)=$id \
         SET same.v=r.v+1 RETURN r.v,same.v",
        Some(&first),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(24), Value::Integer(24)]]);
    assert_seek(&summary);
    assert_eq!(state(&db).1, before.1 + 1);
    assert_eq!(value(&db, &first), vec![vec![Value::Integer(24)]]);

    let fresh = database();
    let (rows, summary) = run(
        &fresh,
        "PROFILE CREATE (a:Stage {k:41})-[:LIVE {v:0}]->(b:Stage {k:42}) \
         WITH 1 AS seed MATCH (a:Stage)-[r:LIVE]->(b:Stage) WHERE elementId(r)='r:1' \
         SET r.v=3 RETURN r.v,a.k,b.k",
        None,
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Stage"]}}"#).unwrap(),
    )
    .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(3),
            Value::Integer(41),
            Value::Integer(42)
        ]]
    );
    assert_seek(&summary);
}

#[test]
fn relationship_delete_is_visible_to_the_next_match_in_the_same_execution() {
    let (db, first, second) = fixture();
    let before = state(&db);
    let (rows, summary) = run(
        &db,
        "PROFILE MATCH ()-[r:LINK]->() WHERE elementId(r)=$id DELETE r \
         WITH 1 AS deleted MATCH ()-[same:LINK]->() WHERE elementId(same)=$id RETURN same.v",
        Some(&first),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert!(rows.is_empty());
    assert_seek(&summary);
    assert_eq!(summary.counters.relationships_deleted, 1);
    assert_eq!(state(&db).1, before.1 + 1);
    assert_ne!(state(&db).0, before.0);
    assert!(value(&db, &first).is_empty());
    assert_eq!(value(&db, &second), vec![vec![Value::Integer(2)]]);
}

#[test]
fn fresh_identity_match_keeps_zero_outer_rows_and_preceding_error_timing() {
    let (db, first, _) = fixture();
    let before = state(&db);
    let (rows, summary) = run(
        &db,
        "PROFILE UNWIND [] AS seed MATCH ()-[r:LINK]->() WHERE elementId(r)=$id \
         SET r.v=1/0 FINISH",
        Some(&first),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert!(rows.is_empty());
    assert_eq!(summary.counters.properties_set, 0);
    assert_eq!(state(&db).1, before.1 + 1);
    assert_eq!(value(&db, &first), vec![vec![Value::Integer(1)]]);
    let before = state(&db);
    let error = run(
        &db,
        "PROFILE MATCH ()-[first:LINK]->() WHERE elementId(first)=$id SET first.v=50 \
         WITH first MATCH ()-[r:LINK]->() \
         WHERE r.v/0=1 AND elementId(r)='r:99999999' RETURN r.v",
        Some(&first),
        ExecutionOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    assert_eq!(state(&db), before);
    assert_eq!(value(&db, &first), vec![vec![Value::Integer(1)]]);
}

#[test]
fn new_label_type_identity_plans_recheck_current_staged_dictionary() {
    let db = database();
    let before = state(&db);
    let (rows, summary) = run(
        &db,
        "PROFILE CREATE (a:Future)-[r:FUTURE_TYPE]->(b:Future) WITH r DELETE r \
         WITH 1 AS seed MATCH (a:Future)-[gone:FUTURE_TYPE]->(b:Future) \
         WHERE elementId(gone)='r:1' RETURN 0",
        None,
        ExecutionOptions::default(),
    )
    .unwrap();
    assert!(rows.is_empty());
    assert_seek(&summary);
    // Creation and deletion of the same provisional ID cancel in the final
    // canonical net delta; only the two new endpoint Nodes remain.
    assert_eq!(summary.counters.relationships_created, 0);
    assert_eq!(summary.counters.relationships_deleted, 0);
    assert_eq!(summary.counters.nodes_created, 2);
    assert_eq!(state(&db).1, before.1 + 1);

    let fresh = database();
    let (rows, _) = run(
        &fresh,
        "PROFILE CREATE (a:Future)-[:RIGHT_TYPE]->(b:Future) WITH 1 AS seed \
         MATCH (a:Future)-[r:WRONG_TYPE]->(b:Future) WHERE elementId(r)='r:1' RETURN 0",
        None,
        ExecutionOptions::default(),
    )
    .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn identity_program_budget_exhaustion_keeps_the_actual_fallback_in_profile() {
    let (db, first, _) = fixture();
    let before = state(&db);
    let mut text = String::from(
        "PROFILE MATCH ()-[initial:LINK]->() WHERE elementId(initial)=$id \
         SET initial.v=1 WITH 1 AS seed ",
    );
    for index in 0..160 {
        text.push_str(&format!(
            "MATCH ()-[r{index}:LINK]->() WHERE elementId(r{index})=$id WITH 1 AS seed "
        ));
    }
    text.push_str("RETURN seed");
    let (rows, summary) = run(&db, &text, Some(&first), ExecutionOptions::default()).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    assert_eq!(state(&db).1, before.1 + 1);
    let profile = summary.metrics.operator_profile().unwrap();
    let seeks = profile
        .iter()
        .filter(|operator| operator.operator == "RelationshipIdSeek")
        .collect::<Vec<_>>();
    assert!(!seeks.is_empty() && seeks.len() <= 128);
    assert!(seeks.iter().all(|operator| operator.db_hits > 0));
    let scans = profile
        .iter()
        .filter(|operator| operator.operator == "NodeScan")
        .collect::<Vec<_>>();
    assert!(
        !scans.is_empty(),
        "global budget must retain a real fallback"
    );
    assert!(scans.iter().all(|operator| operator.db_hits > 3));
    assert_eq!(value(&db, &first), vec![vec![Value::Integer(1)]]);
}

#[test]
fn streamed_write_suffix_obeys_the_whole_program_identity_budget() {
    let (db, first, _) = fixture();
    let before = state(&db);
    let mut text = String::from("PROFILE WITH 1 AS seed ");
    for index in 0..80 {
        text.push_str(&format!(
            "MATCH (pa{index})-[p{index}:LINK]->(pb{index}) WHERE elementId(p{index})=$id "
        ));
    }
    text.push_str("SET p0.v=9 ");
    for index in 0..80 {
        text.push_str(&format!(
            "MATCH (sa{index})-[s{index}:LINK]->(sb{index}) WHERE elementId(s{index})=$id "
        ));
    }
    text.push_str("RETURN seed");
    let (rows, summary) = run(&db, &text, Some(&first), ExecutionOptions::default()).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    assert_eq!(state(&db).1, before.1 + 1);
    let profile = summary.metrics.operator_profile().unwrap();
    assert!(
        profile
            .iter()
            .any(|operator| operator.operator == "RelationshipIdSeek" && operator.db_hits > 0)
    );
    let scans = profile
        .iter()
        .filter(|operator| operator.operator == "NodeScan")
        .collect::<Vec<_>>();
    assert!(!scans.is_empty());
    assert!(
        scans.iter().all(|operator| operator.db_hits > 3),
        "unpermitted suffix identity must execute the displayed scan: {scans:?}"
    );
    assert_eq!(value(&db, &first), vec![vec![Value::Integer(9)]]);
}

#[test]
fn plain_write_suffix_relowers_its_identity_after_the_completed_set() {
    let (db, first, second) = fixture();
    let before = state(&db);
    let (rows, summary) = run(
        &db,
        "PROFILE MATCH ()-[r:LINK]->() WHERE elementId(r)=$id SET r.v=25 \
         MATCH ()-[same:LINK]->() WHERE elementId(same)=$id RETURN same.v",
        Some(&first),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(25)]]);
    assert_eq!(state(&db).1, before.1 + 1);
    assert_eq!(
        summary
            .metrics
            .operator_profile()
            .unwrap()
            .iter()
            .filter(|operator| operator.operator == "RelationshipIdSeek" && operator.db_hits > 0)
            .count(),
        2
    );
    assert_eq!(value(&db, &second), vec![vec![Value::Integer(2)]]);
}

#[test]
fn streamed_suffix_node_identity_matches_the_advertised_fresh_producer() {
    let (db, first, _) = fixture();
    let (rows, summary) = run(
        &db,
        "PROFILE WITH 1 AS seed MATCH ()-[r:LINK]->() WHERE elementId(r)=$id SET r.v=8 \
         MATCH (a)-[s:LINK]->(b) WHERE elementId(a)='n:1' AND elementId(s)=$id RETURN s.v",
        Some(&first),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(8)]]);
    let profile = summary.metrics.operator_profile().unwrap();
    assert!(
        profile
            .iter()
            .any(|operator| operator.operator == "RelationshipIdSeek" && operator.db_hits > 0)
    );
    assert!(
        profile
            .iter()
            .any(|operator| operator.operator == "NodeIdSeek" && operator.db_hits > 0),
        "{profile:?}"
    );
    assert!(
        profile
            .iter()
            .all(|operator| operator.operator != "NodeScan"),
        "{profile:?}"
    );
}

#[test]
fn optional_identity_permissions_preserve_small_quota_preparation_outcomes() {
    let (db, _, _) = fixture();
    let text = "EXPLAIN MATCH ()-[r:LINK]->() WHERE elementId(r)='r:1' SET r.v=1 \
                WITH 1 AS seed MATCH ()-[s:LINK]->() WHERE elementId(s)='r:1' RETURN 0";
    let oracle = text
        .replace("elementId(r)='r:1'", "(elementId(r)='r:1') OR false")
        .replace("elementId(s)='r:1'", "(elementId(s)='r:1') OR false");
    let mut successes = 0;
    for limit in [1, 64, 256, 1024, 4096, 8192, 65536] {
        let options =
            ExecutionOptions::parse_text(&format!(r#"{{"memoryLimitBytes":{limit}}}"#)).unwrap();
        let baseline = prepare(&db, &oracle, BTreeMap::new(), options.clone());
        let actual = prepare(&db, text, BTreeMap::new(), options);
        match (baseline, actual) {
            (Ok(_), Ok(prepared)) => {
                successes += 1;
                assert!(prepared.resources.used() <= limit);
            }
            (Err(expected), Err(actual)) => {
                assert_eq!(actual.kind, expected.kind, "quota {limit}");
                assert_eq!(actual.kind, QueryErrorKind::Resource);
            }
            (Ok(_), Err(actual)) => {
                panic!("identity-only planning failed at quota {limit}: {actual}")
            }
            (Err(expected), Ok(_)) => panic!(
                "identity-only planning changed a preparation error at quota {limit}: {expected}"
            ),
        }
    }
    assert!(successes > 0);
}

#[test]
fn temporary_identity_plan_storage_does_not_consume_the_executor_quota() {
    let (db, first, _) = fixture();
    let padding = "a".repeat(10_000);
    let text = format!(
        "UNWIND [] AS seed MATCH ()-[r:LINK]->() WHERE elementId(r)='r:1' SET r.v=1/0 \
         WITH r MATCH ()-[same:LINK]->() WHERE elementId(same)='r:1' \
         AND '{padding}'='{padding}' RETURN same.v"
    );
    let before = state(&db);
    let options = ExecutionOptions::parse_text(r#"{"memoryLimitBytes":131072}"#).unwrap();
    let (rows, _) = run(&db, &text, None, options).unwrap();
    assert!(rows.is_empty());
    assert_eq!(state(&db).1, before.1 + 1);
    assert_eq!(value(&db, &first), vec![vec![Value::Integer(1)]]);
}
