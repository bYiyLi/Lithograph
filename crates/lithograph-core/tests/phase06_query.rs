use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, prepare};
use lithograph_core::storage::{create_storage_schema, initialize_root};
use rusqlite::Connection;

#[path = "phase06_query/mutation.rs"]
mod mutation;
#[path = "phase06_query/temporal.rs"]
mod temporal;

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite must open");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1),\
             magic TEXT NOT NULL,\
             database_id TEXT NOT NULL,\
             storage_format INTEGER NOT NULL);",
        )
        .expect("metadata table");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn rows(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    rows_with_options(connection, query, ExecutionOptions::default())
}

fn rows_with_options(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Vec<Vec<Value>> {
    let prepared = prepare(connection, query, BTreeMap::new(), options)
        .unwrap_or_else(|error| panic!("prepare {query:?}: {error}"));
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor
            .next_batch(connection, 2)
            .unwrap_or_else(|error| panic!("execute {query:?}: {error}"));
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection).expect("complete read");
            return rows;
        }
    }
}

fn execution_error(connection: &Connection, query: &str) -> String {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap_or_else(|error| panic!("prepare {query:?}: {error}"));
    let mut cursor = QueryCursor::new(prepared);
    cursor
        .next_batch(connection, 2)
        .expect_err("query must fail during execution")
        .to_string()
}

fn query_error(connection: &Connection, query: &str) -> String {
    match prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    ) {
        Err(error) => error.to_string(),
        Ok(prepared) => {
            let mut cursor = QueryCursor::new(prepared);
            cursor
                .next_batch(connection, 2)
                .expect_err("query must fail before completion")
                .to_string()
        }
    }
}

#[test]
fn unwind_with_filter_and_order_compose_rows() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2] AS x WITH x, x + 1 AS y WHERE y > 1 RETURN x, y ORDER BY x DESC",
        ),
        vec![
            vec![Value::Integer(2), Value::Integer(3)],
            vec![Value::Integer(1), Value::Integer(2)],
        ]
    );
}

#[test]
fn let_and_for_share_the_clause_pipeline() {
    let connection = fresh_storage();
    assert_eq!(
        rows(&connection, "LET x = 1 FOR y IN [x, 2] RETURN y"),
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
    assert_eq!(
        rows(&connection, "UNWIND 5 AS x FOR y IN x + 1 RETURN x, y",),
        vec![vec![Value::Integer(5), Value::Integer(6)]]
    );
    assert!(rows(&connection, "UNWIND null AS x RETURN x").is_empty());
}

#[test]
fn union_all_and_next_preserve_composed_query_semantics() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "{ RETURN 1 AS x } UNION ALL { RETURN 2 AS x } NEXT RETURN x + 1 AS y",
        ),
        vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]
    );
}

#[test]
fn next_uses_only_explicit_results_and_resets_unit_queries() {
    let connection = fresh_storage();
    assert_eq!(
        rows(&connection, "FINISH NEXT RETURN 1 AS value"),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [] AS ignored FINISH NEXT RETURN 2 AS value",
        ),
        vec![vec![Value::Integer(2)]]
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2] AS value CREATE (:NextWrite {value:value}) NEXT RETURN 3 AS value",
        ),
        vec![vec![Value::Integer(3)]]
    );
    assert!(query_error(&connection, "CREATE (node:Hidden) NEXT RETURN node").contains("node"));
}

#[test]
fn next_preserves_unit_call_cardinality_without_leaking_scope() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2] AS value CALL { WITH value CREATE (:UnitCall {value:value}) } NEXT RETURN 1 AS result",
        ),
        vec![vec![Value::Integer(1)], vec![Value::Integer(1)]]
    );
    assert!(
        query_error(
            &connection,
            "UNWIND [1] AS value CALL { WITH value CREATE (:NoLeak) } NEXT RETURN value",
        )
        .contains("value")
    );

    rows(&connection, "CREATE (:FirstLabel), (:SecondLabel) FINISH");
    assert_eq!(
        rows(
            &connection,
            "CALL db.labels() YIELD label WHERE label IN ['FirstLabel', 'SecondLabel'] NEXT RETURN 1 AS result",
        ),
        vec![vec![Value::Integer(1)], vec![Value::Integer(1)]]
    );
    assert!(
        query_error(
            &connection,
            "CALL db.labels() YIELD label NEXT RETURN label",
        )
        .contains("label")
    );
}

#[test]
fn next_requires_aliases_and_scopes_union_flavors_per_segment() {
    let connection = fresh_storage();
    assert!(!query_error(&connection, "RETURN 1 NEXT RETURN 2 AS value").is_empty());
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1] AS value RETURN * NEXT RETURN value",
        ),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        rows(
            &connection,
            "RETURN 1 AS x UNION RETURN 2 AS x NEXT RETURN x AS y UNION ALL RETURN x + 10 AS y NEXT RETURN y ORDER BY y",
        ),
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(11)],
            vec![Value::Integer(12)],
        ]
    );
}

#[test]
fn incomplete_queries_and_returning_terminal_subqueries_are_rejected() {
    let connection = fresh_storage();
    for query in [
        "MATCH (node)",
        "UNWIND [1] AS value",
        "WITH 1 AS value",
        "LET value = 1",
        "CALL { RETURN 1 AS value }",
    ] {
        assert!(
            query_error(&connection, query).contains("incomplete query"),
            "query must be rejected as incomplete: {query}",
        );
    }
    rows(&connection, "CALL { CREATE (:TerminalUnitCall) }");
    assert_eq!(
        rows(
            &connection,
            "MATCH (node:TerminalUnitCall) RETURN count(node)",
        ),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn when_selects_the_first_true_branch() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "WHEN false THEN { RETURN 1 AS x } WHEN true THEN { RETURN 2 AS x } ELSE { RETURN 3 AS x }",
        ),
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn unbraced_when_executes_only_the_selected_write_branch() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "WHEN true THEN CREATE (:Chosen) RETURN 1 AS value ELSE CREATE (:Other) RETURN 2 AS value",
        ),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (chosen:Chosen) RETURN count(chosen), COUNT { MATCH (other:Other) RETURN other }",
        ),
        vec![vec![Value::Integer(1), Value::Integer(0)]]
    );
}

