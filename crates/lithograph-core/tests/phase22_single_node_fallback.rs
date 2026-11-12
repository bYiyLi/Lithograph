//! Bounded program fallback keeps the complete per-outer MATCH before WHERE.

use std::cell::Cell;
use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, OperatorRuntimeMetrics, PhysicalOperator, PhysicalPlan, QueryCounters,
    QueryCursor, QueryError, QueryErrorKind, SnapshotSelector, prepare,
};
use lithograph_core::storage;
use rusqlite::Connection;

const MEMBERS: [usize; 3] = [1, 3, 6];
const FALLBACK: &str = "MATCH(b:Chosen) WITH collect(b) AS bs MATCH(a:Probe) \
    WHERE (a IN bs) OR false RETURN elementId(a),a.seq";

struct ReadScope<'a>(&'a Connection);

impl<'a> ReadScope<'a> {
    fn new(db: &'a Connection) -> Self {
        db.execute_batch("SAVEPOINT phase22_single_node_read")
            .unwrap();
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
        self.0
            .execute_batch("RELEASE phase22_single_node_read")
            .unwrap();
    }
}

#[derive(Debug)]
struct Observed {
    rows: Vec<Vec<Value>>,
    physical: PhysicalPlan,
    operators: Vec<OperatorRuntimeMetrics>,
    summary_rows: u64,
    counters: QueryCounters,
}

fn execute(db: &Connection, text: &str, options: ExecutionOptions) -> Result<Observed, QueryError> {
    execute_with_shared_lease(db, text, options, 0)
}

fn execute_with_shared_lease(
    db: &Connection,
    text: &str,
    options: ExecutionOptions,
    held_bytes: u64,
) -> Result<Observed, QueryError> {
    let prepared = prepare(db, text, BTreeMap::new(), options)?;
    let physical = prepared.physical.clone();
    let resources = prepared.resources.clone();
    let held = resources
        .reserve(held_bytes)
        .expect("reserve test-only shared-ledger pressure after prepare");
    let mut cursor = QueryCursor::new(prepared);
    let result = (|| {
        let mut rows = Vec::new();
        loop {
            let batch = cursor.next_batch(db, 2)?;
            rows.extend(batch.rows);
            if batch.done {
                let summary = cursor.complete(db)?;
                let operators = summary.metrics.operator_profile().unwrap_or(&[]).to_vec();
                if !operators.is_empty() {
                    assert_eq!(
                        operators.iter().map(|op| op.db_hits).sum::<u64>(),
                        summary.metrics.db_hits,
                        "every hit retains an actual slot: {text}; {operators:?}"
                    );
                    assert!(
                        operators.iter().all(|op| op.semi_mask.is_none()),
                        "OR-false/ordinary fallback must not claim a semi-mask: {text}"
                    );
                }
                return Ok(Observed {
                    rows,
                    physical,
                    operators,
                    summary_rows: summary.metrics.rows,
                    counters: summary.counters.clone(),
                });
            }
        }
    })();
    drop(cursor);
    assert_eq!(
        resources.used(),
        held_bytes,
        "success/error leaves only the held shared lease: {text}"
    );
    drop(held);
    assert_eq!(
        resources.used(),
        0,
        "success/error releases the bounded fallback: {text}"
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

fn fixture(count: usize) -> (Connection, Vec<Value>) {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT NOT NULL,\
        database_id TEXT NOT NULL,storage_format INTEGER NOT NULL)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO main._lithograph_meta VALUES(1,'lithograph-format-v1',\
        '00000000-0000-4000-8000-000000000027',?1)",
        [storage::STORAGE_FORMAT],
    )
    .unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    let mut rows = write(
        &db,
        &format!(
            "UNWIND range(0,{}) AS seq \
        CREATE(n:Probe:Visible {{seq:seq}}) RETURN elementId(n),n.seq",
            count - 1
        ),
    )
    .rows;
    rows.sort_by_key(|row| match row[1] {
        Value::Integer(seq) => seq,
        _ => panic!("seed sequence"),
    });
    assert_eq!(rows.len(), count);
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
        "MATCH(n:Probe) WHERE n.seq IN [1,3,6] SET n:Chosen FINISH",
    );
    write(&db, "MATCH(n:Probe) WHERE n.seq=1 SET n:Only FINISH");
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    (db, ids)
}

