//! Completed-list advice must keep the original IN semantics and fresh reads.

use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, OperatorRuntimeMetrics, QueryCounters, QueryCursor, QueryError,
    QueryErrorKind, SnapshotSelector, prepare,
};
use lithograph_core::storage;
use rusqlite::Connection;

const MEMBERS: [usize; 3] = [3, 61, 113];
const NODE_LIST: &str = "MATCH(b:SipChosen) WITH collect(b) AS bs \
    MATCH(a:SipProbe) WHERE a IN bs RETURN elementId(a),a.seq";

struct ReadScope<'a>(&'a Connection);

impl<'a> ReadScope<'a> {
    fn new(db: &'a Connection) -> Self {
        db.execute_batch("SAVEPOINT phase22_sip_read").unwrap();
        let _: String = db
            .query_row(
                "SELECT database_id FROM main._lithograph_meta WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        Self(db)
    }
}

impl Drop for ReadScope<'_> {
    fn drop(&mut self) {
        self.0.execute_batch("RELEASE phase22_sip_read").unwrap();
    }
}

#[derive(Debug)]
struct Observed {
    rows: Vec<Vec<Value>>,
    columns: Vec<String>,
    operators: Vec<OperatorRuntimeMetrics>,
    summary_rows: u64,
    db_hits: u64,
    counters: QueryCounters,
}

fn execute(db: &Connection, text: &str, options: ExecutionOptions) -> Result<Observed, QueryError> {
    execute_with_batch(db, text, options, 7)
}

fn execute_with_batch(
    db: &Connection,
    text: &str,
    options: ExecutionOptions,
    batch_size: usize,
) -> Result<Observed, QueryError> {
    let prepared = prepare(db, text, BTreeMap::new(), options)?;
    let columns = prepared.columns.clone();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let result = (|| {
        let mut rows = Vec::new();
        loop {
            let batch = cursor.next_batch(db, batch_size)?;
            rows.extend(batch.rows);
            if batch.done {
                let summary = cursor.complete(db)?;
                let operators = summary.metrics.operator_profile().unwrap_or(&[]).to_vec();
                if !operators.is_empty() {
                    assert_eq!(
                        operators.iter().map(|op| op.db_hits).sum::<u64>(),
                        summary.metrics.db_hits,
                        "every actual storage hit needs its PROFILE slot: {text}; {operators:?}"
                    );
                }
                return Ok(Observed {
                    rows,
                    columns,
                    operators,
                    summary_rows: summary.metrics.rows,
                    db_hits: summary.metrics.db_hits,
                    counters: summary.counters.clone(),
                });
            }
        }
    })();
    drop(cursor);
    assert_eq!(
        resources.used(),
        0,
        "success/error must release query leases: {text}"
    );
    result
}

fn read(db: &Connection, text: &str, options: ExecutionOptions) -> Result<Observed, QueryError> {
    let _scope = ReadScope::new(db);
    execute(db, &format!("PROFILE {text}"), options)
}

fn write(db: &Connection, text: &str) -> Observed {
    execute(db, text, ExecutionOptions::default())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn canonical(text: &str) -> String {
    let (prefix, rest) = text.rsplit_once(" WHERE ").expect("final MATCH WHERE");
    let (predicate, output) = rest.split_once(" RETURN ").expect("final RETURN");
    format!("{prefix} WHERE ({predicate}) OR false RETURN {output}")
}

fn bag(rows: &[Vec<Value>]) -> Vec<String> {
    let mut rows = rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

fn checked(db: &Connection, text: &str, options: ExecutionOptions) -> (Observed, Observed) {
    let actual = read(db, text, options.clone()).unwrap();
    let original = read(db, &canonical(text), options).unwrap();
    assert_eq!(actual.columns, original.columns);
    assert_eq!(bag(&actual.rows), bag(&original.rows), "{text}");
    assert!(
        original.operators.iter().all(|op| op.semi_mask.is_none()),
        "OR-false must retain ordinary MATCH"
    );
    (actual, original)
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
        '00000000-0000-4000-8000-000000000026',?1)",
        [storage::STORAGE_FORMAT],
    )
    .unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    db
}

