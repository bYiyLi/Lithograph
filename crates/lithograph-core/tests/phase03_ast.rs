use lithograph_core::cypher::{
    AstKind, ConstraintKind, ExistenceModifierKind, GraphTypeOperationKind, MatchModeKind,
    MergeActionKind, NameExpressionKind, OrderDirectionKind, QueryConnector, QueryOptionValue,
    SetOperatorKind, SetQuantifierKind, ShowTargetKind, TransactionDisjointKind,
    TransactionErrorKind, parse,
};

#[test]
fn ast_preserves_phase03_semantic_discriminators() {
    let ast = parse(
        "MATCH REPEATABLE ELEMENTS (n) RETURN ALL count(DISTINCT n) AS total ORDER BY total DESC",
    )
    .expect("parse semantic discriminators");
    let nodes = ast.root.descendants().collect::<Vec<_>>();
    assert!(
        nodes
            .iter()
            .any(|node| { node.kind == AstKind::MatchMode(MatchModeKind::RepeatableElements) })
    );
    assert!(
        nodes
            .iter()
            .any(|node| { node.kind == AstKind::SetQuantifier(SetQuantifierKind::All) })
    );
    assert!(
        nodes
            .iter()
            .any(|node| { node.kind == AstKind::SetQuantifier(SetQuantifierKind::Distinct) })
    );
    assert!(
        nodes
            .iter()
            .any(|node| { node.kind == AstKind::OrderDirection(OrderDirectionKind::Descending) })
    );
}

#[test]
fn ast_preserves_query_options_mutation_and_transaction_discriminators() {
    let ast = parse(
        "PROFILE CYPHER runtime=slotted MERGE (n:Person {id: 1}) ON CREATE SET n.created = 1 ON MATCH SET n += {seen: true}",
    )
    .expect("parse query options and mutation discriminators");
    assert_eq!(ast.query_options.len(), 1);
    assert_eq!(ast.query_options[0].name, "runtime");
    assert_eq!(
        ast.query_options[0].value,
        QueryOptionValue::Identifier("slotted".to_owned())
    );
    let nodes = ast.root.descendants().collect::<Vec<_>>();
    assert!(
        nodes
            .iter()
            .any(|node| node.kind == AstKind::MergeAction(MergeActionKind::Create))
    );
    assert!(
        nodes
            .iter()
            .any(|node| node.kind == AstKind::MergeAction(MergeActionKind::Match))
    );
    assert!(
        nodes
            .iter()
            .any(|node| node.kind == AstKind::SetOperator(SetOperatorKind::Assign))
    );
    assert!(
        nodes
            .iter()
            .any(|node| node.kind == AstKind::SetOperator(SetOperatorKind::AddAssign))
    );

    let ast =
        parse("CALL { RETURN 1 AS x } IN TRANSACTIONS ON ERROR RETRY 1 SEC THEN BREAK RETURN x")
            .expect("parse retry fallback");
    assert!(ast.root.descendants().any(|node| {
        node.kind == AstKind::TransactionRetryFallback(TransactionErrorKind::Break)
    }));

    for (query, expected) in [
        (
            "CYPHER option=1 RETURN 1 AS x",
            QueryOptionValue::IntegerLiteral("1".to_owned()),
        ),
        (
            "CYPHER option=1.5 RETURN 1 AS x",
            QueryOptionValue::FloatLiteral("1.5".to_owned()),
        ),
        (
            "CYPHER option='value' RETURN 1 AS x",
            QueryOptionValue::StringLiteral("'value'".to_owned()),
        ),
        (
            "CYPHER option=value RETURN 1 AS x",
            QueryOptionValue::Identifier("value".to_owned()),
        ),
    ] {
        let ast = parse(query).unwrap_or_else(|error| panic!("{query}: {error}"));
        assert_eq!(ast.query_options[0].value, expected, "{query}");
    }
}