fn expected(ids: &[Value], members: &[usize], delta: i64) -> Vec<Vec<Value>> {
    members
        .iter()
        .map(|&seq| vec![ids[seq].clone(), Value::Integer(seq as i64 + delta)])
        .collect()
}

fn bag(rows: &[Vec<Value>]) -> Vec<String> {
    let mut rows = rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect::<Vec<_>>();
    rows.sort();
    rows
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

fn node_access<'a>(result: &'a Observed, variable: &str) -> &'a OperatorRuntimeMetrics {
    let slots = result
        .physical
        .operators
        .iter()
        .enumerate()
        .filter_map(|(slot, operator)| match operator {
            PhysicalOperator::NodeScan { variable: name }
            | PhysicalOperator::LabelIndexScan { variable: name, .. }
                if name == variable =>
            {
                Some(slot)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        slots.len(),
        1,
        "one fresh node access: {:?}",
        result.physical
    );
    let access = &result.operators[slots[0]];
    assert_eq!(access.id, slots[0] as u64);
    access
}

fn assert_fixed_three(actual: &Observed, ids: &[Value]) {
    assert_fixed_three_pattern(actual, ids, 128);
}

fn assert_fixed_three_pattern(actual: &Observed, ids: &[Value], pattern_rows: u64) {
    assert_eq!(bag(&actual.rows), bag(&expected(ids, &MEMBERS, 0)));
    assert_eq!(actual.summary_rows, 3);
    let access = node_access(actual, "a");
    assert_eq!(
        access.rows, pattern_rows,
        "all fresh pattern matches precede WHERE: {actual:?}"
    );
    assert!(access.db_hits > 0);
    let project = actual
        .operators
        .iter()
        .rev()
        .find(|op| op.operator == "Project")
        .unwrap();
    assert_eq!(project.rows, 3);
}

#[test]
fn complete_fixed_three_bag_keeps_full_pattern_rows_before_where() {
    let (db, ids) = fixture(128);
    let before = state(&db);
    let actual = read(&db, FALLBACK, ExecutionOptions::default()).unwrap();
    assert_fixed_three(&actual, &ids);
    assert_eq!(state(&db), before);

    // COLLECT blocks costed S4 planning for the whole program. A real incoming
    // c point, rather than an assumed Cartesian plan, installs explicit sites.
    let text = "MATCH(b:Chosen) WITH collect(b) AS bs MATCH(c:Only) WITH bs,c \
        MATCH(c) WITH bs MATCH(a:Probe) WHERE (a IN bs) OR false RETURN elementId(a),a.seq";
    let scoped = read(&db, text, ExecutionOptions::default()).unwrap();
    assert_fixed_three(&scoped, &ids);
    let points = scoped
        .physical
        .operators
        .iter()
        .enumerate()
        .filter_map(|(slot, operator)| {
            matches!(operator, PhysicalOperator::NodeIdSeek { variable } if variable == "c")
                .then_some(slot)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        points.len(),
        1,
        "actual bound c site: {:?}",
        scoped.physical
    );
    let point = &scoped.operators[points[0]];
    assert_eq!(point.id, points[0] as u64);
    assert_eq!(point.operator, "NodeIdSeek");
    assert_eq!(point.rows, 1);
    assert!(point.db_hits > 0);
    let access = node_access(&scoped, "a");
    let filters = scoped
        .physical
        .operators
        .iter()
        .enumerate()
        .filter_map(|(slot, operator)| {
            (slot as u64 > access.id && matches!(operator, PhysicalOperator::Filter))
                .then_some(slot)
        })
        .collect::<Vec<_>>();
    // Only the final a clause follows its producer; earlier Filters cannot make
    // this assertion pass or absorb the pattern-only cursor's 128 rows.
    assert_eq!(
        filters.len(),
        1,
        "one final a residual: {:?}",
        scoped.physical
    );
    let residual = &scoped.operators[filters[0]];
    assert_eq!(residual.id, filters[0] as u64);
    assert_eq!(residual.operator, "Filter");
    assert_eq!(
        residual.rows, 3,
        "pattern-only rows cannot prefill this Filter: {residual:?}"
    );
    assert_eq!(state(&db), before);
}

#[test]
fn null_and_mixed_lists_keep_original_short_circuit_order() {
    let (db, ids) = fixture(8);
    for (rhs, members) in [
        ("[]", vec![]),
        ("null", vec![]),
        ("[null]", vec![]),
        ("bs+[null]", vec![1]),
        ("bs+[1]", vec![1]),
    ] {
        let text = format!(
            "MATCH(b:Only) WITH collect(b) AS bs WITH {rhs} AS members \
            MATCH(a:Only) WHERE (a IN members) OR false RETURN elementId(a),a.seq"
        );
        let actual = read(&db, &text, ExecutionOptions::default()).unwrap();
        assert_eq!(
            bag(&actual.rows),
            bag(&expected(&ids, &members, 0)),
            "{rhs}"
        );
    }
    let before = state(&db);
    for rhs in ["[1]+bs", "[null,1]", "1"] {
        let text = format!(
            "MATCH(b:Only) WITH collect(b) AS bs WITH {rhs} AS members \
            MATCH(a:Only) WHERE (a IN members) OR false RETURN elementId(a),a.seq"
        );
        let error = read(&db, &text, ExecutionOptions::default()).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Type, "{rhs}: {error:?}");
        assert_eq!(state(&db), before);
    }
}

#[test]
fn zero_pattern_candidates_and_zero_outer_do_not_evaluate_rhs() {
    let (db, _) = fixture(8);
    let before = state(&db);
    for text in [
        "WITH 1 AS step MATCH(a:NeverDefined) WHERE (a IN range(1,2,step-1)) OR false RETURN elementId(a),a.seq",
        "WITH 1 AS members MATCH(a:Probe {seq:-1}) WHERE (a IN members) OR false RETURN elementId(a),a.seq",
        "MATCH(b:Chosen) WITH collect(b) AS bs UNWIND [] AS seed WITH 1 AS members \
         MATCH(a:Probe) WHERE (a IN members) OR false RETURN elementId(a),a.seq",
        "WITH 1 AS members MATCH(a:Probe {seq:null}) WHERE (a IN members) OR false RETURN elementId(a),a.seq",
    ] {
        assert!(
            read(&db, text, ExecutionOptions::default())
                .unwrap()
                .rows
                .is_empty(),
            "{text}"
        );
        assert_eq!(state(&db), before);
    }
}

#[test]
fn unsupported_inline_pattern_errors_still_precede_where_errors() {
    let (db, _) = fixture(8);
    let before = state(&db);
    // Inline properties remain on the old matcher. This protects that rejection
    // boundary; the plain-node success case above proves the new bounded route.
    for predicate in ["false", "(size(range(1,2,0))=0) OR false"] {
        let text = format!(
            "MATCH(b:Chosen) WITH collect(b) AS bs \
            MATCH(a:Probe {{seq:CASE WHEN a.seq=7 THEN 1/0 ELSE a.seq END}}) \
            WHERE {predicate} RETURN elementId(a),a.seq"
        );
        let error = read(&db, &text, ExecutionOptions::default()).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Type);
        assert_eq!(
            error.message, "division by zero",
            "pattern must finish before WHERE: {error:?}"
        );
        assert_eq!(state(&db), before);
    }
}

