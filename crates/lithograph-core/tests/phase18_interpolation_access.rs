use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, prepare};
use lithograph_core::storage::{create_storage_schema, initialize_root};
use rusqlite::Connection;

fn storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn execute(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: &str,
    batch_size: usize,
) -> Result<Vec<Vec<Value>>, QueryError> {
    let prepared = prepare(
        connection,
        query,
        params,
        ExecutionOptions::parse_text(options)?,
    )?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, batch_size)?;
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection)?;
            return Ok(rows);
        }
    }
}

fn text(value: &str) -> Vec<Vec<Value>> {
    vec![vec![Value::String(value.to_owned())]]
}

#[test]
fn interpolated_expression_subqueries_execute_with_default_and_explicit_access() {
    let connection = storage();
    for options in [
        "{}",
        r#"{"readAccess":{"allowExternalIO":true}}"#,
        r#"{"readAccess":{"allowExternalIO":false}}"#,
    ] {
        for query in [
            "RETURN s\"{COUNT { RETURN 1 }}\"",
            "RETURN s\"{size(COLLECT { RETURN 1 })}\"",
            "RETURN s\"{CASE WHEN EXISTS { RETURN 1 } THEN 1 ELSE 0 END}\"",
        ] {
            assert_eq!(
                execute(&connection, query, BTreeMap::new(), options, 1).expect(query),
                text("1")
            );
        }
        assert_eq!(
            execute(
                &connection,
                "RETURN s\"{EXISTS { RETURN 1 }}\"",
                BTreeMap::new(),
                options,
                1
            )
            .expect("EXISTS interpolation"),
            text("true")
        );
    }
}

#[test]
fn interpolated_graph_subqueries_keep_query_visibility_across_rows_and_batches() {
    let connection = storage();
    execute(
        &connection,
        "UNWIND range(1,270) AS id CREATE (:Person {id:id, secret:1}) FINISH",
        BTreeMap::new(),
        "{}",
        16,
    )
    .expect("graph fixture");
    let query = "MATCH (n:Person) RETURN n.id, s\"{COUNT { WITH n WHERE n.secret IS NOT NULL RETURN n }}\" AS count";
    for batch_size in [1, 17, 128] {
        for (options, expected) in [
            ("{}", "1"),
            (
                r#"{"readAccess":{"result":{"denyProperties":["secret"]}}}"#,
                "1",
            ),
            (
                r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#,
                "0",
            ),
        ] {
            let rows = execute(&connection, query, BTreeMap::new(), options, batch_size)
                .expect("correlated interpolation");
            assert_eq!(rows.len(), 270);
            assert!(
                rows.iter()
                    .all(|row| row[1] == Value::String(expected.to_owned()))
            );
        }
    }
}

#[test]
fn interpolated_aggregates_keep_the_global_group_and_lazy_graph_branches() {
    let connection = storage();
    for query in [
        "UNWIND [1,2,3] AS n RETURN s\"{sum(n)}\"",
        "UNWIND [1,2,3] AS n RETURN s\"{CASE WHEN EXISTS { RETURN 1 } THEN sum(n) ELSE 0 END}\"",
        "UNWIND [1,2,3] AS n RETURN CASE WHEN sum(n) = 6 THEN s\"{COUNT { RETURN 1 }}\" ELSE 'failed' END",
    ] {
        let expected = if query.ends_with("END") { "1" } else { "6" };
        assert_eq!(
            execute(&connection, query, BTreeMap::new(), "{}", 1).expect(query),
            text(expected)
        );
    }
}

