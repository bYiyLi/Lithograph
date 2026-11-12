//! Ordered TopK uses an actual persistent numeric cursor, with the full Sort oracle as fallback.

use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, OperatorRuntimeMetrics, PhysicalOperator, QueryCursor, QueryError,
    QueryErrorKind, SnapshotSelector, prepare,
};
use lithograph_core::storage;
use rusqlite::Connection;

const TOPK: &str = "MATCH (n:TopKDoc) WHERE n.rank >= 0 AND n.rank < 1000000 \
    RETURN n.rank ORDER BY n.rank ASC SKIP $skip LIMIT $limit";
const SORT_ORACLE: &str = "MATCH (n:TopKDoc) WHERE (n.rank >= 0 AND n.rank < 1000000) OR false \
    RETURN n.rank ORDER BY n.rank ASC SKIP $skip LIMIT $limit";

struct ReadScope<'a>(&'a Connection);

impl<'a> ReadScope<'a> {
    fn new(db: &'a Connection) -> Self {
        db.execute_batch("SAVEPOINT phase22_topk_read").unwrap();
        Self(db)
    }
}

impl Drop for ReadScope<'_> {
    fn drop(&mut self) {
        self.0.execute_batch("RELEASE phase22_topk_read").unwrap();
    }
}

#[derive(Debug)]
struct Observed {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    plan: String,
    ordered: bool,
    external_sort: bool,
    profile: Option<Vec<OperatorRuntimeMetrics>>,
    db_hits: u64,
}

fn consume(
    db: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Result<Observed, QueryError> {
    let prepared = prepare(db, text, params, options)?;
    let columns = prepared.columns.clone();
    let plan = prepared.physical.explain();
    let ordered = prepared
        .physical
        .operators
        .iter()
        .any(|operator| matches!(operator, PhysicalOperator::OrderedTopK));
    let external_sort = prepared
        .physical
        .operators
        .iter()
        .any(|operator| matches!(operator, PhysicalOperator::ExternalSort));
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let result = (|| {
        let mut rows = Vec::new();
        loop {
            let done = {
                let batch = cursor.next_batch(db, 2)?;
                let done = batch.done;
                rows.extend(batch.rows);
                done
            };
            if done {
                let summary = cursor.complete(db)?;
                let profile = summary
                    .metrics
                    .operator_profile()
                    .map(|operators| operators.to_vec());
                if let Some(operators) = &profile {
                    assert_eq!(
                        operators
                            .iter()
                            .map(|operator| operator.db_hits)
                            .sum::<u64>(),
                        summary.metrics.db_hits,
                        "PROFILE must account for actual access: {plan}; {operators:?}"
                    );
                }
                return Ok(Observed {
                    columns,
                    rows,
                    plan,
                    ordered,
                    external_sort,
                    profile,
                    db_hits: summary.metrics.db_hits,
                });
            }
        }
    })();
    drop(cursor);
    assert_eq!(
        resources.used(),
        0,
        "success/error must release TopK resources"
    );
    result
}

fn write(db: &Connection, text: &str) {
    consume(db, text, BTreeMap::new(), ExecutionOptions::default())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"));
}

fn read(
    db: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Result<Observed, QueryError> {
    let _scope = ReadScope::new(db);
    consume(db, text, params, options)
}

fn fixture(typed: bool, keyed: bool) -> Connection {
    let db = Connection::open_in_memory().unwrap();
    initialize_fixture(&db, typed, keyed, 1000);
    db
}

fn initialize_fixture(db: &Connection, typed: bool, keyed: bool, nodes: i64) {
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY, magic TEXT, database_id TEXT, storage_format INTEGER);").unwrap();
    storage::create_storage_schema(db).unwrap();
    storage::initialize_root(db).unwrap();
    if nodes > 0 {
        write(
            db,
            &format!(
                "UNWIND range(0,{}) AS i CREATE (:TopKDoc {{rank:{}-i}}) FINISH",
                nodes - 1,
                nodes - 1
            ),
        );
    }
    if typed {
        write(
            db,
            "CREATE CONSTRAINT topk_rank_type FOR (n:TopKDoc) REQUIRE n.rank IS :: INTEGER",
        );
    }
    if keyed {
        // NODE KEY already owns the single-property Range Index.
        write(
            db,
            "CREATE CONSTRAINT topk_rank_key FOR (n:TopKDoc) REQUIRE n.rank IS NODE KEY",
        );
    } else {
        write(
            db,
            "CREATE RANGE INDEX topk_rank_range FOR (n:TopKDoc) ON (n.rank)",
        );
    }
    storage::create_checkpoint(db, storage::branch_head(db, "main").unwrap()).unwrap();
}

fn bounds(skip: i64, limit: i64) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("skip".to_owned(), Value::Integer(skip)),
        ("limit".to_owned(), Value::Integer(limit)),
    ])
}