#[test]
fn graph_view_and_query_access_still_filter_both_materialized_phases() {
    let (db, ids) = fixture(8);
    write(
        &db,
        "MATCH(n:Probe) WHERE n.seq=3 REMOVE n:Visible SET n:Hidden FINISH",
    );
    let before = state(&db);
    let visible =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#).unwrap();
    let actual = read(&db, FALLBACK, visible).unwrap();
    assert_eq!(bag(&actual.rows), bag(&expected(&ids, &[1, 6], 0)));
    let masked =
        ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["seq"]}}}"#)
            .unwrap();
    let actual = read(&db, FALLBACK, masked).unwrap();
    let expected = MEMBERS
        .iter()
        .map(|&seq| vec![ids[seq].clone(), Value::Null])
        .collect::<Vec<_>>();
    assert_eq!(bag(&actual.rows), bag(&expected));
    assert_eq!(state(&db), before);
}

#[test]
fn historical_fallback_uses_its_own_membership_and_values() {
    let (db, ids) = fixture(8);
    let before = state(&db);
    write(
        &db,
        "MATCH(n:Chosen) SET n.seq=n.seq+1000 REMOVE n:Chosen FINISH",
    );
    let current = state(&db);
    let mut historical = ExecutionOptions::default();
    historical.snapshot = SnapshotSelector::Commit(before.0.to_hex());
    let actual = read(&db, FALLBACK, historical).unwrap();
    assert_eq!(bag(&actual.rows), bag(&expected(&ids, &MEMBERS, 0)));
    assert!(
        read(&db, FALLBACK, ExecutionOptions::default())
            .unwrap()
            .rows
            .is_empty()
    );
    assert_eq!(state(&db), current);
}