#[test]
fn lazy_graph_booleans_preserve_three_valued_logic_and_type_errors() {
    let connection = storage();
    for (query, expected) in [
        ("RETURN s\"{null AND EXISTS { RETURN 1 }}\"", "null"),
        ("RETURN s\"{null OR EXISTS { RETURN 1 }}\"", "true"),
        (
            "RETURN s\"{null AND EXISTS { MATCH (n) RETURN n }}\"",
            "false",
        ),
        (
            "RETURN s\"{null OR EXISTS { MATCH (n) RETURN n }}\"",
            "null",
        ),
        (
            "RETURN CASE null WHEN null THEN s\"{COUNT { RETURN 1 }}\" ELSE 'fallback' END",
            "fallback",
        ),
    ] {
        for prefix in ["", "PROFILE "] {
            let query = format!("{prefix}{query}");
            assert_eq!(
                execute(&connection, &query, BTreeMap::new(), "{}", 1).expect(&query),
                text(expected)
            );
        }
    }
    for query in [
        "RETURN s\"{1 AND EXISTS { RETURN 1 }}\"",
        "RETURN s\"{1 OR EXISTS { RETURN 1 }}\"",
        "RETURN s\"{CASE WHEN COUNT { RETURN 1 } THEN 1 ELSE 0 END}\"",
    ] {
        assert_eq!(
            execute(&connection, query, BTreeMap::new(), "{}", 1)
                .expect_err("invalid boolean operand")
                .kind,
            QueryErrorKind::Type
        );
    }
}

#[test]
fn scoped_graph_expressions_keep_list_variables_accumulators_and_shadowing() {
    let connection = storage();
    for (query, expected) in [
        (
            "RETURN all(x IN [1,2] WHERE EXISTS { WITH x WHERE x > 0 RETURN x })",
            Value::Boolean(true),
        ),
        (
            "RETURN any(x IN [1,2] WHERE COUNT { WITH x WHERE x = 2 RETURN x } = 1)",
            Value::Boolean(true),
        ),
        (
            "RETURN none(x IN [1,2] WHERE COUNT { WITH x WHERE x < 0 RETURN x } = 1)",
            Value::Boolean(true),
        ),
        (
            "RETURN single(x IN [1,2] WHERE COUNT { WITH x WHERE x = 2 RETURN x } = 1)",
            Value::Boolean(true),
        ),
        (
            "RETURN reduce(total=0,x IN [1,2] | total + COUNT { WITH x WHERE x > 0 RETURN x })",
            Value::Integer(2),
        ),
        (
            "RETURN allReduce(total=0,x IN [1,2] | total+x, EXISTS { WITH total WHERE total > 0 RETURN total })",
            Value::Boolean(true),
        ),
        (
            "RETURN all(x IN [1,2] WHERE any(x IN [x+1] WHERE COUNT { WITH x WHERE x > 1 RETURN x } = 1))",
            Value::Boolean(true),
        ),
        (
            "RETURN s\"{all(x IN [1,2] WHERE EXISTS { WITH x WHERE x > 0 RETURN x })}\"",
            Value::String("true".to_owned()),
        ),
    ] {
        assert_eq!(
            execute(&connection, query, BTreeMap::new(), "{}", 1).expect(query),
            vec![vec![expected]]
        );
    }
}

#[test]
fn scoped_graph_expressions_preserve_hidden_properties_plain_maps_and_group_inputs() {
    let connection = storage();
    execute(
        &connection,
        "CREATE (:Person {secret:1}), (:Person {secret:1}) FINISH",
        BTreeMap::new(),
        "{}",
        1,
    )
    .expect("graph fixture");
    let query = "MATCH (n:Person) WITH collect(n) AS people RETURN all(p IN people WHERE EXISTS { WITH p WHERE p.secret = 1 RETURN p }), reduce(total=0,p IN people | total + COUNT { WITH p WHERE p.secret = 1 RETURN p }), allReduce(total=0,p IN people | total + COUNT { WITH p WHERE p.secret = 1 RETURN p }, EXISTS { WITH total WHERE total > 0 RETURN total }), all(p IN [{secret:1}] WHERE EXISTS { WITH p WHERE p.secret = 1 RETURN p })";
    for (options, expected) in [
        (
            "{}",
            vec![
                Value::Boolean(true),
                Value::Integer(2),
                Value::Boolean(true),
                Value::Boolean(true),
            ],
        ),
        (
            r#"{"readAccess":{"result":{"denyProperties":["secret"]}}}"#,
            vec![
                Value::Boolean(true),
                Value::Integer(2),
                Value::Boolean(true),
                Value::Boolean(true),
            ],
        ),
        (
            r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#,
            vec![
                Value::Boolean(false),
                Value::Integer(0),
                Value::Boolean(false),
                Value::Boolean(true),
            ],
        ),
    ] {
        assert_eq!(
            execute(&connection, query, BTreeMap::new(), options, 1).expect("scoped visibility"),
            vec![expected]
        );
        let grouped = "MATCH (n:Person) RETURN all(p IN collect(n) WHERE EXISTS { WITH p WHERE p.secret = 1 RETURN p })";
        assert_eq!(
            execute(&connection, grouped, BTreeMap::new(), options, 1)
                .expect("scoped aggregate collection"),
            vec![vec![Value::Boolean(!options.contains("query"))]]
        );
    }
}

