use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, PhysicalOperator, PreparedQuery, QueryCursor, QueryError, QueryErrorKind,
    prepare,
};
use lithograph_core::storage::{
    STORAGE_FORMAT, branch_head, create_storage_schema, initialize_connection_state,
    initialize_root,
};
use rusqlite::Connection;

const MIB: u64 = 1024 * 1024;
const REBUILD: &str = "CALL lithograph.index.rebuild('ix','branch/main') YIELD indexedEntities RETURN indexedEntities";
const SEEK: &str = "MATCH (n:Indexed) WHERE n.payload >= 'x' RETURN 1";

fn database() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    connection.execute("INSERT INTO _lithograph_meta VALUES(1,'lithograph-format-v1','00000000-0000-4000-8000-000000000019',?1)", [STORAGE_FORMAT]).unwrap();
    create_storage_schema(&connection).unwrap();
    initialize_root(&connection).unwrap();
    initialize_connection_state(&connection).unwrap();
    connection
}

fn drain(connection: &Connection, prepared: PreparedQuery) -> Result<Vec<Vec<Value>>, QueryError> {
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 17)?;
        rows.extend(batch.rows.iter().cloned());
        let done = batch.done;
        drop(batch);
        if done {
            drop(cursor.complete(connection)?);
            return Ok(rows);
        }
    }
}

fn run(
    connection: &Connection,
    query: &str,
    limit: Option<u64>,
) -> Result<Vec<Vec<Value>>, QueryError> {
    let options = limit.map_or_else(ExecutionOptions::default, |bytes| {
        ExecutionOptions::parse(&serde_json::json!({"memoryLimitBytes":bytes})).unwrap()
    });
    let prepared = prepare(connection, query, BTreeMap::new(), options)?;
    assert_index_seek(query, &prepared);
    drain(connection, prepared)
}

fn assert_index_seek(query: &str, prepared: &PreparedQuery) {
    if query == SEEK {
        assert!(prepared.physical.operators.iter().any(|operator| matches!(operator,
            PhysicalOperator::IndexSeek { index, kind: lithograph_core::storage::StandardIndexKind::Range, .. } if index == "ix"
        )), "cold fixture must execute ix, actual plan: {:?}", prepared.physical);
    }
}

fn seed(connection: &Connection, count: usize, bytes: usize) {
    let prepared = prepare(
        connection,
        "UNWIND range(1,$count) AS i CREATE (:Indexed {payload:$payload}) FINISH",
        BTreeMap::from([
            ("count".to_owned(), Value::Integer(count as i64)),
            ("payload".to_owned(), Value::String("x".repeat(bytes))),
        ]),
        ExecutionOptions::default(),
    )
    .unwrap();
    drain(connection, prepared).unwrap();
    run(
        connection,
        "CREATE CONSTRAINT payload_type FOR (n:Indexed) REQUIRE n.payload IS :: STRING",
        None,
    )
    .unwrap();
    run(
        connection,
        "CREATE RANGE INDEX ix FOR (n:Indexed) ON (n.payload)",
        None,
    )
    .unwrap();
}

fn clear_derived(connection: &Connection) {
    connection.execute_batch("DELETE FROM main._lithograph_index_entries; DELETE FROM main._lithograph_index_generations; DELETE FROM temp._lithograph_standard_index_cache_meta; DELETE FROM temp._lithograph_standard_index_cache_local; DELETE FROM temp._lithograph_standard_index_changed_owners;").unwrap();
}

fn ready_generations(connection: &Connection) -> i64 {
    connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_index_generations WHERE complete=1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn maintenance_and_cold_seek_reject_large_encoding_before_publication() {
    let connection = database();
    seed(&connection, 1, 4 * MIB as usize);
    let head = branch_head(&connection, "main").unwrap();
    let generations = ready_generations(&connection);
    let error = run(&connection, REBUILD, Some(16 * MIB)).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert_eq!(ready_generations(&connection), generations);
    assert_eq!(branch_head(&connection, "main").unwrap(), head);
    assert_eq!(
        run(&connection, REBUILD, Some(256 * MIB)).unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    clear_derived(&connection);
    let error = run(&connection, SEEK, Some(16 * MIB)).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(ready_generations(&connection), 0);
    let ready_local: i64 = connection
        .query_row(
            "SELECT count(*) FROM temp._lithograph_standard_index_cache_meta WHERE complete=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        ready_local, 0,
        "failed build cannot publish a partial cache"
    );
    assert_eq!(
        run(&connection, SEEK, Some(256 * MIB)).unwrap(),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        ready_generations(&connection),
        0,
        "read fallback only writes TEMP"
    );
    assert_eq!(branch_head(&connection, "main").unwrap(), head);
}

#[test]
fn maintenance_and_cold_seek_cancel_during_value_population_and_retry() {
    for query in [REBUILD, SEEK] {
        let connection = database();
        seed(&connection, 129, 64);
        clear_derived(&connection);
        let head = branch_head(&connection, "main").unwrap();
        let prepared = prepare(
            &connection,
            query,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .unwrap();
        assert_index_seek(query, &prepared);
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        let observed = std::cell::Cell::new(0_i64);
        let error = cursor
            .next_batch_with_interrupt(&connection, 1, &|| {
                let count: i64 = connection
                    .query_row(
                        "SELECT count(*) FROM temp._lithograph_standard_index_cache_local",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                observed.set(observed.get().max(count));
                count >= 8
            })
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
        assert!(
            (8..=64).contains(&observed.get()),
            "cancellation must interrupt population"
        );
        cursor.cancel(&connection).unwrap();
        drop(cursor);
        assert_eq!(resources.used(), 0);
        assert_eq!(ready_generations(&connection), 0);
        assert_eq!(branch_head(&connection, "main").unwrap(), head);
        assert_eq!(
            run(&connection, query, Some(256 * MIB)).unwrap(),
            if query == SEEK {
                vec![vec![Value::Integer(1)]; 129]
            } else {
                vec![vec![Value::Integer(129)]]
            }
        );
    }
}