fn fixture() -> (Connection, Vec<Value>) {
    let db = database();
    let mut rows = write(
        &db,
        "UNWIND range(0,127) AS seq \
        CREATE(n:SipProbe:Visible {seq:seq,secret:seq}) RETURN elementId(n),n.seq",
    )
    .rows;
    rows.sort_by_key(|row| match row[1] {
        Value::Integer(seq) => seq,
        _ => panic!("seed sequence"),
    });
    assert_eq!(rows.len(), 128);
    let ids = rows
        .into_iter()
        .enumerate()
        .map(|(seq, row)| {
            assert_eq!(row[1], Value::Integer(seq as i64));
            assert!(matches!(row[0], Value::String(_)));
            row[0].clone()
        })
        .collect();
    write(
        &db,
        "MATCH(n:SipProbe) WHERE n.seq IN [3,61,113] SET n:SipChosen FINISH",
    );
    write(&db, "MATCH(n:SipProbe) WHERE n.seq=3 SET n:SipOnly FINISH");
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    (db, ids)
}

fn selected(ids: &[Value], members: &[usize], delta: i64) -> Vec<Vec<Value>> {
    members
        .iter()
        .map(|&seq| vec![ids[seq].clone(), Value::Integer(seq as i64 + delta)])
        .collect()
}