#[test]
fn aggregate_inventory_honors_null_and_per_call_distinct() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 1, 2, null] AS x RETURN count(*) AS rows, count(x) AS values, count(DISTINCT x) AS uniqueValues, sum(x) AS sum, avg(x) AS average, collect(DISTINCT x) AS collected",
        ),
        vec![vec![
            Value::Integer(4),
            Value::Integer(3),
            Value::Integer(2),
            Value::Integer(4),
            Value::Float(4.0 / 3.0),
            Value::List(vec![Value::Integer(1), Value::Integer(2)]),
        ]]
    );
    for query in [
        "UNWIND [] AS x RETURN percentileCont(x, 2)",
        "UNWIND [] AS x RETURN percentileDisc(x, null)",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }
}

#[test]
fn aggregate_expressions_materialize_subqueries_duration_and_empty_groups() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2] AS x RETURN sum(COUNT { UNWIND range(1, x) AS y RETURN y }) AS total, CASE WHEN count(*) = 2 THEN 'ok' ELSE 'bad' END AS state",
        ),
        vec![vec![Value::Integer(3), Value::String("ok".to_owned()),]]
    );

    assert_eq!(
        rows(
            &connection,
            "UNWIND [duration('P2DT3H'), duration('PT1H45S')] AS value RETURN sum(value), avg(value)",
        ),
        vec![vec![
            Value::Duration(
                lithograph_core::cypher::DurationValue::parse("P2DT4H45S").expect("sum duration"),
            ),
            Value::Duration(
                lithograph_core::cypher::DurationValue::parse("P1DT2H22.5S")
                    .expect("average duration"),
            ),
        ]]
    );

    assert_eq!(
        rows(&connection, "UNWIND [] AS x RETURN stDev(x), stDevP(x)",),
        vec![vec![Value::Null, Value::Null]]
    );
}

#[test]
fn implicit_and_explicit_grouping_share_alias_semantics() {
    let connection = fresh_storage();
    let expected = vec![
        vec![Value::Integer(0), Value::Integer(2), Value::Integer(6)],
        vec![Value::Integer(1), Value::Integer(2), Value::Integer(4)],
    ];
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2, 3, 4] AS x RETURN x % 2 AS parity, count(*) AS total, sum(x) AS sum ORDER BY parity",
        ),
        expected
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2, 3, 4] AS x RETURN x % 2 AS parity, count(*) AS total, sum(x) AS sum GROUP BY parity ORDER BY parity",
        ),
        expected
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2, 3, 4] AS x RETURN x % 2 AS parity, count(*) AS total, sum(x) AS sum GROUP BY ALL ORDER BY parity",
        ),
        expected
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2, 3, 4] AS x RETURN x % 2 + count(*) AS combined GROUP BY x % 2 ORDER BY combined",
        ),
        vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 1, 2] AS x RETURN x GROUP BY ALL ORDER BY x",
        ),
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2] AS x RETURN 1 AS constant, count(*) AS total GROUP BY ()",
        ),
        vec![vec![Value::Integer(1), Value::Integer(2)]]
    );
    for query in [
        "UNWIND [1, 2] AS x RETURN x, count(*) GROUP BY ()",
        "UNWIND [1, 2] AS x RETURN x + 1 AS y, count(*) GROUP BY x",
        "UNWIND [1, 2] AS x RETURN rand(), count(*) GROUP BY ()",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }
}

#[test]
fn global_aggregation_emits_identity_row_for_empty_input() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "UNWIND [] AS x RETURN count(*) AS total, count(x) AS present, sum(x) AS sum, collect(x) AS values, avg(x) AS average",
        ),
        vec![vec![
            Value::Integer(0),
            Value::Integer(0),
            Value::Integer(0),
            Value::List(Vec::new()),
            Value::Null,
        ]]
    );
}

#[test]
fn comparison_case_subscript_and_comprehension_semantics_execute() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "RETURN 2 IN [1, 2] AS inside, 'abc' STARTS WITH 'a' AS starts, null IS NULL AS empty, 1 IS :: INTEGER AS typed, [1,2,3][-1] AS last, [1,2,3][1..] AS tail, CASE 1 WHEN 1 THEN 'yes' ELSE 'no' END AS choice, [x IN [1,2,3] WHERE x > 1 | x * 2] AS mapped, all(x IN [true, null] WHERE x) AS every",
        ),
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Integer(3),
            Value::List(vec![Value::Integer(2), Value::Integer(3)]),
            Value::String("yes".to_owned()),
            Value::List(vec![Value::Integer(4), Value::Integer(6)]),
            Value::Null,
        ]]
    );
}

#[test]
fn normalization_predicates_support_all_forms_and_null_for_non_strings() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "WITH '\u{00e4}' AS composed, 'a\u{0308}' AS decomposed RETURN composed IS NORMALIZED, composed IS NFD NORMALIZED, decomposed IS NOT NORMALIZED, decomposed IS NFD NORMALIZED, '\u{fe64}' IS NFKC NORMALIZED, normalize('\u{fe64}', NFKC) IS NFKC NORMALIZED, 1 IS NORMALIZED, null IS NOT NORMALIZED",
        ),
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn map_projection_and_string_interpolation_preserve_unicode() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "WITH {a: 1, b: 2} AS m RETURN m{.*, a: 3, c: 4} AS mapped, s\"值={1 + 2}😀\" AS rendered",
        ),
        vec![vec![
            Value::Map(BTreeMap::from([
                ("a".to_owned(), Value::Integer(3)),
                ("b".to_owned(), Value::Integer(2)),
                ("c".to_owned(), Value::Integer(4)),
            ])),
            Value::String("值=3😀".to_owned()),
        ]]
    );
}