#[test]
fn ast_preserves_pattern_schema_and_ingestion_discriminators() {
    let ast = parse("MATCH (n:!Person&%)-[:!KNOWS|$($kind)]->() RETURN n")
        .expect("parse label and relationship type expressions");
    let nodes = ast.root.descendants().collect::<Vec<_>>();
    assert!(
        nodes
            .iter()
            .any(|node| node.kind == AstKind::LabelExpression)
    );
    assert!(
        nodes
            .iter()
            .any(|node| { node.kind == AstKind::NameExpression(NameExpressionKind::Negation(1)) })
    );
    assert!(
        nodes
            .iter()
            .any(|node| { node.kind == AstKind::NameExpression(NameExpressionKind::Wildcard) })
    );
    assert!(
        nodes
            .iter()
            .any(|node| { node.kind == AstKind::NameExpression(NameExpressionKind::Dynamic) })
    );

    let ast = parse(
        "CREATE VECTOR INDEX `vec idx` IF NOT EXISTS FOR (n:Doc) ON EACH [n.embedding] WITH [n.lang]",
    )
    .expect("parse index discriminators");
    let nodes = ast.root.descendants().collect::<Vec<_>>();
    assert!(nodes.iter().any(|node| {
        node.kind == AstKind::IndexName && node.text.as_deref() == Some("`vec idx`")
    }));
    assert!(nodes.iter().any(|node| {
        node.kind == AstKind::ExistenceModifier(ExistenceModifierKind::IfNotExists)
    }));
    assert!(
        nodes
            .iter()
            .any(|node| node.kind == AstKind::IndexTargetEach)
    );

    let ast = parse("ALTER CURRENT GRAPH TYPE ADD { (p:Person => {id :: INTEGER IS NODE KEY}) }")
        .expect("parse graph type discriminators");
    let nodes = ast.root.descendants().collect::<Vec<_>>();
    assert!(
        nodes
            .iter()
            .any(|node| { node.kind == AstKind::GraphTypeOperation(GraphTypeOperationKind::Add) })
    );
    assert!(
        nodes
            .iter()
            .any(|node| { node.kind == AstKind::GraphAlias && node.text.as_deref() == Some("p") })
    );
    assert!(
        nodes
            .iter()
            .any(|node| { node.kind == AstKind::ConstraintKind(ConstraintKind::NodeKey) })
    );

    let ast = parse("SHOW CURRENT GRAPH TYPE AS GRAPH YIELD nodes RETURN nodes")
        .expect("parse show graph target");
    assert!(
        ast.root
            .descendants()
            .any(|node| { node.kind == AstKind::ShowTarget(ShowTargetKind::CurrentGraphType) })
    );
    assert!(
        ast.root
            .descendants()
            .any(|node| node.kind == AstKind::ShowAsGraph)
    );

    let ast = parse(
        "LOAD CSV WITH HEADERS FROM 'file:///rows.csv' AS row FIELDTERMINATOR ';' RETURN row",
    )
    .expect("parse load csv discriminators");
    assert!(
        ast.root
            .descendants()
            .any(|node| node.kind == AstKind::LoadCsvHeaders)
    );
    assert!(
        ast.root
            .descendants()
            .any(|node| node.kind == AstKind::LoadCsvFieldTerminator)
    );
}

#[test]
fn typed_discriminators_ignore_layout_and_comments() {
    let ast = parse("{ RETURN 1 AS x } UNION /* gap */ ALL { RETURN 2 AS x }")
        .expect("parse commented UNION ALL");
    assert!(
        ast.root
            .descendants()
            .any(|node| node.kind == AstKind::Connector(QueryConnector::UnionAll))
    );

    let ast = parse("MATCH DIFFERENT /* gap */ RELATIONSHIPS (n)-->(m) RETURN n")
        .expect("parse commented match mode");
    assert!(
        ast.root
            .descendants()
            .any(|node| { node.kind == AstKind::MatchMode(MatchModeKind::DifferentRelationships) })
    );

    let ast = parse("MERGE (n) ON /* gap */ CREATE SET n.created = true")
        .expect("parse commented merge action");
    assert!(
        ast.root
            .descendants()
            .any(|node| node.kind == AstKind::MergeAction(MergeActionKind::Create))
    );

    let ast = parse(
        "CALL { RETURN 1 AS x } IN CONCURRENT TRANSACTIONS DISJOINT BY /* gap */ NONE ON ERROR /* gap */ RETRY 1 SEC THEN /* gap */ CONTINUE RETURN x",
    )
    .expect("parse commented transaction discriminators");
    assert!(
        ast.root.descendants().any(|node| {
            node.kind == AstKind::TransactionDisjoint(TransactionDisjointKind::None)
        })
    );
    assert!(
        ast.root
            .descendants()
            .any(|node| { node.kind == AstKind::TransactionError(TransactionErrorKind::Retry) })
    );
    assert!(ast.root.descendants().any(|node| {
        node.kind == AstKind::TransactionRetryFallback(TransactionErrorKind::Continue)
    }));

    let ast =
        parse("CREATE CONSTRAINT person_key FOR (n:Person) REQUIRE n.id IS NODE /* gap */ KEY")
            .expect("parse commented constraint kind");
    assert!(
        ast.root
            .descendants()
            .any(|node| { node.kind == AstKind::ConstraintKind(ConstraintKind::NodeKey) })
    );

    let ast = parse("SHOW CURRENT /* gap */ GRAPH TYPE").expect("parse commented SHOW target");
    assert!(
        ast.root
            .descendants()
            .any(|node| { node.kind == AstKind::ShowTarget(ShowTargetKind::CurrentGraphType) })
    );
}