fn staged(action: &str, delta: i64, empty: bool) {
    let (db, ids) = fixture(8);
    let before = state(&db);
    let text = format!(
        "MATCH(b:Chosen) WITH collect(b) AS bs UNWIND bs AS member \
        {action} WITH DISTINCT bs MATCH(a:Probe) WHERE (a IN bs) OR false RETURN elementId(a),a.seq"
    );
    let actual = write(&db, &format!("PROFILE {text}"));
    let expected = if empty {
        vec![]
    } else {
        expected(&ids, &MEMBERS, delta)
    };
    assert_eq!(bag(&actual.rows), bag(&expected));
    assert_eq!(state(&db).1, before.1 + 1);
    assert_ne!(state(&db).0, before.0);
    match action {
        "SET member.seq=member.seq+1000" => assert_eq!(actual.counters.properties_set, 3),
        "REMOVE member:Probe" => assert_eq!(actual.counters.labels_removed, 3),
        "DELETE member" => assert_eq!(actual.counters.nodes_deleted, 3),
        _ => panic!("unknown staged fixture action"),
    }
}

#[test]
fn fallback_rechecks_staged_values_labels_and_deletions() {
    staged("SET member.seq=member.seq+1000", 1000, false);
    staged("REMOVE member:Probe", 0, true);
    staged("DELETE member", 0, true);
}