#[test]
fn scalar_function_families_return_typed_values() {
    let connection = fresh_storage();
    let result = rows(
        &connection,
        "RETURN abs(-2) AS absolute, round(1.25, 1) AS rounded, normalize('é') AS normalized, reverse('😀a') AS reversed, range(3, 1, -1) AS range, toInteger('42') AS converted, coalesce(null, 'x') AS fallback, uuid('550e8400-e29b-41d4-a716-446655440000') AS uuid, vector([1.0, 2.0], 2, FLOAT64) AS vector",
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Integer(2));
    assert_eq!(result[0][1], Value::Float(1.3));
    assert_eq!(result[0][2], Value::String("é".to_owned()));
    assert_eq!(result[0][3], Value::String("a😀".to_owned()));
    assert_eq!(
        result[0][4],
        Value::List(vec![
            Value::Integer(3),
            Value::Integer(2),
            Value::Integer(1)
        ])
    );
    assert_eq!(result[0][5], Value::Integer(42));
    assert_eq!(result[0][6], Value::String("x".to_owned()));
    assert!(matches!(result[0][7], Value::Uuid(_)));
    assert!(matches!(result[0][8], Value::Vector(ref value) if value.dimension() == 2));
}

#[test]
fn numeric_overflow_and_nan_keep_distinct_runtime_semantics() {
    let connection = fresh_storage();
    assert!(
        execution_error(&connection, "RETURN 9223372036854775807 + 1 AS overflow")
            .contains("overflow")
    );
    assert_eq!(
        rows(
            &connection,
            "RETURN isNaN(sqrt(-1.0)) AS nan, isNaN(1.0) AS finite",
        ),
        vec![vec![Value::Boolean(true), Value::Boolean(false)]]
    );
}

#[test]
fn round_modes_and_vector_list_conversions_follow_the_frozen_profile() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "RETURN round(-1.5) AS legacyTie, round(-1.55, 1) AS defaultTie, round(1.249, 1, 'UP') AS up, round(-1.251, 1, 'DOWN') AS down, round(-1.251, 1, 'CEILING') AS ceiling, round(-1.251, 1, 'FLOOR') AS floor, round(1.25, 1, 'HALF_DOWN') AS halfDown, round(1.25, 1, 'HALF_EVEN') AS halfEven, round(1.35, 1, 'HALF_EVEN') AS halfOdd, toFloatList(vector([1, 2], 2, INTEGER64)) AS floats, toIntegerList(vector([1.9, -2.1], 2, FLOAT64)) AS integers",
        ),
        vec![vec![
            Value::Float(-1.0),
            Value::Float(-1.6),
            Value::Float(1.3),
            Value::Float(-1.2),
            Value::Float(-1.2),
            Value::Float(-1.3),
            Value::Float(1.2),
            Value::Float(1.2),
            Value::Float(1.4),
            Value::List(vec![Value::Float(1.0), Value::Float(2.0)]),
            Value::List(vec![Value::Integer(1), Value::Integer(-2)]),
        ]]
    );
    assert!(query_error(&connection, "RETURN round(1.2, 1, 'UNKNOWN')").contains("mode"));
}

#[test]
fn spatial_vector_and_temporal_constructors_compose() {
    let connection = fresh_storage();
    let result = rows(
        &connection,
        "RETURN point.distance(point({x: 0.0, y: 0.0}), point({x: 3.0, y: 4.0})) AS distance, vector_distance(vector([0.0, 0.0], 2, FLOAT64), vector([3.0, 4.0], 2, FLOAT64), EUCLIDEAN) AS vectorDistance, date('2024-02-29') AS date, datetime('2024-03-31T01:30:00+01:00[Europe/Paris]') AS zoned",
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Float(5.0));
    assert_eq!(result[0][1], Value::Float(5.0));
    assert!(matches!(result[0][2], Value::Date(ref value) if value.as_str() == "2024-02-29"));
    assert!(
        matches!(result[0][3], Value::ZonedDateTime(ref value) if value.zone() == "Europe/Paris")
    );
}

