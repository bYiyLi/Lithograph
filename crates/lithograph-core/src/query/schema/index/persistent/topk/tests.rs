use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

use super::*;
use crate::query::schema::StandardIndexPredicate;
use crate::query::{ExecutionOptions, QueryCursor, QueryResources, prepare};

fn execute(db: &Connection, text: &str) {
    let prepared = prepare(db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let mut cursor = QueryCursor::new(prepared);
    while !cursor.next_batch(db, 17).unwrap().done {}
    cursor.complete(db).unwrap();
}

fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER);\
         INSERT INTO _lithograph_meta VALUES(1,'lithograph-format-v1',\
         '00000000-0000-4000-8000-000000000033',3);",
    ).unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    for text in [
        "CREATE (:Metric {rank:9}), (:Metric {rank:1}), (:Metric {rank:5}) FINISH",
        "CREATE CONSTRAINT metric_rank_type FOR(n:Metric) REQUIRE n.rank IS :: INTEGER",
        "CREATE RANGE INDEX metric_rank FOR(n:Metric) ON(n.rank)",
    ] {
        execute(&db, text);
    }
    db
}

fn snapshot(db: &Connection) -> Snapshot<'_> {
    let prepared = prepare(
        db,
        "MATCH(n:Metric) WHERE n.rank >= 0 AND n.rank < 100 RETURN n.rank",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    Snapshot::from_resolved_state(db, prepared.resolved_state.as_ref().unwrap())
}

fn seek() -> StandardIndexSeek {
    StandardIndexSeek {
        index_name: "metric_rank".to_owned(),
        kind: StandardIndexKind::Range,
        predicates: vec![(
            0,
            StandardIndexPredicate::Bounds {
                lower: Some((Value::Integer(0), true)),
                upper: Some((Value::Integer(100), false)),
            },
        )],
    }
}

fn bind(snapshot: &Snapshot<'_>) -> QueryResult<Option<OrderedNumericIndexRead>> {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let result =
        try_bind_topk_numeric_index(snapshot, &seek(), &control, &mut PlanningBudget::default());
    assert_eq!(resources.used(), 0, "advice owns no retained workspace");
    result
}

#[test]
fn temp_metadata_insert_deny_and_ignore_never_issue_ordering_proof() {
    for ignore in [false, true] {
        let db = fixture();
        let snapshot = snapshot(&db);
        assert!(bind(&snapshot).unwrap().is_some());
        db.execute_batch("DELETE FROM temp._lithograph_standard_index_cache_meta")
            .unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&hits);
        db.authorizer(Some(move |context: AuthContext<'_>| {
            if context.database_name == Some("temp")
                && matches!(
                    context.action,
                    AuthAction::Insert {
                        table_name: "_lithograph_standard_index_cache_meta"
                    }
                )
            {
                observed.fetch_add(1, Ordering::Relaxed);
                if ignore {
                    Authorization::Ignore
                } else {
                    Authorization::Deny
                }
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
        assert!(bind(&snapshot).unwrap().is_none(), "ignore={ignore}");
        assert!(
            hits.load(Ordering::Relaxed) > 0,
            "actual TEMP INSERT authorization"
        );
        db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        let bindings: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM temp._lithograph_standard_index_cache_meta",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bindings, 0);
        assert!(
            bind(&snapshot).unwrap().is_some(),
            "normal ready binding remains usable"
        );
    }
}

#[test]
fn another_prefix_local_rows_refuse_before_building_shared_range_index() {
    let db = fixture();
    let snapshot = snapshot(&db);
    assert!(bind(&snapshot).unwrap().is_some());
    db.execute_batch(
        "DROP INDEX temp._lithograph_standard_index_cache_range_number;\
         DELETE FROM temp._lithograph_standard_index_cache_meta;\
         INSERT INTO temp._lithograph_standard_index_cache_local\
         (snapshot_hash,index_name,owner_kind,owner_id,property_ordinal)\
         VALUES(zeroblob(32),'unrelated_text_prefix',1,777,0);",
    )
    .unwrap();
    assert!(empty_overlay(&snapshot, "metric_rank").unwrap());
    assert!(bind(&snapshot).unwrap().is_none());
    let rebuilt: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM temp.sqlite_schema \
         WHERE type='index' AND name='_lithograph_standard_index_cache_range_number')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !rebuilt,
        "TopK advice must not index another prefix's materialization"
    );
    let rows: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM temp._lithograph_standard_index_cache_local \
         WHERE index_name='unrelated_text_prefix' AND owner_id=777",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1, "existing local materialization is not cleared");
}

#[test]
fn actual_temp_page_cap_returns_unavailable_advice_without_leaked_rows() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("PRAGMA temp_store=FILE; CREATE TEMP TABLE capped(payload BLOB)")
        .unwrap();
    let pages: i64 = db
        .pragma_query_value(Some("temp"), "page_count", |row| row.get(0))
        .unwrap();
    db.pragma_update(Some("temp"), "max_page_count", pages)
        .unwrap();
    let insert = "INSERT INTO temp.capped VALUES(zeroblob(65536))";
    let error = db.execute(insert, []).expect_err("actual TEMP pager cap");
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DiskFull)
    );
    assert!(optional_temp::<usize>(Err(error)).unwrap().is_none());
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    assert!(!optional_temp_ddl(&db, insert, &control, &mut PlanningBudget::default()).unwrap());
    let rows: i64 = db
        .query_row("SELECT COUNT(*) FROM temp.capped", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0);
    assert_eq!(resources.used(), 0);
}

fn sqlite_failure(code: i32) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(code),
        Some("injected TEMP error".into()),
    )
}

#[test]
fn temp_capacity_error_classification_keeps_real_read_failures_and_cancellation() {
    // NOMEM is classification-only: no unsafe allocator hook or real heap OOM.
    for code in [rusqlite::ffi::SQLITE_NOMEM, rusqlite::ffi::SQLITE_TOOBIG] {
        assert!(
            optional_temp::<()>(Err(sqlite_failure(code)))
                .unwrap()
                .is_none()
        );
    }
    for code in [
        rusqlite::ffi::SQLITE_IOERR,
        rusqlite::ffi::SQLITE_CORRUPT,
        rusqlite::ffi::SQLITE_INTERRUPT,
        rusqlite::ffi::SQLITE_BUSY,
    ] {
        let expected = QueryError::from(sqlite_failure(code));
        let actual = optional_temp::<()>(Err(sqlite_failure(code))).unwrap_err();
        assert_eq!(actual.kind, expected.kind);
        assert_eq!(actual.sqlite_code, Some(code));
        assert_eq!(actual.message, expected.message);
    }
}

#[test]
fn main_generation_read_denial_propagates_instead_of_optional_temp_fallback() {
    let db = fixture();
    let snapshot = snapshot(&db);
    assert!(bind(&snapshot).unwrap().is_some());
    db.authorizer(Some(|context: AuthContext<'_>| {
        if context.database_name == Some("main")
            && matches!(
                context.action,
                AuthAction::Read {
                    table_name: "_lithograph_index_generations",
                    ..
                }
            )
        {
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }))
    .unwrap();
    let error = bind(&snapshot).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_AUTH));
    db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
}