fn assert_three_point_reads(actual: &Observed, original: &Observed) {
    let masks = actual
        .operators
        .iter()
        .filter_map(|op| op.semi_mask.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(
        masks.len(),
        1,
        "one actually applied list: {:?}",
        actual.operators
    );
    assert_eq!(masks[0].keys, 3);
    assert_eq!(masks[0].seeks, 3);
    assert!(
        actual.db_hits < original.db_hits,
        "finite IDs must beat the 128-owner scan: {actual:?}; {original:?}"
    );
}

#[test]
fn completed_node_and_element_id_lists_use_real_finite_reads() {
    let (db, ids) = fixture();
    for text in [
        NODE_LIST,
        "MATCH(b:SipChosen) WITH collect(elementId(b)) AS ids \
         MATCH(a:SipProbe) WHERE elementId(a) IN ids RETURN elementId(a),a.seq",
    ] {
        let (actual, original) = checked(&db, text, ExecutionOptions::default());
        assert_eq!(bag(&actual.rows), bag(&selected(&ids, &MEMBERS, 0)));
        assert_three_point_reads(&actual, &original);
    }
}

#[test]
fn completed_collect_summary_counts_public_rows_once_across_caller_batches() {
    let (db, ids) = fixture();
    let cases: [(&str, &[usize]); 3] = [
        ("SipAbsent", &[]),
        ("SipOnly", &[3]),
        ("SipChosen", &MEMBERS),
    ];
    for (label, members) in cases {
        let expected = selected(&ids, members, 0);
        let body = format!(
            "MATCH(b:{label}) WITH collect(b) AS bs \
            MATCH(a:SipProbe) WHERE a IN bs RETURN elementId(a),a.seq"
        );
        for profiled in [false, true] {
            let text = format!("{}{body}", if profiled { "PROFILE " } else { "" });
            for batch_size in [1, 2] {
                let _scope = ReadScope::new(&db);
                let actual =
                    execute_with_batch(&db, &text, ExecutionOptions::default(), batch_size)
                        .unwrap();
                assert_eq!(
                    bag(&actual.rows),
                    bag(&expected),
                    "{text}; batch={batch_size}"
                );
                assert_eq!(
                    actual.summary_rows,
                    actual.rows.len() as u64,
                    "public rows counted once: {text}; batch={batch_size}"
                );
                if profiled {
                    let project = actual
                        .operators
                        .iter()
                        .rev()
                        .find(|operator| operator.operator == "Project")
                        .expect("final public projection");
                    assert_eq!(
                        project.rows,
                        actual.rows.len() as u64,
                        "final Project counts public rows: {text}; batch={batch_size}"
                    );
                } else {
                    assert!(actual.operators.is_empty());
                }
            }
        }
    }
}

#[test]
fn repeated_collect_members_do_not_multiply_the_match_bag() {
    let (db, ids) = fixture();
    let text = "MATCH(b:SipChosen) UNWIND [1,1] AS duplicate WITH collect(b) AS bs \
        MATCH(a:SipProbe) WHERE a IN bs RETURN elementId(a),a.seq";
    let (actual, original) = checked(&db, text, ExecutionOptions::default());
    assert_eq!(bag(&actual.rows), bag(&selected(&ids, &MEMBERS, 0)));
    assert_three_point_reads(&actual, &original);
}

#[test]
fn finite_sip_rows_are_counted_once_after_an_explicit_bound_point() {
    let (db, ids) = fixture();
    let text = "MATCH(b:SipChosen) WITH collect(b) AS bs MATCH(c:SipOnly) \
        WITH bs,c MATCH(c) WITH bs MATCH(a:SipProbe) WHERE a IN bs RETURN elementId(a),a.seq";
    let (actual, original) = checked(&db, text, ExecutionOptions::default());
    assert_eq!(bag(&actual.rows), bag(&selected(&ids, &MEMBERS, 0)));
    assert_three_point_reads(&actual, &original);
    // c is this query's only bound node. SIP retains its original access name,
    // so this independent NodeIdSeek proves the explicit-site path was active.
    let points = actual
        .operators
        .iter()
        .filter(|operator| operator.operator == "NodeIdSeek")
        .collect::<Vec<_>>();
    assert_eq!(points.len(), 1, "bound c point: {:?}", actual.operators);
    assert_eq!(points[0].rows, 1, "bound c point: {:?}", actual.operators);
    assert!(
        points[0].db_hits > 0,
        "bound c must actually validate its owner"
    );
    let owner = actual
        .operators
        .iter()
        .find(|operator| operator.semi_mask.is_some())
        .expect("actual finite-ID access owner");
    assert_eq!(
        owner.rows, 3,
        "SIP output rows must not be counted twice: {owner:?}"
    );
}

#[test]
fn empty_null_and_mixed_incoming_lists_preserve_original_in_order() {
    let (db, ids) = fixture();
    for (rhs, members) in [
        ("[]", vec![]),
        ("null", vec![]),
        ("[null]", vec![]),
        ("bs+[null]", vec![3]),
        ("bs+[1]", vec![3]),
    ] {
        let text = format!(
            "MATCH(b:SipOnly) WITH collect(b) AS bs WITH {rhs} AS members \
            MATCH(a:SipOnly) WHERE a IN members RETURN elementId(a),a.seq"
        );
        let (actual, _) = checked(&db, &text, ExecutionOptions::default());
        assert_eq!(
            bag(&actual.rows),
            bag(&selected(&ids, &members, 0)),
            "{rhs}"
        );
        if rhs == "bs+[1]" {
            assert!(
                actual.operators.iter().all(|op| op.semi_mask.is_none()),
                "mixed list must use original equality"
            );
        }
    }
    for rhs in ["[1]+bs", "[null,1]", "1"] {
        let text = format!(
            "MATCH(b:SipOnly) WITH collect(b) AS bs WITH {rhs} AS members \
            MATCH(a:SipOnly) WHERE a IN members RETURN elementId(a),a.seq"
        );
        let original = read(&db, &canonical(&text), ExecutionOptions::default()).unwrap_err();
        let actual = read(&db, &text, ExecutionOptions::default()).unwrap_err();
        assert_eq!(original.kind, QueryErrorKind::Type, "{rhs}");
        assert_eq!(actual.kind, original.kind);
        assert_eq!(actual.message, original.message);
    }
}

#[test]
fn empty_collect_and_zero_outer_are_different_safe_boundaries() {
    let (db, _) = fixture();
    for text in [
        "MATCH(b:SipAbsent) WITH collect(b) AS bs MATCH(a:SipProbe) WHERE a IN bs RETURN elementId(a),a.seq",
        "MATCH(b:SipChosen) WITH collect(b) AS bs UNWIND [] AS seed \
         WITH 1 AS members MATCH(a:SipProbe) WHERE a IN members RETURN elementId(a),a.seq",
    ] {
        let (actual, _) = checked(&db, text, ExecutionOptions::default());
        assert!(actual.rows.is_empty());
        assert!(
            actual
                .operators
                .iter()
                .filter_map(|op| op.semi_mask.as_ref())
                .all(|mask| mask.seeks == 0)
        );
    }
}

#[test]
fn graph_view_and_query_access_remain_authoritative_after_collect() {
    let (db, ids) = fixture();
    write(
        &db,
        "MATCH(n:SipProbe) WHERE n.seq=61 REMOVE n:Visible SET n:Hidden FINISH",
    );
    let visible =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#).unwrap();
    let (actual, _) = checked(&db, NODE_LIST, visible);
    assert_eq!(bag(&actual.rows), bag(&selected(&ids, &[3, 113], 0)));
    let hidden =
        ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["seq"]}}}"#)
            .unwrap();
    let (actual, _) = checked(&db, NODE_LIST, hidden);
    let expected = MEMBERS
        .iter()
        .map(|&seq| vec![ids[seq].clone(), Value::Null])
        .collect::<Vec<_>>();
    assert_eq!(bag(&actual.rows), bag(&expected));
}