#[test]
fn frozen_collection_text_numeric_and_vector_edges_match_the_profile() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "RETURN cardinality({a: 1, b: 2}) AS mapCardinality, isEmpty({}) AS emptyMap, coll.flatten(['a', ['b', ['c']]], 2) AS flat, coll.indexOf([1, 2], null) AS nullIndex, coll.sort(null) AS nullSort, string.join(['a', null, 'b'], '-') AS joined, split('a,b;c', [',', ';']) AS pieces, split('', '') AS emptySplit, replace('hello', 'l', 'w', 1) AS replaced, sign(-0.2) AS sign, vector_distance(vector([0, 0], 2, INTEGER8), vector([3, 4], 2, INTEGER8), EUCLIDEAN_SQUARED) AS squared, vector_distance(vector([1, 2], 2, INTEGER8), vector([3, 4], 2, INTEGER8), DOT) AS dot, vector_norm(vector([3, 4], 2, INTEGER8), MANHATTAN) AS norm, size(vector([1, 2], 2, INTEGER8)) AS vectorSize, vector.similarity.euclidean([0, 0], [3, 4]) AS similarity",
        ),
        vec![vec![
            Value::Integer(2),
            Value::Boolean(true),
            Value::List(vec![
                Value::String("a".to_owned()),
                Value::String("b".to_owned()),
                Value::String("c".to_owned()),
            ]),
            Value::Null,
            Value::Null,
            Value::String("a-b".to_owned()),
            Value::List(vec![
                Value::String("a".to_owned()),
                Value::String("b".to_owned()),
                Value::String("c".to_owned()),
            ]),
            Value::List(vec![Value::String(String::new())]),
            Value::String("hewlo".to_owned()),
            Value::Integer(-1),
            Value::Float(25.0),
            Value::Float(-11.0),
            Value::Float(7.0),
            Value::Integer(2),
            Value::Float(f64::from(1.0_f32 / 26.0_f32)),
        ]]
    );

    for query in [
        "RETURN coll.insert([1], -1, 2)",
        "RETURN coll.remove([1], 1)",
        "RETURN coll.flatten([[1]], -1)",
        "RETURN string.join(['a'])",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }

    let _ = rows(
        &connection,
        "CREATE (a:Cardinality)-[:EDGE]->(b:Cardinality) RETURN a",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH p = (:Cardinality)-[:EDGE]->(:Cardinality) RETURN cardinality(p)",
        ),
        vec![vec![Value::Integer(3)]]
    );
}

#[test]
fn conversion_type_normalization_and_keyword_string_forms_match_the_profile() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "RETURN toBoolean('not a boolean') AS booleanFailure, toInteger('not an integer') AS integerFailure, toFloat('not a float') AS floatFailure, valueType([date('2024-01-01')]) AS dateListType, valueType([1, null]) AS nullableListType, valueType([]) AS emptyListType, valueType(vector([1, 2], 2, FLOAT32 NOT NULL)) AS vectorType, valueType(uuid('550e8400-e29b-41d4-a716-446655440000')) AS uuidType, normalize('\u{FE64}', NFKC) AS normalized, trim(LEADING 'x' FROM 'xxyx') AS leading, trim(TRAILING 'x' FROM 'xxyx') AS trailing, trim(BOTH 'x' FROM 'xxyx') AS both",
        ),
        vec![vec![
            Value::Null,
            Value::Null,
            Value::Null,
            Value::String("LIST<DATE NOT NULL> NOT NULL".to_owned()),
            Value::String("LIST<INTEGER> NOT NULL".to_owned()),
            Value::String("LIST<NOTHING> NOT NULL".to_owned()),
            Value::String("VECTOR<FLOAT32 NOT NULL>(2) NOT NULL".to_owned()),
            Value::String("UUID NOT NULL".to_owned()),
            Value::String("<".to_owned()),
            Value::String("yx".to_owned()),
            Value::String("xxy".to_owned()),
            Value::String("y".to_owned()),
        ]]
    );
    for query in [
        "RETURN toBoolean(1.2)",
        "RETURN left('x', -1)",
        "RETURN left('x', null)",
        "RETURN right('x', null)",
        "RETURN substring('x', -1)",
        "RETURN substring('x', null)",
        "RETURN substring('x', 0, null)",
        "RETURN string.indexOf('hello', 'l', 1)",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }
    assert_eq!(
        rows(
            &connection,
            "RETURN left(null, null), right(null, null), substring(null, 0)",
        ),
        vec![vec![Value::Null, Value::Null, Value::Null]]
    );
}

#[test]
fn correlated_call_subqueries_run_once_per_input_row_and_can_nest() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2] AS x CALL (x) { RETURN x * 10 AS y } CALL (y) { RETURN y + 1 AS z } RETURN x, y, z ORDER BY x",
        ),
        vec![
            vec![Value::Integer(1), Value::Integer(10), Value::Integer(11)],
            vec![Value::Integer(2), Value::Integer(20), Value::Integer(21)],
        ]
    );
    assert_eq!(
        rows(&connection, "LET a = 1, b = 2 LET c = a + b RETURN a, b, c",),
        vec![vec![
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(3),
        ]]
    );
    for query in [
        "LET a = 1, b = a + 1 RETURN b",
        "WITH 1 AS a LET a = 2 RETURN a",
        "LET a = 1, a = 2 RETURN a",
        "WITH 1 AS x UNWIND [2] AS x RETURN x",
        "WITH 1 AS x FOR x IN [2] RETURN x",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }
}

#[test]
fn call_subquery_imports_obey_scope_clause_and_importing_with_rules() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "WITH 11 AS x CALL (x) { UNWIND [2, 3] AS y WITH y RETURN x * y AS a } RETURN x, a ORDER BY a",
        ),
        vec![
            vec![Value::Integer(11), Value::Integer(22)],
            vec![Value::Integer(11), Value::Integer(33)],
        ]
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2] AS x CALL { WITH x RETURN x * 10 AS y } RETURN x, y ORDER BY x",
        ),
        vec![
            vec![Value::Integer(1), Value::Integer(10)],
            vec![Value::Integer(2), Value::Integer(20)],
        ]
    );
    assert_eq!(
        rows(
            &connection,
            "WITH 1 AS x, 2 AS y CALL { WITH x RETURN x AS z UNION ALL WITH y RETURN y AS z } RETURN z ORDER BY z",
        ),
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
    assert_eq!(
        rows(
            &connection,
            "WITH 1 AS x CALL { WITH x WITH 2 AS y RETURN y AS z } RETURN x, z",
        ),
        vec![vec![Value::Integer(1), Value::Integer(2)]]
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2] AS x CALL (x) { CALL () { UNWIND [] AS y RETURN y } FINISH } RETURN x ORDER BY x",
        ),
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
    assert_eq!(
        rows(
            &connection,
            "CALL { RETURN 1 AS x NEXT RETURN x AS y UNION ALL RETURN x + 10 AS y } RETURN y ORDER BY y",
        ),
        vec![vec![Value::Integer(1)], vec![Value::Integer(11)]]
    );

    for query in [
        "WITH [1, 2, 3] AS values CALL { WITH values WHERE size(values) > 2 RETURN values AS selected } RETURN selected",
        "WITH 1 AS x CALL (x) { WITH 2 AS x RETURN x AS y } RETURN y",
        "WITH 1 AS x CALL (x) { LET y = 1, x = 2 RETURN y } RETURN y",
        "WITH 1 AS x CALL (x) { UNWIND [2] AS x RETURN x AS y } RETURN y",
        "WITH 1 AS x CALL { WITH x RETURN x AS y NEXT RETURN y } RETURN y",
        "WITH 1 AS x CALL () { RETURN x AS y } RETURN y",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }

    let collision = fresh_storage();
    let _ = rows(&collision, "CREATE (:N {value: 7}) RETURN 1");
    assert_eq!(
        rows(
            &collision,
            "WITH 1 AS n CALL { MATCH (n:N) RETURN n.value AS value } RETURN n, value",
        ),
        vec![vec![Value::Integer(1), Value::Integer(7)]]
    );
}

