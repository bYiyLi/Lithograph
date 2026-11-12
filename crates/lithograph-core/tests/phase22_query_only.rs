use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, QueryResources, QuerySummary,
    SnapshotSelector, prepare, prepare_with_resources_and_interrupt,
};
use lithograph_core::storage;
use rusqlite::{Connection, OpenFlags};

fn execute(
    db: &Connection,
    text: &str,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), QueryError> {
    let prepared = prepare(db, text, BTreeMap::new(), options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(db, 3)?;
        rows.extend(batch.rows);
        if batch.done {
            return Ok((rows, cursor.complete(db)?));
        }
    }
}

fn run(db: &Connection, text: &str) -> Vec<Vec<Value>> {
    execute(db, text, ExecutionOptions::default())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .0
}

fn fixture(path: &std::path::Path, generation: bool) -> storage::HashId {
    let db = Connection::open(path).unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY, magic TEXT, database_id TEXT, storage_format INTEGER);").unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    run(
        &db,
        "UNWIND range(0,15) AS i CREATE (:Item {v:i,g:i})-[:LINK {v:i,g:i}]->(:Endpoint)",
    );
    assert_eq!(
        run(&db, "MATCH(n:Item) RETURN count(n)"),
        vec![vec![Value::Integer(16)]]
    );
    for ddl in [
        "CREATE CONSTRAINT nv FOR(n:Item) REQUIRE n.v IS :: INTEGER",
        "CREATE CONSTRAINT ng FOR(n:Item) REQUIRE n.g IS :: INTEGER",
        "CREATE CONSTRAINT rv FOR()-[r:LINK]-() REQUIRE r.v IS :: INTEGER",
        "CREATE CONSTRAINT rg FOR()-[r:LINK]-() REQUIRE r.g IS :: INTEGER",
        "CREATE RANGE INDEX node_values FOR(n:Item) ON(n.v)",
        "CREATE RANGE INDEX rel_values FOR()-[r:LINK]-() ON(r.v)",
        "CREATE LOOKUP INDEX rel_types FOR()-[r]-() ON EACH type(r)",
    ] {
        run(&db, ddl);
    }
    let old = storage::branch_head(&db, "main").unwrap();
    run(
        &db,
        "CREATE (:Item {v:99,g:99})-[:LINK {v:99,g:99}]->(:Endpoint)",
    );
    if !generation {
        db.execute_batch(
            "DELETE FROM _lithograph_index_entries; DELETE FROM _lithograph_index_generations;",
        )
        .unwrap();
    }
    old
}

#[test]
fn query_only_reopens_use_truthful_canonical_access_for_nodes_and_relationships() {
    for generation in [true, false] {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("query-only.sqlite");
        let old = fixture(&path, generation);
        let before = std::fs::read(&path).unwrap();
        for readonly in [false, true] {
            let flags = if readonly {
                OpenFlags::SQLITE_OPEN_READ_ONLY
            } else {
                OpenFlags::SQLITE_OPEN_READ_WRITE
            };
            let db = Connection::open_with_flags(&path, flags).unwrap();
            db.pragma_update(None, "query_only", true).unwrap();
            let head = storage::branch_head(&db, "main").unwrap();
            for (pattern, variable) in [("(n:Item)", "n"), ("()-[r:LINK]->()", "r")] {
                for (predicate, expected) in [
                    (format!("{variable}.v=1000 AND {variable}.g>=0"), vec![]),
                    (
                        format!("{variable}.v=5 AND {variable}.g>=0"),
                        vec![vec![Value::Integer(5)]],
                    ),
                    (
                        format!("{variable}.v IN[1,1,3] AND {variable}.g>=0"),
                        vec![vec![Value::Integer(1)], vec![Value::Integer(3)]],
                    ),
                ] {
                    let text = format!(
                        "MATCH {pattern} WHERE {predicate} RETURN {variable}.v ORDER BY {variable}.v"
                    );
                    let explained = run(&db, &format!("EXPLAIN {text}"));
                    assert!(
                        matches!(&explained[0][0], Value::String(plan) if !plan.contains("IndexSeek")),
                        "{explained:?}"
                    );
                    assert_eq!(run(&db, &text), expected);
                    let (rows, summary) =
                        execute(&db, &format!("PROFILE {text}"), ExecutionOptions::default())
                            .unwrap();
                    assert_eq!(rows, expected);
                    assert!(
                        !summary
                            .metrics
                            .operator_profile()
                            .unwrap()
                            .iter()
                            .any(|metric| metric.operator == "IndexSeek")
                    );
                }
            }
            let mut historical = ExecutionOptions::default();
            historical.snapshot = SnapshotSelector::Commit(old.to_hex());
            assert!(
                execute(
                    &db,
                    "MATCH(n:Item) WHERE n.v=99 RETURN n.v",
                    historical.clone()
                )
                .unwrap()
                .0
                .is_empty()
            );
            assert!(
                execute(
                    &db,
                    "MATCH()-[r:LINK]->() WHERE r.v=99 RETURN r.v",
                    historical
                )
                .unwrap()
                .0
                .is_empty()
            );
            assert_eq!(
                run(&db, "MATCH()-[r:LINK]->() RETURN count(r)"),
                vec![vec![Value::Integer(17)]]
            );
            assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn query_only_keeps_direct_identity_and_real_cancellation() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("identity.sqlite");
    fixture(&path, true);
    let db = Connection::open(&path).unwrap();
    db.pragma_update(None, "query_only", true).unwrap();
    for (pattern, variable, id, operator) in [
        ("(n:Item)", "n", "n:11", "NodeIdSeek"),
        ("()-[r:LINK]->()", "r", "r:6", "RelationshipIdSeek"),
    ] {
        let text =
            format!("MATCH {pattern} WHERE elementId({variable})='{id}' RETURN {variable}.v");
        let plan = run(&db, &format!("EXPLAIN {text}"));
        assert!(matches!(&plan[0][0], Value::String(plan) if plan.contains(operator)));
        assert_eq!(run(&db, &text), vec![vec![Value::Integer(5)]]);
    }
    let resources = QueryResources::default();
    let result = prepare_with_resources_and_interrupt(
        &db,
        "MATCH(n:Item) WHERE n.v=5 RETURN n.v",
        BTreeMap::new(),
        ExecutionOptions::default(),
        resources.clone(),
        &|| true,
    );
    assert_eq!(result.unwrap_err().kind, QueryErrorKind::Interrupted);
    assert_eq!(resources.used(), 0);
}