fn integers(values: &[i64]) -> Vec<Vec<Value>> {
    values
        .iter()
        .map(|&value| vec![Value::Integer(value)])
        .collect()
}

fn operator_rows(observed: &Observed, name: &str) -> u64 {
    let operators = observed.profile.as_ref().expect("PROFILE operators");
    let matches = operators
        .iter()
        .filter(|operator| operator.operator == name)
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "{name}: {operators:?}");
    matches[0].rows
}

fn assert_sorted_fallback(observed: &Observed) {
    assert!(!observed.ordered, "{}", observed.plan);
    assert!(observed.external_sort, "{}", observed.plan);
}

#[test]
fn persistent_integer_topk_preserves_value_order_and_consumes_skip_plus_limit() {
    let db = fixture(true, true);
    let head = storage::branch_head(&db, "main").unwrap();
    // The smallest ranks belong to the largest NodeIds, not the owner-id prefix.
    let ids = read(
        &db,
        "MATCH (n:TopKDoc) WHERE n.rank IN [2,3,4] RETURN elementId(n),n.rank ORDER BY n.rank",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_eq!(
        ids.rows,
        vec![
            vec![Value::String("n:998".to_owned()), Value::Integer(2)],
            vec![Value::String("n:997".to_owned()), Value::Integer(3)],
            vec![Value::String("n:996".to_owned()), Value::Integer(4)],
        ]
    );
    let ordinary = read(&db, TOPK, bounds(2, 3), ExecutionOptions::default()).unwrap();
    let profiled = read(
        &db,
        &format!("PROFILE {TOPK}"),
        bounds(2, 3),
        ExecutionOptions::default(),
    )
    .unwrap();
    for observed in [&ordinary, &profiled] {
        assert_eq!(observed.columns, ["n.rank"]);
        assert_eq!(observed.rows, integers(&[2, 3, 4]));
        assert!(
            observed.ordered && !observed.external_sort,
            "{}",
            observed.plan
        );
    }
    assert_eq!(operator_rows(&profiled, "IndexSeek"), 5);
    assert_eq!(operator_rows(&profiled, "OrderedTopK"), 5);
    assert!(
        profiled.db_hits > 0 && profiled.db_hits < 600,
        "{profiled:?}"
    );
    let oracle = read(&db, SORT_ORACLE, bounds(2, 3), ExecutionOptions::default()).unwrap();
    assert_sorted_fallback(&oracle);
    assert_eq!(ordinary.rows, oracle.rows);
    assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
}

#[test]
fn ordered_topk_crosses_numeric_pages_and_handles_eof_before_the_requested_rank() {
    let db = fixture(true, true);
    for (skip, limit, expected, consumed) in [
        (257, 2, integers(&[257, 258]), 259),
        (1001, 3, Vec::new(), 1000),
    ] {
        let actual = read(
            &db,
            &format!("PROFILE {TOPK}"),
            bounds(skip, limit),
            ExecutionOptions::default(),
        )
        .unwrap();
        assert!(actual.ordered && !actual.external_sort, "{}", actual.plan);
        assert_eq!(actual.rows, expected);
        assert_eq!(operator_rows(&actual, "IndexSeek"), consumed);
        assert_eq!(operator_rows(&actual, "OrderedTopK"), consumed);
    }
}

#[test]
fn projection_alias_and_tail_errors_keep_the_original_sort_boundary() {
    let db = fixture(true, true);
    for (text, message) in [
        (
            "MATCH (n:TopKDoc) WHERE n.rank>=0 AND n.rank<1000000 \
             RETURN n.rank AS n ORDER BY n.rank ASC SKIP 2 LIMIT 3",
            "property access requires a structural value or null",
        ),
        (
            "MATCH (n:TopKDoc) WHERE n.rank>=0 AND n.rank<1000000 \
             RETURN n.rank,1/(n.rank-100) AS danger ORDER BY n.rank ASC SKIP 2 LIMIT 3",
            "division by zero",
        ),
        (
            "MATCH (n:TopKDoc) WHERE n.rank>=0 AND n.rank<1000000 \
             RETURN n.rank ORDER BY n.rank ASC,1/(n.rank-100) ASC SKIP 2 LIMIT 3",
            "division by zero",
        ),
    ] {
        let prepared = prepare(&db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
        assert!(
            !prepared
                .physical
                .operators
                .contains(&PhysicalOperator::OrderedTopK),
            "{}",
            prepared.physical.explain()
        );
        assert!(
            prepared
                .physical
                .operators
                .contains(&PhysicalOperator::ExternalSort)
        );
        drop(prepared);
        let error = read(&db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Type, "{text}: {error:?}");
        assert_eq!(error.message, message, "{text}");
    }
}

#[test]
fn descending_extra_projection_and_zero_limit_keep_external_sort() {
    let db = fixture(true, true);
    for (text, expected) in [
        (
            "MATCH (n:TopKDoc) WHERE n.rank>=0 AND n.rank<1000000 \
             RETURN n.rank ORDER BY n.rank DESC SKIP 2 LIMIT 3",
            integers(&[997, 996, 995]),
        ),
        (
            "MATCH (n:TopKDoc) WHERE n.rank>=0 AND n.rank<1000000 \
             RETURN n.rank,n.rank AS again ORDER BY n.rank ASC SKIP 2 LIMIT 3",
            vec![
                vec![Value::Integer(2), Value::Integer(2)],
                vec![Value::Integer(3), Value::Integer(3)],
                vec![Value::Integer(4), Value::Integer(4)],
            ],
        ),
        (
            "MATCH (n:TopKDoc) WHERE n.rank>=0 AND n.rank<1000000 \
             RETURN n.rank ORDER BY n.rank ASC LIMIT 0",
            Vec::new(),
        ),
    ] {
        let actual = read(&db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
        assert_sorted_fallback(&actual);
        assert_eq!(actual.rows, expected, "{text}");
    }
    // LIMIT 0 must not erase the old barrier's evaluation of the bad tail row.
    let error = read(
        &db,
        "MATCH (n:TopKDoc) WHERE n.rank>=0 AND n.rank<1000000 \
         RETURN n.rank,1/(n.rank-100) ORDER BY n.rank ASC LIMIT 0",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    assert_eq!(error.message, "division by zero");
}

#[test]
fn observed_distinct_values_do_not_replace_unique_or_integer_schema_proofs() {
    for (typed, keyed) in [(true, false), (false, true)] {
        let db = fixture(typed, keyed);
        let actual = read(&db, TOPK, bounds(2, 3), ExecutionOptions::default()).unwrap();
        assert_sorted_fallback(&actual);
        assert_eq!(actual.rows, integers(&[2, 3, 4]));
    }
}

#[test]
fn duplicated_cutoff_values_keep_the_full_tie_bag() {
    let db = fixture(true, false);
    write(&db, "MATCH (n:TopKDoc) SET n.rank=n.rank/2 FINISH");
    write(
        &db,
        "CALL lithograph.index.rebuild('topk_rank_range','branch/main') \
         YIELD name RETURN name",
    );
    let actual = read(&db, TOPK, bounds(2, 3), ExecutionOptions::default()).unwrap();
    let oracle = read(&db, SORT_ORACLE, bounds(2, 3), ExecutionOptions::default()).unwrap();
    assert_sorted_fallback(&actual);
    assert_sorted_fallback(&oracle);
    assert_eq!(actual.rows, integers(&[1, 1, 2]));
    assert_eq!(actual.rows, oracle.rows);
}

#[test]
fn unique_with_a_missing_property_requires_the_independent_existence_proof() {
    let db = fixture(true, false);
    write(&db, "DROP INDEX topk_rank_range");
    write(
        &db,
        "CREATE CONSTRAINT topk_rank_unique FOR (n:TopKDoc) REQUIRE n.rank IS UNIQUE",
    );
    write(&db, "CREATE (:TopKDoc) FINISH");
    write(
        &db,
        "CALL lithograph.index.rebuild('topk_rank_unique','branch/main') \
         YIELD name RETURN name",
    );
    let nullable = read(&db, TOPK, bounds(2, 3), ExecutionOptions::default()).unwrap();
    assert_sorted_fallback(&nullable);
    assert_eq!(nullable.rows, integers(&[2, 3, 4]));
    write(
        &db,
        "MATCH (n:TopKDoc) WHERE n.rank IS NULL DELETE n FINISH",
    );
    write(
        &db,
        "CREATE CONSTRAINT topk_rank_exists FOR (n:TopKDoc) REQUIRE n.rank IS NOT NULL",
    );
    write(
        &db,
        "CALL lithograph.index.rebuild('topk_rank_unique','branch/main') \
         YIELD name RETURN name",
    );
    let proved = read(
        &db,
        &format!("PROFILE {TOPK}"),
        bounds(2, 3),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert!(proved.ordered && !proved.external_sort, "{}", proved.plan);
    assert_eq!(proved.rows, integers(&[2, 3, 4]));
    assert_eq!(operator_rows(&proved, "OrderedTopK"), 5);
}

#[test]
fn restricted_view_and_hidden_sort_property_never_use_canonical_index_order() {
    let db = fixture(true, true);
    let restricted =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["TopKDoc"]}}"#).unwrap();
    let actual = read(&db, TOPK, bounds(2, 3), restricted).unwrap();
    assert_sorted_fallback(&actual);
    assert_eq!(actual.rows, integers(&[2, 3, 4]));
    let hidden =
        ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["rank"]}}}"#)
            .unwrap();
    let actual = read(&db, TOPK, bounds(2, 3), hidden).unwrap();
    assert_sorted_fallback(&actual);
    assert!(actual.rows.is_empty());
}

