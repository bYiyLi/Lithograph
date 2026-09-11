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
fn unsupported_pattern_semantics_are_rejected_instead_of_silently_approximated() {
    let fixture = fixture();
    for query in [
        "MATCH (n:Person|Secret) RETURN n",
        "MATCH (n:!Secret) RETURN n",
        "MATCH (n:Person {age: 41}) RETURN n",
        "MATCH (n:Person WHERE n.age > 30) RETURN n",
        "MATCH (a)-[:KNOWS*1..2]->(b) RETURN a",
        "MATCH (a)-[:KNOWS {since: 1}]->(b) RETURN a",
    ] {
        let error = prepare(
            &fixture.connection,
            query,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .expect_err("unsupported Phase 04 pattern syntax must be rejected");
        assert_eq!(error.kind, QueryErrorKind::Semantic, "query: {query}");
        assert!(error.message.contains("Phase 04"), "query: {query}");
    }
}

#[test]
fn distinct_aggregate_arguments_are_rejected_until_phase06() {
    let fixture = fixture();
    let error = prepare(
        &fixture.connection,
        "MATCH (n:Person) RETURN count(DISTINCT n.age) AS total",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect_err("count(DISTINCT ...) must not be approximated as count(...)");
    assert_eq!(error.kind, QueryErrorKind::Semantic);
    assert!(error.message.contains("DISTINCT aggregate"));
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