#[test]
fn staged_new_dictionary_is_fresh_and_where_failure_rolls_back() {
    let (db, ids) = fixture(8);
    let before = state(&db);
    let text = "CREATE(n:FreshChosen:FreshProbe {seq:999}) WITH 1 AS seed \
        MATCH(b:FreshChosen) WITH collect(b) AS bs MATCH(a:FreshProbe) \
        WHERE (a IN bs) OR false RETURN elementId(a),a.seq";
    let actual = write(&db, &format!("PROFILE {text}"));
    assert_eq!(actual.rows.len(), 1);
    assert_eq!(actual.rows[0][1], Value::Integer(999));
    assert_eq!(actual.counters.nodes_created, 1);
    let direct = read(
        &db,
        "MATCH(a:FreshProbe) RETURN elementId(a),a.seq",
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_eq!(bag(&actual.rows), bag(&direct.rows));
    assert_eq!(state(&db).1, before.1 + 1);
    let before = state(&db);
    let failing = "MATCH(b:Chosen) WITH collect(b) AS bs UNWIND bs AS member \
        SET member.seq=member.seq+1000 WITH DISTINCT bs MATCH(a:Probe) \
        WHERE (a IN ([1]+bs)) OR false RETURN elementId(a),a.seq";
    let error = execute(
        &db,
        &format!("PROFILE {failing}"),
        ExecutionOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    assert_eq!(state(&db), before);
    let recovered = read(&db, FALLBACK, ExecutionOptions::default()).unwrap();
    assert_eq!(bag(&recovered.rows), bag(&expected(&ids, &MEMBERS, 0)));
}

fn temp_catalog(db: &Connection) -> Vec<(String, String)> {
    db.prepare("SELECT name,coalesce(sql,'') FROM sqlite_temp_master ORDER BY name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn mid_work_interruption_releases_fallback_and_the_same_handle_recovers() {
    let (db, ids) = fixture(128);
    let before = state(&db);
    let temp_before = temp_catalog(&db);
    assert!(db.is_autocommit());
    {
        let scope = ReadScope::new(&db);
        let prepared = prepare(
            &db,
            &format!("PROFILE {FALLBACK}"),
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .unwrap();
        let resources = prepared.resources.clone();
        let baseline = resources.used();
        let mut cursor = QueryCursor::new(prepared);
        let checks = Cell::new(0_u64);
        let interrupted_used = Cell::new(0_u64);
        let error = cursor
            .next_batch_with_interrupt(&db, 2, &|| {
                checks.set(checks.get() + 1);
                let used = resources.used();
                // Preparation is already complete. Let actual cursor work and
                // allocations proceed before refusing an execution checkpoint.
                let cancel = checks.get() >= 64 && used > baseline;
                if cancel {
                    interrupted_used.set(used);
                }
                cancel
            })
            .expect_err("cancel after real fallback work, before the first public batch");
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
        assert!(
            checks.get() >= 64,
            "the callback must not cancel immediately"
        );
        assert!(
            interrupted_used.get() > baseline,
            "cancellation observed live execution allocations"
        );
        drop(cursor);
        assert_eq!(
            resources.used(),
            0,
            "interrupted fallback releases its leases"
        );
        drop(scope);
    }
    assert!(db.is_autocommit());
    assert_eq!(temp_catalog(&db), temp_before);
    assert_eq!(state(&db), before);

    let recovered = read(&db, FALLBACK, ExecutionOptions::default()).unwrap();
    assert_fixed_three(&recovered, &ids);
    assert!(db.is_autocommit());
    assert_eq!(temp_catalog(&db), temp_before);
    assert_eq!(state(&db), before);
}

#[test]
fn hard_512k_keeps_the_fixed_three_bag_across_small_pattern_domains() {
    for count in [128, 1000] {
        // Seeding and checkpoint construction use the fixture's default budget;
        // only the application-facing fallback execution receives this limit.
        let (db, ids) = fixture(count);
        let before = state(&db);
        let mut options = ExecutionOptions::default();
        options.memory_limit_bytes = Some(524_288);
        let actual = read(&db, FALLBACK, options)
            .unwrap_or_else(|error| panic!("hard512 fallback domain {count}: {error:?}"));
        assert_fixed_three_pattern(&actual, &ids, count as u64);
        assert_eq!(state(&db), before);
    }
}

#[test]
fn hard_512k_keeps_two_property_members_in_the_fallback_spool() {
    let (db, ids) = fixture(128);
    write(&db, "MATCH(n:Probe) SET n.k=n.seq FINISH");
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let before = state(&db);
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = Some(524_288);
    let actual = read(&db, FALLBACK, options)
        .unwrap_or_else(|error| panic!("hard512 fallback with two-property members: {error:?}"));
    assert_fixed_three(&actual, &ids);
    assert_eq!(state(&db), before);

    // This models observed live-prefix pressure on the same query ledger. It
    // does not claim to clone the exact frozen million-node execution state.
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = Some(524_288);
    let pressured = {
        let _scope = ReadScope::new(&db);
        execute_with_shared_lease(&db, &format!("PROFILE {FALLBACK}"), options, 150_856)
            .unwrap_or_else(|error| panic!("hard512 fallback with shared pressure: {error:?}"))
    };
    assert_fixed_three(&pressured, &ids);
    assert_eq!(state(&db), before);
}
