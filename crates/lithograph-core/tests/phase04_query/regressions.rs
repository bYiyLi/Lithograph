use super::*;

#[test]
fn missing_parameter_is_rejected_before_execution() {
    let fixture = fixture();
    let error = prepare(
        &fixture.connection,
        "MATCH (n:Person) WHERE n.age >= $minimum RETURN n.name",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect_err("missing parameter must be rejected");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    assert!(error.message.contains("$minimum"));
}

#[test]
fn separate_match_clauses_reset_relationship_uniqueness() {
    let fixture = fixture();
    let separate = rows(
        &fixture.connection,
        "MATCH (a:Person)-[:KNOWS]->(b:Person) MATCH (c:Person)-[:KNOWS]->(d:Person) RETURN a.name AS a, b.name AS b, c.name AS c, d.name AS d ORDER BY a, b, c, d",
        ExecutionOptions::default(),
    );
    assert_eq!(separate.len(), 4);

    let same_clause = rows(
        &fixture.connection,
        "MATCH (a:Person)-[:KNOWS]->(b:Person), (c:Person)-[:KNOWS]->(d:Person) RETURN a.name AS a, b.name AS b, c.name AS c, d.name AS d ORDER BY a, b, c, d",
        ExecutionOptions::default(),
    );
    assert_eq!(same_clause.len(), 2);
}

#[test]
fn physical_plan_keeps_relationship_spec_per_expand() {
    let fixture = fixture();
    intern_relationship_type(&fixture.connection, "LIKES").expect("LIKES");
    let prepared = prepare(
        &fixture.connection,
        "MATCH (a:Person)-[:KNOWS]->(b:Person) MATCH (a)<-[:LIKES]-(c) RETURN a.name",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare query");
    let explain = prepared.physical.explain();
    assert!(explain.contains("relationship_type: Some(\"KNOWS\"), direction: Outgoing"));
    assert!(explain.contains("relationship_type: Some(\"LIKES\"), direction: Incoming"));
}

#[test]
fn order_by_expression_can_reference_projection_alias() {
    let fixture = fixture();
    let result = rows(
        &fixture.connection,
        "MATCH (n:Person) RETURN n.age AS age ORDER BY age + 2",
        ExecutionOptions::default(),
    );
    assert_eq!(
        result,
        vec![
            vec![Value::Integer(27)],
            vec![Value::Integer(34)],
            vec![Value::Integer(41)],
        ]
    );
}

#[test]
fn floating_division_by_zero_preserves_ieee_values() {
    let fixture = fixture();
    let result = rows(
        &fixture.connection,
        "RETURN 0.0 / 0.0 AS nan, 1.0 / 0.0 AS positive, -1.0 / 0.0 AS negative",
        ExecutionOptions::default(),
    );
    assert_eq!(result.len(), 1);
    assert!(matches!(result[0][0], Value::Float(value) if value.is_nan()));
    assert!(matches!(result[0][1], Value::Float(value) if value == f64::INFINITY));
    assert!(matches!(result[0][2], Value::Float(value) if value == f64::NEG_INFINITY));
}

#[test]
fn phase06_pattern_semantics_execute_instead_of_being_silently_approximated() {
    let fixture = fixture();
    for (query, expected) in [
        ("MATCH (n:Person|Secret) RETURN count(n)", 4),
        ("MATCH (n:!Secret) RETURN count(n)", 3),
        ("MATCH (n:Person {age: 41}) RETURN count(n)", 1),
        ("MATCH (n:Person WHERE n.age > 30) RETURN count(n)", 2),
        ("MATCH (a)-[:KNOWS*1..2]->(b) RETURN count(*)", 4),
        ("MATCH (a)-[:KNOWS {since: 1}]->(b) RETURN count(*)", 0),
    ] {
        assert_eq!(
            rows(&fixture.connection, query, ExecutionOptions::default()),
            vec![vec![Value::Integer(expected)]],
            "query: {query}"
        );
    }
}

#[test]
fn distinct_aggregate_arguments_execute_with_phase06_semantics() {
    let fixture = fixture();
    assert_eq!(
        rows(
            &fixture.connection,
            "MATCH (n:Person) RETURN count(DISTINCT n.age) AS total",
            ExecutionOptions::default(),
        ),
        vec![vec![Value::Integer(3)]]
    );
}

#[test]
fn query_cursor_observes_interrupt_check_at_batch_boundary() {
    let fixture = fixture();
    let prepared = prepare(
        &fixture.connection,
        "MATCH (n:Person) RETURN n.name",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare query");
    let mut cursor = QueryCursor::new(prepared);
    let error = cursor
        .next_batch_with_interrupt(&fixture.connection, 1, &|| true)
        .expect_err("interrupted host connection must stop execution");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
}

#[test]
fn doubled_string_delimiters_decode_in_values_and_map_keys() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            r#"RETURN 'it''s' AS single, "say ""yes""" AS double, {'a''b': '中''文'} AS object"#,
            ExecutionOptions::default(),
        ),
        vec![vec![
            Value::String("it's".to_owned()),
            Value::String("say \"yes\"".to_owned()),
            Value::Map(BTreeMap::from([(
                "a'b".to_owned(),
                Value::String("中'文".to_owned()),
            )])),
        ]]
    );
}

#[test]
fn map_projection_literal_entries_evaluate_property_expressions() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "WITH {score:10, name:'Ada'} AS record RETURN record{.*, score:record.score+1, label:record.name, nested:{score:record.score}} AS object",
            ExecutionOptions::default(),
        ),
        vec![vec![Value::Map(BTreeMap::from([
            ("score".to_owned(), Value::Integer(11)),
            ("name".to_owned(), Value::String("Ada".to_owned())),
            ("label".to_owned(), Value::String("Ada".to_owned())),
            (
                "nested".to_owned(),
                Value::Map(BTreeMap::from([("score".to_owned(), Value::Integer(10))])),
            ),
        ]))]]
    );
}

#[test]
fn regex_operator_parses_before_equality_and_keeps_predicate_semantics() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "RETURN 'a12' =~ 'a[0-9]+' AS matched, 'a12z' =~ 'a[0-9]+' AS whole, null =~ 'x' AS unknown, 1 = 1 AS equal, 1 <> 2 AS different",
            ExecutionOptions::default(),
        ),
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Null,
            Value::Boolean(true),
            Value::Boolean(true),
        ]]
    );
    let prepared = prepare(
        &connection,
        "RETURN 'text' =~ '[' AS invalid",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("valid Cypher containing an invalid regex");
    let mut cursor = QueryCursor::new(prepared);
    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("invalid regex must fail at evaluation");
    assert_eq!(error.kind, QueryErrorKind::Semantic);
}

#[test]
fn count_distinct_requires_a_value_expression_even_with_empty_input() {
    let connection = fresh_storage();
    for query in [
        "UNWIND [1, 1, 2] AS value RETURN count(DISTINCT *)",
        "UNWIND [] AS value RETURN count(DISTINCT *)",
        "RETURN count(DISTINCT)",
    ] {
        let error = prepare(
            &connection,
            query,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .expect_err("DISTINCT without a value expression must fail during prepare");
        assert_eq!(error.kind, QueryErrorKind::Semantic, "{query}");
    }
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1,1,2] AS value RETURN count(*) AS rows, count(DISTINCT value) AS values",
            ExecutionOptions::default(),
        ),
        vec![vec![Value::Integer(3), Value::Integer(2)]]
    );
}