#[test]
fn result_only_policy_keeps_core_identity_and_scalar_values_complete() {
    let (db, ids) = fixture();
    let mask =
        ExecutionOptions::parse_text(r#"{"readAccess":{"result":{"denyProperties":["seq"]}}}"#)
            .unwrap();
    let text = "MATCH(b:SipChosen) WITH collect(b) AS bs \
        MATCH(a:SipProbe) WHERE a IN bs RETURN elementId(a),a.seq,a";
    let (actual, _) = checked(&db, text, mask);
    assert_eq!(
        bag(&actual
            .rows
            .iter()
            .map(|row| row[..2].to_vec())
            .collect::<Vec<_>>()),
        bag(&selected(&ids, &MEMBERS, 0))
    );
    for row in actual.rows {
        // Result masking belongs to the public JSON encoder, not raw Core Values.
        let Value::Node(node) = &row[2] else {
            panic!("complete Core Node")
        };
        assert_eq!(Value::String(node.element_id.clone()), row[0]);
        assert_eq!(node.properties.get("seq"), Some(&row[1]));
        assert_eq!(node.properties.get("secret"), Some(&row[1]));
    }
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

#[test]
fn historical_collect_uses_its_pinned_values_and_membership() {
    let (db, ids) = fixture();
    let before = state(&db);
    write(
        &db,
        "MATCH(n:SipChosen) SET n.seq=n.seq+1000 REMOVE n:SipChosen FINISH",
    );
    let head = state(&db);
    assert_ne!(head.0, before.0);
    let mut historical = ExecutionOptions::default();
    historical.snapshot = SnapshotSelector::Commit(before.0.to_hex());
    let (actual, _) = checked(&db, NODE_LIST, historical);
    assert_eq!(bag(&actual.rows), bag(&selected(&ids, &MEMBERS, 0)));
    assert!(
        checked(&db, NODE_LIST, ExecutionOptions::default())
            .0
            .rows
            .is_empty()
    );
    assert_eq!(state(&db), head);
}

fn staged_case(action: &str, delta: i64, empty: bool) {
    let text = format!(
        "MATCH(b:SipChosen) WITH collect(b) AS bs UNWIND bs AS old \
        {action} WITH DISTINCT bs MATCH(a:SipProbe) WHERE a IN bs RETURN elementId(a),a.seq"
    );
    let (db, ids) = fixture();
    let before = state(&db);
    let actual = write(&db, &format!("PROFILE {text}"));
    let (original_db, _) = fixture();
    let original = write(&original_db, &format!("PROFILE {}", canonical(&text)));
    assert_eq!(bag(&actual.rows), bag(&original.rows));
    assert_eq!(actual.counters, original.counters);
    let expected = if empty {
        vec![]
    } else {
        selected(&ids, &MEMBERS, delta)
    };
    assert_eq!(bag(&actual.rows), bag(&expected));
    assert_eq!(state(&db).1, before.1 + 1);
    assert_ne!(state(&db).0, before.0);
}

#[test]
fn completed_list_rechecks_current_properties_labels_and_deletion_after_writes() {
    staged_case("SET old.seq=old.seq+1000", 1000, false);
    staged_case("REMOVE old:SipProbe", 0, true);
    staged_case("DELETE old", 0, true);
}

#[test]
fn fresh_staged_dictionary_and_late_errors_keep_write_publication_atomic() {
    let (db, ids) = fixture();
    let before = state(&db);
    let text = "CREATE(n:SipNewChosen:SipNewProbe {seq:300}) WITH 1 AS ready \
        MATCH(b:SipNewChosen) WITH collect(b) AS bs MATCH(a:SipNewProbe) \
        WHERE a IN bs RETURN elementId(a),a.seq";
    let actual = write(&db, &format!("PROFILE {text}"));
    assert_eq!(actual.rows.len(), 1);
    assert_eq!(actual.rows[0][1], Value::Integer(300));
    assert_eq!(actual.counters.nodes_created, 1);
    let direct = read(
        &db,
        "MATCH(a:SipNewProbe) RETURN elementId(a),a.seq",
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_eq!(bag(&actual.rows), bag(&direct.rows));
    assert_eq!(state(&db).1, before.1 + 1);
    let before = state(&db);
    let failing = "MATCH(b:SipChosen) WITH collect(b) AS bs UNWIND bs AS old \
        SET old.seq=old.seq+1000 WITH DISTINCT bs MATCH(a:SipProbe) WHERE a IN bs RETURN 1/0";
    for source in [failing.to_owned(), canonical(failing)] {
        let error = execute(
            &db,
            &format!("PROFILE {source}"),
            ExecutionOptions::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Type);
        assert_eq!(state(&db), before);
    }
    let (recovered, _) = checked(&db, NODE_LIST, ExecutionOptions::default());
    assert_eq!(bag(&recovered.rows), bag(&selected(&ids, &MEMBERS, 0)));
}