#[test]
fn current_graph_registry_drives_call() {
    let connection = fresh_storage();
    let _ = rows(
        &connection,
        "CREATE (:Person {name: 'Ada'}) RETURN count(*) AS created",
    );
    assert_eq!(
        rows(&connection, "CALL db.labels() YIELD label RETURN label"),
        vec![vec![Value::String("Person".to_owned())]]
    );
    assert_eq!(
        rows(&connection, "CALL db.labels()"),
        vec![vec![Value::String("Person".to_owned())]]
    );
    assert_eq!(
        rows(
            &connection,
            "CALL db.labels() YIELD label WHERE label STARTS WITH 'P' RETURN label",
        ),
        vec![vec![Value::String("Person".to_owned())]]
    );
    assert_eq!(
        rows(
            &connection,
            "CALL db.labels() YIELD label WHERE label STARTS WITH 'X' RETURN label",
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        rows(&connection, "CALL db.labels() YIELD *"),
        vec![vec![Value::String("Person".to_owned())]]
    );
    for query in [
        "CALL db.labels() RETURN label",
        "CALL db.labels() YIELD * RETURN label",
        "CALL db.labels() YIELD missing RETURN missing",
        "CALL db.labels() YIELD label WHERE missing RETURN label",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }
}

fn assert_show_function_inventory(connection: &Connection) {
    let functions = rows(
        connection,
        "SHOW FUNCTIONS YIELD name, aggregating WHERE aggregating RETURN name ORDER BY name",
    );
    assert!(functions.contains(&vec![Value::String("count".to_owned())]));
    assert!(functions.contains(&vec![Value::String("sum".to_owned())]));
    assert_eq!(
        rows(
            connection,
            "SHOW FUNCTIONS YIELD name WHERE name = 'allReduce' RETURN name",
        ),
        vec![vec![Value::String("allReduce".to_owned())]]
    );
    assert_eq!(
        rows(
            connection,
            "SHOW FUNCTIONS YIELD name, category WHERE name IN ['abs', 'acos', 'all', 'string.join', 'db.nameFromElementId'] RETURN name, category ORDER BY name",
        ),
        vec![
            vec![
                Value::String("abs".to_owned()),
                Value::String("Numeric".to_owned()),
            ],
            vec![
                Value::String("acos".to_owned()),
                Value::String("Trigonometric".to_owned()),
            ],
            vec![
                Value::String("all".to_owned()),
                Value::String("Predicate".to_owned()),
            ],
            vec![
                Value::String("db.nameFromElementId".to_owned()),
                Value::String("Database".to_owned()),
            ],
            vec![
                Value::String("string.join".to_owned()),
                Value::String("String".to_owned()),
            ],
        ]
    );
    assert_eq!(
        rows(
            connection,
            "SHOW FUNCTIONS YIELD name, signature WHERE name = 'uuid' RETURN signature",
        ),
        vec![
            vec![Value::String("uuid() :: UUID".to_owned())],
            vec![Value::String("uuid(name :: STRING) :: UUID".to_owned())],
            vec![Value::String(
                "uuid(mostSigBits :: INTEGER, leastSigBits :: INTEGER) :: UUID".to_owned(),
            )],
        ]
    );
}

fn assert_show_function_metadata(connection: &Connection) {
    assert_eq!(
        rows(
            connection,
            "SHOW FUNCTIONS YIELD name, signature, returnDescription WHERE name IN ['avg', 'coll.indexOf', 'keys', 'nodes', 'relationships', 'string.join', 'timestamp', 'toIntegerList'] RETURN name, signature, returnDescription ORDER BY name",
        ),
        vec![
            vec![
                Value::String("avg".to_owned()),
                Value::String(
                    "avg(input :: INTEGER | FLOAT | DURATION) :: INTEGER | FLOAT | DURATION"
                        .to_owned(),
                ),
                Value::String("INTEGER | FLOAT | DURATION".to_owned()),
            ],
            vec![
                Value::String("coll.indexOf".to_owned()),
                Value::String(
                    "coll.indexOf(list :: LIST<ANY>, value :: ANY) :: INTEGER".to_owned(),
                ),
                Value::String("INTEGER".to_owned()),
            ],
            vec![
                Value::String("keys".to_owned()),
                Value::String(
                    "keys(input :: NODE | RELATIONSHIP | MAP) :: LIST<STRING>".to_owned(),
                ),
                Value::String("LIST<STRING>".to_owned()),
            ],
            vec![
                Value::String("nodes".to_owned()),
                Value::String("nodes(input :: PATH) :: LIST<NODE>".to_owned()),
                Value::String("LIST<NODE>".to_owned()),
            ],
            vec![
                Value::String("relationships".to_owned()),
                Value::String("relationships(input :: PATH) :: LIST<RELATIONSHIP>".to_owned(),),
                Value::String("LIST<RELATIONSHIP>".to_owned()),
            ],
            vec![
                Value::String("string.join".to_owned()),
                Value::String(
                    "string.join(input :: LIST<STRING>, delimiter :: STRING) :: STRING".to_owned(),
                ),
                Value::String("STRING".to_owned()),
            ],
            vec![
                Value::String("timestamp".to_owned()),
                Value::String("timestamp() :: INTEGER".to_owned()),
                Value::String("INTEGER".to_owned()),
            ],
            vec![
                Value::String("toIntegerList".to_owned()),
                Value::String(
                    "toIntegerList(input :: VECTOR | LIST<ANY>) :: LIST<INTEGER>".to_owned(),
                ),
                Value::String("LIST<INTEGER>".to_owned()),
            ],
        ]
    );
    assert!(
        rows(
            connection,
            "SHOW FUNCTIONS YIELD name WHERE name = 'property_exists' RETURN name",
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            connection,
            "SHOW FUNCTIONS YIELD name, isDeprecated, deprecatedBy WHERE name = 'id' RETURN isDeprecated, deprecatedBy",
        ),
        vec![vec![
            Value::Boolean(true),
            Value::String("elementId".to_owned()),
        ]]
    );
    assert_eq!(
        rows(
            connection,
            "SHOW FUNCTIONS YIELD name, argumentDescription WHERE name = 'abs' RETURN 'default' IN keys(argumentDescription[0]), argumentDescription[0].default IS NULL",
        ),
        vec![vec![Value::Boolean(true), Value::Boolean(true)]]
    );
}

#[test]
fn show_uses_the_complete_registry_column_contract() {
    let connection = fresh_storage();
    assert_show_function_inventory(&connection);
    assert_show_function_metadata(&connection);

    let prepared = prepare(
        &connection,
        "SHOW FUNCTIONS YIELD aggregating",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("SHOW YIELD columns");
    assert_eq!(prepared.columns, vec!["aggregating"]);
    let all_columns = rows(&connection, "SHOW FUNCTIONS YIELD * LIMIT 1");
    assert_eq!(all_columns.len(), 1);
    assert_eq!(all_columns[0].len(), 12);
    assert!(matches!(&all_columns[0][3], Value::String(value) if value.contains("(")));
    assert!(matches!(&all_columns[0][5], Value::List(_)));
    assert_eq!(all_columns[0][10], Value::Boolean(false));
    assert_eq!(all_columns[0][11], Value::Null);
    assert_eq!(
        rows(&connection, "SHOW FUNCTION YIELD name WHERE name = 'abs'"),
        vec![vec![Value::String("abs".to_owned())]]
    );
    assert_eq!(
        rows(
            &connection,
            "SHOW FUNCTIONS YIELD name ORDER BY name SKIP 1 LIMIT 2 RETURN collect(name)",
        ),
        vec![vec![Value::List(vec![
            Value::String("acos".to_owned()),
            Value::String("all".to_owned()),
        ])]]
    );
    assert_eq!(
        rows(
            &connection,
            "SHOW FUNCTIONS YIELD name WITH name WHERE name = 'abs' RETURN name",
        ),
        vec![vec![Value::String("abs".to_owned())]]
    );
    for query in [
        "SHOW FUNCTIONS WITH name RETURN name",
        "SHOW FUNCTIONS YIELD * WITH name RETURN name",
        "SHOW FUNCTIONS RETURN name",
        "SHOW FUNCTIONS YIELD missing",
        "SHOW FUNCTIONS YIELD name + '!' AS decorated",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }
}

#[test]
fn composable_show_preserves_input_rows_and_scope() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2] AS x SHOW FUNCTIONS YIELD name WHERE name = 'abs' RETURN x, name ORDER BY x",
        ),
        vec![
            vec![Value::Integer(1), Value::String("abs".to_owned())],
            vec![Value::Integer(2), Value::String("abs".to_owned())],
        ]
    );
    assert_eq!(
        rows(
            &connection,
            "WITH 1 AS x SHOW FUNCTIONS YIELD name WHERE x = 1 AND name = 'abs' RETURN x, name",
        ),
        vec![vec![Value::Integer(1), Value::String("abs".to_owned()),]]
    );
    assert!(
        rows(
            &connection,
            "WITH 1 AS x WHERE false SHOW FUNCTIONS YIELD name RETURN x, name LIMIT 1",
        )
        .is_empty()
    );
    for query in [
        "WITH 1 AS x SHOW FUNCTIONS YIELD name",
        "WITH 'outer' AS name SHOW FUNCTIONS YIELD name RETURN name LIMIT 1",
        "WITH 1 AS x SHOW FUNCTIONS YIELD x RETURN x",
        "WITH 1 AS x SHOW FUNCTIONS YIELD * RETURN x",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }
}