#[test]
fn scoped_graph_expressions_keep_null_empty_and_type_semantics() {
    let connection = storage();
    for (query, expected) in [
        (
            "RETURN all(x IN null WHERE EXISTS { RETURN 1 }), reduce(total=0,x IN null | total+COUNT { RETURN 1 }), allReduce(total=0,x IN null | total+x, EXISTS { RETURN 1 })",
            vec![Value::Null, Value::Null, Value::Null],
        ),
        (
            "RETURN all(x IN [] WHERE EXISTS { RETURN 1 }), reduce(total=0,x IN [] | total+COUNT { RETURN 1 }), allReduce(total=0,x IN [] | total+x, EXISTS { RETURN 1 })",
            vec![
                Value::Boolean(true),
                Value::Integer(0),
                Value::Boolean(true),
            ],
        ),
        (
            "RETURN all(x IN [1] WHERE CASE WHEN EXISTS { RETURN 1 } THEN null ELSE true END), any(x IN [1] WHERE CASE WHEN EXISTS { RETURN 1 } THEN null ELSE true END), allReduce(total=0,x IN [1] | total+x, CASE WHEN EXISTS { RETURN 1 } THEN null ELSE true END)",
            vec![Value::Null, Value::Null, Value::Null],
        ),
    ] {
        assert_eq!(
            execute(&connection, query, BTreeMap::new(), "{}", 1).expect(query),
            vec![expected]
        );
    }
    for (query, params) in [
        (
            "RETURN all(x IN $values WHERE EXISTS { RETURN 1 })",
            BTreeMap::from([("values".to_owned(), Value::Integer(1))]),
        ),
        (
            "RETURN reduce(total=0,x IN $values | total+COUNT { RETURN 1 })",
            BTreeMap::from([("values".to_owned(), Value::Integer(1))]),
        ),
        (
            "RETURN allReduce(total=0,x IN [1] | total+x, CASE WHEN EXISTS { RETURN 1 } THEN $predicate ELSE true END)",
            BTreeMap::from([("predicate".to_owned(), Value::Integer(1))]),
        ),
    ] {
        assert_eq!(
            execute(&connection, query, params, "{}", 1)
                .expect_err("type validation")
                .kind,
            QueryErrorKind::Type,
            "{query}"
        );
    }
}

#[test]
fn graph_sort_subqueries_can_read_projected_aliases_without_replacing_the_group() {
    let connection = storage();
    for sort in [
        "COUNT { WITH v WHERE v = 1 RETURN v }",
        "s\"{COUNT { WITH v WHERE v = 1 RETURN v }}\"",
    ] {
        let query = format!("UNWIND [1,2] AS x RETURN x AS v ORDER BY {sort} ASC");
        assert_eq!(
            execute(&connection, &query, BTreeMap::new(), "{}", 1).expect("projected sort alias"),
            vec![vec![Value::Integer(2)], vec![Value::Integer(1)]]
        );
        let grouped =
            format!("UNWIND [1,2] AS x RETURN x AS v, count(*) AS total ORDER BY {sort} ASC");
        assert_eq!(
            execute(&connection, &grouped, BTreeMap::new(), "{}", 1)
                .expect("grouped projected sort alias"),
            vec![
                vec![Value::Integer(2), Value::Integer(1)],
                vec![Value::Integer(1), Value::Integer(1)]
            ]
        );
    }
    execute(
        &connection,
        "CREATE (:SortPerson {name:'A', secret:1}), (:SortPerson {name:'B', secret:2}) FINISH",
        BTreeMap::new(),
        "{}",
        1,
    )
    .expect("sort graph fixture");
    let query = "MATCH (n:SortPerson) RETURN n.name, n AS v ORDER BY s\"{COUNT { WITH v WHERE v.secret = 1 RETURN v }}\" ASC";
    for (options, expected) in [
        ("{}", vec!["B", "A"]),
        (
            r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#,
            vec!["A", "B"],
        ),
    ] {
        let rows =
            execute(&connection, query, BTreeMap::new(), options, 1).expect("sort alias policy");
        assert_eq!(
            rows.iter().map(|row| row[0].clone()).collect::<Vec<_>>(),
            expected
                .into_iter()
                .map(|value| Value::String(value.to_owned()))
                .collect::<Vec<_>>()
        );
    }
    let query =
        "UNWIND [null,1] AS x RETURN x AS v ORDER BY COUNT { WITH v WHERE v IS NULL RETURN v } ASC";
    assert_eq!(
        execute(&connection, query, BTreeMap::new(), "{}", 1).expect("null alias"),
        vec![vec![Value::Integer(1)], vec![Value::Null]]
    );
}