#[test]
fn host_query_only_uses_sort_without_creating_host_temp_objects() {
    let db = fixture(true, true);
    db.pragma_update(None, "query_only", true).unwrap();
    let catalog = || {
        db.prepare("SELECT type,name FROM sqlite_temp_master ORDER BY type,name")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let before = catalog();
    let head = storage::branch_head(&db, "main").unwrap();
    let actual = read(
        &db,
        &format!("PROFILE {TOPK}"),
        bounds(2, 3),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_sorted_fallback(&actual);
    assert_eq!(actual.rows, integers(&[2, 3, 4]));
    assert_eq!(catalog(), before);
    assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
}

#[test]
fn absent_derived_generation_on_a_fresh_connection_keeps_the_sort_oracle() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("topk-cache-miss.sqlite3");
    let head = {
        let db = Connection::open(&path).unwrap();
        initialize_fixture(&db, true, true, 1000);
        let head = storage::branch_head(&db, "main").unwrap();
        // Disposable derived-state miss, as in the existing query_only tests.
        // Both entries and manifests disappear; canonical graph/schema remain.
        db.execute_batch(
            "DELETE FROM _lithograph_index_entries; DELETE FROM _lithograph_index_generations;",
        )
        .unwrap();
        head
    };
    let db = Connection::open(&path).unwrap();
    let actual = read(&db, TOPK, bounds(2, 3), ExecutionOptions::default()).unwrap();
    assert_sorted_fallback(&actual);
    assert_eq!(actual.rows, integers(&[2, 3, 4]));
    assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
}

#[test]
fn empty_single_and_short_inputs_preserve_the_complete_ordered_result() {
    for (nodes, skip, limit, expected) in [
        (0, 0, 3, Vec::new()),
        (1, 0, 3, integers(&[0])),
        (3, 2, 3, integers(&[2])),
    ] {
        let db = Connection::open_in_memory().unwrap();
        initialize_fixture(&db, true, true, nodes);
        let actual = read(
            &db,
            &format!("PROFILE {TOPK}"),
            bounds(skip, limit),
            ExecutionOptions::default(),
        )
        .unwrap();
        assert_eq!(actual.columns, ["n.rank"]);
        assert_eq!(actual.rows, expected);
        // An empty label or a different final leaf may conservatively retain
        // Sort. Only an actually admitted numeric path owns TopK row counts.
        if actual.ordered {
            assert!(!actual.external_sort, "{}", actual.plan);
            assert_eq!(operator_rows(&actual, "IndexSeek"), nodes as u64);
            assert_eq!(operator_rows(&actual, "OrderedTopK"), nodes as u64);
        } else {
            assert_sorted_fallback(&actual);
        }
    }
}

#[test]
fn historical_exact_generation_is_separate_from_the_current_delta() {
    let db = fixture(true, true);
    let historical = storage::branch_head(&db, "main").unwrap();
    let exact_generations = |commit: storage::HashId| {
        db.query_row(
            "SELECT count(*) FROM _lithograph_index_generations \
             WHERE anchor_commit=?1 AND complete=1",
            [commit.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert_eq!(exact_generations(historical), 1);
    write(
        &db,
        "MATCH (n:TopKDoc) WHERE n.rank=0 SET n.rank=1000 FINISH",
    );
    let current = storage::branch_head(&db, "main").unwrap();
    assert_ne!(current, historical);
    assert_eq!(exact_generations(current), 0);
    let mut options = ExecutionOptions::default();
    options.snapshot = SnapshotSelector::Commit(historical.to_hex());
    let old = read(&db, TOPK, bounds(2, 3), options).unwrap();
    assert!(old.ordered && !old.external_sort, "{}", old.plan);
    assert_eq!(old.rows, integers(&[2, 3, 4]));
    let changed = read(&db, TOPK, bounds(2, 3), ExecutionOptions::default()).unwrap();
    assert_sorted_fallback(&changed);
    assert_eq!(changed.rows, integers(&[3, 4, 5]));
    assert_eq!(exact_generations(current), 0, "read must not rebuild main");
    assert_eq!(storage::branch_head(&db, "main").unwrap(), current);
}

#[test]
fn a_cross_page_live_batch_can_close_or_interrupt_without_retaining_topk_state() {
    let db = fixture(true, true);
    let head = storage::branch_head(&db, "main").unwrap();
    for interrupt in [false, true] {
        let scope = ReadScope::new(&db);
        let prepared = prepare(
            &db,
            &format!("PROFILE {TOPK}"),
            bounds(257, 20),
            ExecutionOptions::default(),
        )
        .unwrap();
        assert!(
            prepared
                .physical
                .operators
                .contains(&PhysicalOperator::OrderedTopK)
        );
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        let batch = cursor.next_batch(&db, 2).unwrap();
        assert_eq!(batch.rows, integers(&[257, 258]));
        assert!(!batch.done);
        drop(batch);
        if interrupt {
            let error = cursor
                .next_batch_with_interrupt(&db, 2, &|| true)
                .unwrap_err();
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
            assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
        } else {
            cursor.cancel(&db).unwrap();
        }
        drop(cursor);
        assert_eq!(resources.used(), 0);
        drop(scope);
        assert!(db.is_autocommit());
        assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
        let fresh = read(&db, TOPK, bounds(2, 3), ExecutionOptions::default()).unwrap();
        assert!(fresh.ordered && !fresh.external_sort, "{}", fresh.plan);
        assert_eq!(fresh.rows, integers(&[2, 3, 4]));
    }
}

#[test]
fn moving_the_connection_wrapper_keeps_the_same_ordered_numeric_source() {
    let db = Box::new(fixture(true, true));
    db.execute_batch("SAVEPOINT phase22_topk_read").unwrap();
    let prepared = prepare(
        &db,
        &format!("PROFILE {TOPK}"),
        bounds(2, 3),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert!(
        prepared
            .physical
            .operators
            .contains(&PhysicalOperator::OrderedTopK)
    );
    assert!(
        !prepared
            .physical
            .operators
            .contains(&PhysicalOperator::ExternalSort)
    );
    let resources = prepared.resources.clone();
    // The SQLite handle and transaction survive a move of its Rust wrapper.
    // Extension prepare/next likewise use separate wrappers for one handle.
    let db = *db;
    let scope = ReadScope(&db);
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let done = {
            let batch = cursor.next_batch(&db, 2).unwrap();
            let done = batch.done;
            rows.extend(batch.rows);
            done
        };
        if done {
            break;
        }
    }
    assert_eq!(rows, integers(&[2, 3, 4]));
    let summary = cursor.complete(&db).unwrap();
    let profile = summary.metrics.operator_profile().unwrap();
    let topk = profile
        .iter()
        .find(|operator| operator.operator == "OrderedTopK")
        .unwrap();
    assert_eq!(topk.rows, 5);
    drop(summary);
    drop(cursor);
    assert_eq!(resources.used(), 0);
    drop(scope);
    assert!(db.is_autocommit());
}