#[test]
fn show_procedures_uses_the_complete_registry_column_contract() {
    let connection = fresh_storage();
    let default_columns = rows(&connection, "SHOW PROCEDURES");
    assert_eq!(default_columns.len(), 3);
    for row in default_columns {
        assert_eq!(row.len(), 4);
        assert_eq!(row[2], Value::String("READ".to_owned()));
        assert_eq!(row[3], Value::Boolean(false));
    }
    let all_columns = rows(&connection, "SHOW PROCEDURES YIELD * LIMIT 1");
    assert_eq!(all_columns.len(), 1);
    assert_eq!(all_columns[0].len(), 13);
    assert!(matches!(&all_columns[0][4], Value::String(value) if value.contains("::")));
    assert!(matches!(&all_columns[0][6], Value::List(values) if values.len() == 1));
    assert_eq!(
        all_columns[0][12],
        Value::Map(BTreeMap::from([(
            "deprecated".to_owned(),
            Value::Boolean(false),
        )]))
    );
    assert_eq!(rows(&connection, "SHOW PROCEDURE").len(), 3);
    assert!(!query_error(&connection, "SHOW PROCEDURES YIELD category").is_empty());
}

#[test]
fn correlated_subquery_expressions_return_boolean_count_and_list() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2] AS x RETURN x, EXISTS { WITH x WHERE x = 2 RETURN x } AS exists, COUNT { UNWIND range(1, x) AS y RETURN y } AS count, COLLECT { UNWIND range(1, x) AS y RETURN y * 10 AS value } AS collected ORDER BY x",
        ),
        vec![
            vec![
                Value::Integer(1),
                Value::Boolean(false),
                Value::Integer(1),
                Value::List(vec![Value::Integer(10)]),
            ],
            vec![
                Value::Integer(2),
                Value::Boolean(true),
                Value::Integer(2),
                Value::List(vec![Value::Integer(10), Value::Integer(20)]),
            ],
        ]
    );
}