#[test]
fn interpolated_csv_reads_succeed_when_allowed_and_are_rejected_before_open_when_denied() {
    let connection = storage();
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("interpolation.csv");
    std::fs::write(&path, "id,name\n1,Alice\n2,Bob\n").expect("synthetic CSV");
    let source = format!("file://{}", path.display());
    let query = "RETURN s\"{COUNT { LOAD CSV WITH HEADERS FROM $source AS row RETURN row }}\"";
    let params = BTreeMap::from([("source".to_owned(), Value::String(source))]);
    for options in ["{}", r#"{"readAccess":{"allowExternalIO":true}}"#] {
        assert_eq!(
            execute(&connection, query, params.clone(), options, 1)
                .expect("permitted CSV interpolation"),
            text("2")
        );
    }
    std::fs::remove_file(path).expect("remove CSV to distinguish authorization from I/O failure");
    let error = execute(
        &connection,
        query,
        params.clone(),
        r#"{"readAccess":{"allowExternalIO":false}}"#,
        1,
    )
    .expect_err("forbidden before file open");
    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_AUTH));
    let error = execute(
        &connection,
        query,
        params.clone(),
        r#"{"readOnlyNoExternalIO":true}"#,
        1,
    )
    .expect_err("existing safety guard");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    for (dead, expected) in [
        (
            "RETURN CASE WHEN false THEN s\"{COUNT { LOAD CSV FROM $source AS row RETURN row }}\" ELSE 'safe' END",
            "safe",
        ),
        (
            "UNWIND [1,2,3] AS n RETURN CASE WHEN sum(n) = 6 THEN 'safe' ELSE s\"{COUNT { LOAD CSV FROM $source AS row RETURN row }}\" END",
            "safe",
        ),
        (
            "RETURN s\"{false AND EXISTS { LOAD CSV FROM $source AS row RETURN row }}\"",
            "false",
        ),
        (
            "RETURN s\"{true OR EXISTS { LOAD CSV FROM $source AS row RETURN row }}\"",
            "true",
        ),
        (
            "RETURN s\"{reduce(total=0,x IN [] | total+COUNT { LOAD CSV FROM $source AS row RETURN row })}\"",
            "0",
        ),
        (
            "RETURN s\"{allReduce(total=0,x IN null | total+x, EXISTS { LOAD CSV FROM $source AS row RETURN row })}\"",
            "null",
        ),
        (
            "RETURN s\"{size([x IN [] | COUNT { LOAD CSV FROM $source AS row RETURN row }])}\"",
            "0",
        ),
    ] {
        for prefix in ["", "PROFILE "] {
            let dead = format!("{prefix}{dead}");
            assert_eq!(
                execute(&connection, &dead, params.clone(), "{}", 1)
                    .expect("unused interpolation never opens source"),
                text(expected)
            );
            assert_eq!(
                execute(
                    &connection,
                    &dead,
                    params.clone(),
                    r#"{"readAccess":{"allowExternalIO":false}}"#,
                    1
                )
                .expect_err("full AST still rejects unused external capability")
                .kind,
                QueryErrorKind::AccessDenied
            );
        }
    }
}