#[test]
fn expression_subquery_outer_variables_remain_global_without_grouping() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "WITH 1 AS g RETURN COLLECT { UNWIND [1, 2, 3] AS x WITH * WHERE x < 0 WITH count(*) AS agg RETURN agg + g } AS values",
        ),
        vec![vec![Value::List(vec![Value::Integer(1)])]]
    );
    assert!(
        !query_error(
            &connection,
            "WITH 'Peter' AS name RETURN EXISTS { WITH 'Ozzy' AS name RETURN name }",
        )
        .is_empty()
    );
}

#[test]
fn star_projection_and_empty_conditional_keep_a_stable_schema() {
    let connection = fresh_storage();
    assert_eq!(
        rows(&connection, "UNWIND [1] AS x WITH *, x + 1 AS y RETURN *",),
        vec![vec![Value::Integer(1), Value::Integer(2)]]
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [] AS seed RETURN seed NEXT { WHEN true THEN { RETURN 1 AS value } ELSE { RETURN 2 AS value } }",
        ),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn variable_paths_group_variables_and_path_modes_have_distinct_cardinality() {
    let connection = fresh_storage();
    let _ = rows(
        &connection,
        "CREATE (a:N {name:'a'}), (b:N {name:'b'}), (c:N {name:'c'}), (d:N {name:'d'}) CREATE (a)-[:R {cost:1}]->(b), (b)-[:R {cost:2}]->(c), (c)-[:R {cost:3}]->(a), (a)-[:R {cost:4}]->(d), (b)-[:R {cost:5}]->(d) FINISH",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH p = (a:N {name:'a'})-[r:R]->{1,3}(b) RETURN b.name AS name, length(p) AS hops, size(r) AS relationships ORDER BY hops, name",
        ),
        vec![
            vec![
                Value::String("b".to_owned()),
                Value::Integer(1),
                Value::Integer(1),
            ],
            vec![
                Value::String("d".to_owned()),
                Value::Integer(1),
                Value::Integer(1),
            ],
            vec![
                Value::String("c".to_owned()),
                Value::Integer(2),
                Value::Integer(2),
            ],
            vec![
                Value::String("d".to_owned()),
                Value::Integer(2),
                Value::Integer(2),
            ],
            vec![
                Value::String("a".to_owned()),
                Value::Integer(3),
                Value::Integer(3),
            ],
        ]
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH p = ACYCLIC (a:N {name:'a'})-[:R]->{1,3}(b) RETURN count(p) AS paths",
        ),
        vec![vec![Value::Integer(4)]]
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH REPEATABLE ELEMENTS p = WALK (a:N {name:'a'})-[:R]->{4}(b) RETURN count(p) AS paths",
        ),
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn match_and_path_modes_enforce_frozen_profile_combinations() {
    let connection = fresh_storage();
    let _ = rows(
        &connection,
        "CREATE (a:N {name:'a'}), (b:N {name:'b'}) CREATE (a)-[:R]->(b) FINISH",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (a:N)-[r:R]->(b:N)-[r]->(c:N) RETURN count(*)",
        ),
        vec![vec![Value::Integer(0)]]
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH REPEATABLE ELEMENTS (a:N)-[r:R]->(b:N)<-[r]-(c:N) RETURN count(*)",
        ),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH p = ANY SHORTEST ACYCLIC PATH (a:N {name:'a'})-[:R]->{1,2}(b:N) RETURN length(p)",
        ),
        vec![vec![Value::Integer(1)]]
    );

    for query in [
        "MATCH REPEATABLE ELEMENTS (a)-[:R]->+(b) RETURN a",
        "MATCH REPEATABLE ELEMENTS TRAIL (a)-[:R]->{1,2}(b) RETURN a",
        "MATCH ACYCLIC (a)-[:R*1..2]->(b) RETURN a",
        "MATCH ACYCLIC (a)-->(b), TRAIL (c)-->(d) RETURN a",
        "MATCH ANY (a)-->(b), (c)-->(d) RETURN a",
        "MATCH SIMPLE (a)-->(b) RETURN a",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }
}

#[test]
fn shortest_selectors_and_quantified_pattern_predicates_filter_paths() {
    let connection = fresh_storage();
    let _ = rows(
        &connection,
        "CREATE (a:N {name:'a'}), (b:N {name:'b'}), (c:N {name:'c'}), (d:N {name:'d'}) CREATE (a)-[:R {cost:1}]->(b), (b)-[:R {cost:1}]->(d), (a)-[:R {cost:2}]->(c), (c)-[:R {cost:2}]->(d), (a)-[:R {cost:9}]->(d) FINISH",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH p = ALL SHORTEST (a:N {name:'a'})-[:R]->{1,2}(d:N {name:'d'}) RETURN length(p) AS hops",
        ),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (a:N {name:'a'}) ((x)-[r:R]->(y) WHERE r.cost < 5){2} (d:N {name:'d'}) RETURN [node IN y | node.name] AS names, [rel IN r | rel.cost] AS costs ORDER BY names[0]",
        ),
        vec![
            vec![
                Value::List(vec![
                    Value::String("b".to_owned()),
                    Value::String("d".to_owned()),
                ]),
                Value::List(vec![Value::Integer(1), Value::Integer(1)]),
            ],
            vec![
                Value::List(vec![
                    Value::String("c".to_owned()),
                    Value::String("d".to_owned()),
                ]),
                Value::List(vec![Value::Integer(2), Value::Integer(2)]),
            ],
        ]
    );
}

#[test]
fn reductions_type_predicates_and_dynamic_subscripts_cover_null_edges() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "RETURN reduce(acc = 0, x IN [1, 2, 3] | acc + x) AS total, allReduce(acc = 0, x IN [1, 2] | acc + x, acc < 4) AS bounded, allReduce(acc = 0, x IN [1, 3] | acc + x, acc < 4) AS exceeded, allReduce(acc = 0, x IN [] | acc + x, false) AS empty",
        ),
        vec![vec![
            Value::Integer(6),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
        ]]
    );
    assert_eq!(
        rows(
            &connection,
            "WITH {nested: {answer: 42}} AS value RETURN value['nested'].answer AS orderedPostfix, [1,2,3][null..2] AS nullSlice, null IS :: INTEGER AS nullableType, null IS :: INTEGER NOT NULL AS requiredType, [1,null] IS :: LIST<INTEGER> AS nullableList, [1,null] IS :: LIST<INTEGER NOT NULL> AS requiredList",
        ),
        vec![vec![
            Value::Integer(42),
            Value::Null,
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(false),
        ]]
    );
}

#[test]
fn label_and_pattern_expressions_use_the_current_graph_view() {
    let connection = fresh_storage();
    let _ = rows(
        &connection,
        "CREATE (a:Person:Visible {name:'a'}), (b:Person:Visible {name:'b'}), (hidden:Person {name:'hidden'}) CREATE (a)-[:R]->(b), (a)-[:R]->(hidden) FINISH",
    );
    let options = ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#)
        .expect("Graph View options");
    assert_eq!(
        rows_with_options(
            &connection,
            "MATCH (n:Person {name:'a'}) RETURN n['name'] AS dynamicProperty, n:Person AS staticLabel, n:$(\"Person\") AS dynamicLabel, n IS NOT LABELED Admin AS notAdmin, PROPERTY_EXISTS(n, name) AS hasName, PROPERTY_EXISTS(n, missing) AS hasMissing, exists((n)--()) AS hasOutgoing, [(n)-->(m) | m.name] AS names, [p = (n)-->(m) WHERE m.name = 'b' | length(p)] AS lengths",
            options,
        ),
        vec![vec![
            Value::String("a".to_owned()),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::List(vec![Value::String("b".to_owned())]),
            Value::List(vec![Value::Integer(1)]),
        ]]
    );
    assert!(
        query_error(&connection, "RETURN PROPERTY_EXISTS({name:'a'}, name)")
            .contains("Node or Relationship")
    );
    assert_eq!(
        rows_with_options(
            &connection,
            "MATCH (n:Person {name:'b'}) RETURN exists((n)-->())",
            ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#,)
                .expect("Graph View options"),
        ),
        vec![vec![Value::Boolean(false)]]
    );
    for query in ["RETURN exists(1)", "MATCH (n) RETURN exists(n.name)"] {
        assert!(
            query_error(&connection, query).contains("pattern expression"),
            "query must fail: {query}"
        );
    }
}

#[test]
fn graph_view_is_preserved_across_subqueries_calls_union_and_paths() {
    let connection = fresh_storage();
    let options = ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#)
        .expect("Graph View options");
    let _ = rows(
        &connection,
        "CREATE (a:Visible {name:'a'}), (b:Visible {name:'b'}), (hidden {name:'hidden'}) CREATE (a)-[:R]->(b), (b)-[:R]->(hidden) FINISH",
    );
    assert_eq!(
        rows_with_options(
            &connection,
            "MATCH (start:Visible {name:'a'}) RETURN COUNT { MATCH (start)-[:R]->+(target) RETURN target } AS reachable, COLLECT { MATCH p = ALL SHORTEST (start)-[:R]->+(target) RETURN target.name } AS names",
            options.clone(),
        ),
        vec![vec![
            Value::Integer(1),
            Value::List(vec![Value::String("b".to_owned())]),
        ]]
    );
    assert_eq!(
        rows_with_options(
            &connection,
            "UNWIND [1, 2] AS value CALL (value) { CREATE (:Visible {value:value}) } CALL { MATCH (node:Visible) RETURN count(node) AS visible } RETURN value, visible ORDER BY value",
            options.clone(),
        ),
        vec![
            vec![Value::Integer(1), Value::Integer(4)],
            vec![Value::Integer(2), Value::Integer(4)],
        ]
    );
    assert_eq!(
        rows_with_options(
            &connection,
            "{ CREATE (:Visible {value:3}) RETURN 3 AS value } UNION ALL { MATCH (node:Visible) RETURN count(node) AS value }",
            options,
        ),
        vec![vec![Value::Integer(3)], vec![Value::Integer(5)]]
    );
}
