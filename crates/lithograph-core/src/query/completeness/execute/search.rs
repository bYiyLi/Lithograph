use super::*;

pub(super) struct SearchSpec {
    pub(super) variable: String,
    pub(super) index: String,
    pub(super) query: expression::Expr,
    pub(super) filter: Option<expression::Expr>,
    pub(super) limit: expression::Expr,
    pub(super) score_alias: Option<String>,
}

pub(super) fn execute_search_match_row(
    executor: &mut ReadExecutor<'_, '_>,
    clause: &AstNode,
    search: &AstNode,
    optional: bool,
    columns: &[String],
    predicate: Option<&expression::Expr>,
    input: BindingRow,
) -> QueryResult<Vec<BindingRow>> {
    let (spec, index, query, limit) = prepare_search(executor, clause, search, &input)?;
    if limit == 0 || query == Value::Null {
        return Ok(optional_search_row(optional, input, columns));
    }
    let hits = vector_candidates(
        executor.connection,
        &executor.snapshot,
        executor.graph_view,
        &index,
        &query,
        executor.is_interrupted,
    )?;
    let selected = select_search_hits(executor, &spec, &input, hits, limit)?;
    let matched = execute_search_patterns(executor, clause, selected)?;
    finish_search_match(executor, matched, predicate, optional, input, columns)
}

fn prepare_search(
    executor: &mut ReadExecutor<'_, '_>,
    clause: &AstNode,
    search: &AstNode,
    input: &BindingRow,
) -> QueryResult<(SearchSpec, IndexDefinition, Value, usize)> {
    let spec = compile_search_spec(search)?;
    let index = resolve_semantic_index(
        executor.connection,
        &executor.snapshot,
        &spec.index,
        StandardIndexKind::Vector,
    )?;
    validate_search_pattern(clause, &spec, &index)?;
    validate_search_expressions(&spec, &index)?;
    let query = executor.evaluate(&spec.query, input)?;
    let limit = search_limit(executor.evaluate(&spec.limit, input)?)?;
    Ok((spec, index, query, limit))
}

fn validate_search_expressions(spec: &SearchSpec, index: &IndexDefinition) -> QueryResult<()> {
    if expression_references_variable(&spec.query, &spec.variable) {
        return Err(QueryError::semantic(format!(
            "SEARCH query vector cannot reference binding variable {:?}",
            spec.variable
        )));
    }
    if let Some(filter) = &spec.filter {
        validate_search_filter(filter, &spec.variable, index)?;
        validate_search_filter_combinations(filter, &spec.variable)?;
    }
    Ok(())
}

fn optional_search_row(optional: bool, input: BindingRow, columns: &[String]) -> Vec<BindingRow> {
    if optional {
        vec![null_extend_row(input, columns)]
    } else {
        Vec::new()
    }
}

fn select_search_hits(
    executor: &mut ReadExecutor<'_, '_>,
    spec: &SearchSpec,
    input: &BindingRow,
    hits: Vec<SemanticHit>,
    limit: usize,
) -> QueryResult<Vec<BindingRow>> {
    let mut rows = Vec::new();
    for hit in hits {
        if let Some(row) = search_hit_row(executor, spec, input, hit)? {
            rows.push(row);
            if rows.len() >= limit {
                break;
            }
        }
    }
    Ok(rows)
}

fn search_hit_row(
    executor: &mut ReadExecutor<'_, '_>,
    spec: &SearchSpec,
    input: &BindingRow,
    hit: SemanticHit,
) -> QueryResult<Option<BindingRow>> {
    let binding = semantic_binding(hit.entity);
    if input
        .values
        .get(&spec.variable)
        .is_some_and(|existing| existing != &binding)
    {
        return Ok(None);
    }
    let mut row = input.clone();
    row.insert(spec.variable.clone(), binding);
    if let Some(filter) = &spec.filter
        && !expression::predicate(executor.evaluate(filter, &row)?)?
    {
        return Ok(None);
    }
    if let Some(alias) = &spec.score_alias {
        row.insert(alias.clone(), BindingValue::Scalar(Value::Float(hit.score)));
    }
    Ok(Some(row))
}

fn execute_search_patterns(
    executor: &mut ReadExecutor<'_, '_>,
    clause: &AstNode,
    selected: Vec<BindingRow>,
) -> QueryResult<Vec<BindingRow>> {
    let mut matched = Vec::new();
    for row in selected {
        matched.extend(super::super::path::execute_match(
            &executor.snapshot,
            executor.graph_view,
            executor.params,
            clause,
            vec![row],
            executor.metrics,
            executor.is_interrupted,
        )?);
    }
    Ok(matched)
}

fn finish_search_match(
    executor: &mut ReadExecutor<'_, '_>,
    mut matched: Vec<BindingRow>,
    predicate: Option<&expression::Expr>,
    optional: bool,
    input: BindingRow,
    columns: &[String],
) -> QueryResult<Vec<BindingRow>> {
    if let Some(predicate) = predicate {
        matched = executor.filter_rows(predicate, matched)?;
    }
    if optional && matched.is_empty() {
        Ok(vec![null_extend_row(input, columns)])
    } else {
        Ok(matched)
    }
}

pub(super) fn compile_search_spec(search: &AstNode) -> QueryResult<SearchSpec> {
    let variable = search
        .descendants()
        .find(|node| node.kind == AstKind::Variable)
        .and_then(|node| node.text.as_deref())
        .map(crate::cypher::unescape_identifier)
        .ok_or_else(|| QueryError::semantic("SEARCH is missing its binding variable"))?;
    let index = search
        .descendants()
        .find(|node| node.kind == AstKind::IndexName)
        .and_then(|node| node.text.as_deref())
        .map(crate::cypher::unescape_identifier)
        .ok_or_else(|| QueryError::semantic("SEARCH is missing its VECTOR Index name"))?;
    let expressions = surface_expressions(search);
    if !(2..=3).contains(&expressions.len()) {
        return Err(QueryError::semantic(
            "SEARCH has an invalid expression shape",
        ));
    }
    let query = compile_expression(expressions[0])?;
    let limit = compile_expression(expressions[expressions.len() - 1])?;
    let filter = if expressions.len() == 3 {
        Some(compile_expression(expressions[1])?)
    } else {
        None
    };
    let score_alias = search
        .descendants()
        .find(|node| node.kind == AstKind::ProjectionAlias)
        .and_then(|node| node.text.as_deref())
        .map(crate::cypher::unescape_identifier);
    Ok(SearchSpec {
        variable,
        index,
        query,
        filter,
        limit,
        score_alias,
    })
}

pub(super) fn validate_search_pattern(
    clause: &AstNode,
    spec: &SearchSpec,
    index: &IndexDefinition,
) -> QueryResult<()> {
    let pattern_parts = clause
        .descendants()
        .filter(|node| node.kind == AstKind::PatternPart)
        .count();
    if pattern_parts != 1 {
        return Err(QueryError::semantic(
            "SEARCH MATCH pattern must contain exactly one pattern part",
        ));
    }
    let relationship_count = clause
        .descendants()
        .filter(|node| node.kind == AstKind::RelationshipPattern)
        .count();
    if relationship_count > 1
        || clause
            .descendants()
            .any(|node| matches!(node.kind, AstKind::VariableLength | AstKind::Quantifier(_)))
    {
        return Err(QueryError::semantic(
            "SEARCH MATCH pattern cannot exceed one fixed relationship hop",
        ));
    }
    if clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::PathSelector(kind) if kind != crate::cypher::PathSelectorKind::All
        )
    }) {
        return Err(QueryError::semantic(
            "SEARCH MATCH pattern supports only the ALL path selector",
        ));
    }
    let named = clause
        .descendants()
        .filter(|node| {
            matches!(
                node.kind,
                AstKind::PatternVariable | AstKind::RelationshipVariable
            )
        })
        .filter_map(|node| node.text.as_deref())
        .map(crate::cypher::unescape_identifier)
        .collect::<BTreeSet<_>>();
    if named.len() != 1 || !named.contains(&spec.variable) {
        return Err(QueryError::semantic(
            "SEARCH MATCH pattern may name only the SEARCH binding variable",
        ));
    }
    validate_search_unbound_elements(clause, &spec.variable)?;
    let node_binding = pattern_has_node_binding(clause, &spec.variable);
    let relationship_binding = pattern_has_relationship_binding(clause, &spec.variable);
    match (&index.target, node_binding, relationship_binding) {
        (IndexTarget::NodeProperties { .. }, true, false)
        | (IndexTarget::RelationshipProperties { .. }, false, true) => Ok(()),
        (IndexTarget::NodeProperties { .. }, _, _) => Err(QueryError::semantic(format!(
            "VECTOR Index {} is a Node Index but SEARCH binding {:?} is not a Node variable",
            index.name, spec.variable
        ))),
        (IndexTarget::RelationshipProperties { .. }, _, _) => Err(QueryError::semantic(format!(
            "VECTOR Index {} is a Relationship Index but SEARCH binding {:?} is not a Relationship variable",
            index.name, spec.variable
        ))),
        _ => Err(QueryError::internal("VECTOR Index has a lookup target")),
    }
}

fn validate_search_unbound_elements(clause: &AstNode, variable: &str) -> QueryResult<()> {
    for node in clause
        .descendants()
        .filter(|node| node.kind == AstKind::NodePattern)
    {
        if pattern_node_binds(node, variable) {
            continue;
        }
        if node.descendants().any(|child| {
            matches!(
                child.kind,
                AstKind::LabelExpression
                    | AstKind::Where
                    | AstKind::Expression(ExpressionKind::Map)
            )
        }) {
            return Err(QueryError::semantic(
                "SEARCH MATCH pattern cannot predicate an unbound Node",
            ));
        }
    }
    for relationship in clause
        .descendants()
        .filter(|node| node.kind == AstKind::RelationshipPattern)
    {
        if pattern_relationship_binds(relationship, variable) {
            continue;
        }
        if relationship.descendants().any(|child| {
            matches!(
                child.kind,
                AstKind::RelationshipTypeExpression
                    | AstKind::Where
                    | AstKind::Expression(ExpressionKind::Map)
            )
        }) {
            return Err(QueryError::semantic(
                "SEARCH MATCH pattern cannot predicate an unbound Relationship",
            ));
        }
    }
    Ok(())
}

fn pattern_node_binds(node: &AstNode, variable: &str) -> bool {
    pattern_element_binds(node, AstKind::PatternVariable, variable)
}

fn pattern_relationship_binds(node: &AstNode, variable: &str) -> bool {
    pattern_element_binds(node, AstKind::RelationshipVariable, variable)
}

fn pattern_has_node_binding(clause: &AstNode, variable: &str) -> bool {
    pattern_has_binding(
        clause,
        AstKind::NodePattern,
        AstKind::PatternVariable,
        variable,
    )
}

fn pattern_has_relationship_binding(clause: &AstNode, variable: &str) -> bool {
    pattern_has_binding(
        clause,
        AstKind::RelationshipPattern,
        AstKind::RelationshipVariable,
        variable,
    )
}

fn pattern_has_binding(
    clause: &AstNode,
    element_kind: AstKind,
    variable_kind: AstKind,
    variable: &str,
) -> bool {
    clause.descendants().any(|node| {
        node.kind == element_kind && pattern_element_binds(node, variable_kind.clone(), variable)
    })
}

fn pattern_element_binds(node: &AstNode, variable_kind: AstKind, variable: &str) -> bool {
    node.descendants().any(|child| {
        child.kind == variable_kind
            && child
                .text
                .as_deref()
                .map(crate::cypher::unescape_identifier)
                .as_deref()
                == Some(variable)
    })
}

pub(super) fn validate_search_filter(
    expression: &expression::Expr,
    variable: &str,
    index: &IndexDefinition,
) -> QueryResult<()> {
    use expression::{BinaryOp, Expr, UnaryOp};
    match expression {
        Expr::Binary(BinaryOp::And, left, right) => {
            validate_search_filter(left, variable, index)?;
            validate_search_filter(right, variable, index)
        }
        Expr::Binary(
            BinaryOp::Equal
            | BinaryOp::Less
            | BinaryOp::LessEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterEqual,
            left,
            right,
        ) => validate_search_comparison(left, right, variable, index),
        Expr::Binary(BinaryOp::In, left, right) => {
            require_search_property(left, variable, index)?;
            if expression_references_variable(right, variable) {
                return Err(QueryError::semantic(
                    "SEARCH filter value cannot reference the SEARCH binding variable",
                ));
            }
            Ok(())
        }
        Expr::Property(_, _) => require_search_property(expression, variable, index).map(|_| ()),
        Expr::Unary(UnaryOp::Not, inner) => {
            require_search_property(inner, variable, index).map(|_| ())
        }
        Expr::IsNull { value, .. } => require_search_property(value, variable, index).map(|_| ()),
        _ => Err(QueryError::semantic(
            "SEARCH WHERE supports only additional-property predicates joined by AND",
        )),
    }
}

fn validate_search_comparison(
    left: &expression::Expr,
    right: &expression::Expr,
    variable: &str,
    index: &IndexDefinition,
) -> QueryResult<()> {
    let left_property = search_property(left, variable);
    let right_property = search_property(right, variable);
    match (left_property, right_property) {
        (Some(property), None) => {
            ensure_search_property(index, property)?;
            reject_search_binding_reference(right, variable)
        }
        (None, Some(property)) => {
            ensure_search_property(index, property)?;
            reject_search_binding_reference(left, variable)
        }
        _ => Err(QueryError::semantic(
            "SEARCH filter comparison must compare one additional property with an independent value",
        )),
    }
}

fn reject_search_binding_reference(
    expression: &expression::Expr,
    variable: &str,
) -> QueryResult<()> {
    if expression_references_variable(expression, variable) {
        Err(QueryError::semantic(
            "SEARCH filter value cannot reference the SEARCH binding variable",
        ))
    } else {
        Ok(())
    }
}

fn require_search_property<'a>(
    expression: &'a expression::Expr,
    variable: &str,
    index: &IndexDefinition,
) -> QueryResult<&'a str> {
    let property = search_property(expression, variable).ok_or_else(|| {
        QueryError::semantic("SEARCH filter predicate must reference the SEARCH binding variable")
    })?;
    ensure_search_property(index, property)?;
    Ok(property)
}

fn search_property<'a>(expression: &'a expression::Expr, variable: &str) -> Option<&'a str> {
    match expression {
        expression::Expr::Property(base, property) if matches!(base.as_ref(), expression::Expr::Variable(name) if name == variable) => {
            Some(property)
        }
        _ => None,
    }
}

fn ensure_search_property(index: &IndexDefinition, property: &str) -> QueryResult<()> {
    if index
        .additional_properties
        .iter()
        .any(|candidate| candidate == property)
    {
        Ok(())
    } else {
        Err(QueryError::semantic(format!(
            "property {property:?} is not an additional filter property of VECTOR Index {}",
            index.name
        )))
    }
}

#[derive(Default)]
struct SearchRangeUse {
    equality: usize,
    lower: usize,
    upper: usize,
}

pub(super) fn validate_search_filter_combinations(
    expression: &expression::Expr,
    variable: &str,
) -> QueryResult<()> {
    let mut uses = BTreeMap::<String, SearchRangeUse>::new();
    collect_search_filter_uses(expression, variable, &mut uses);
    for (property, usage) in uses {
        if usage.lower > 1 || usage.upper > 1 {
            return Err(QueryError::semantic(format!(
                "SEARCH filter cannot apply multiple range predicates in the same direction to property {property:?}"
            )));
        }
        if usage.equality > 0 && usage.lower.saturating_add(usage.upper) > 0 {
            return Err(QueryError::semantic(format!(
                "SEARCH filter cannot combine equality and range predicates for property {property:?}"
            )));
        }
    }
    Ok(())
}

fn collect_search_filter_uses(
    expression: &expression::Expr,
    variable: &str,
    uses: &mut BTreeMap<String, SearchRangeUse>,
) {
    use expression::{BinaryOp, Expr};
    match expression {
        Expr::Binary(BinaryOp::And, left, right) => {
            collect_search_filter_uses(left, variable, uses);
            collect_search_filter_uses(right, variable, uses);
        }
        Expr::Binary(op, left, right)
            if matches!(
                op,
                BinaryOp::Equal
                    | BinaryOp::Less
                    | BinaryOp::LessEqual
                    | BinaryOp::Greater
                    | BinaryOp::GreaterEqual
            ) =>
        {
            if let Some(property) = search_property(left, variable) {
                record_search_range_use(uses, property, *op, true);
            } else if let Some(property) = search_property(right, variable) {
                record_search_range_use(uses, property, *op, false);
            }
        }
        _ => {}
    }
}

fn record_search_range_use(
    uses: &mut BTreeMap<String, SearchRangeUse>,
    property: &str,
    operator: expression::BinaryOp,
    property_on_left: bool,
) {
    use expression::BinaryOp;
    let usage = uses.entry(property.to_owned()).or_default();
    match operator {
        BinaryOp::Equal => usage.equality = usage.equality.saturating_add(1),
        BinaryOp::Greater | BinaryOp::GreaterEqual if property_on_left => {
            usage.lower = usage.lower.saturating_add(1);
        }
        BinaryOp::Less | BinaryOp::LessEqual if property_on_left => {
            usage.upper = usage.upper.saturating_add(1);
        }
        BinaryOp::Greater | BinaryOp::GreaterEqual => {
            usage.upper = usage.upper.saturating_add(1);
        }
        BinaryOp::Less | BinaryOp::LessEqual => {
            usage.lower = usage.lower.saturating_add(1);
        }
        _ => {}
    }
}

pub(super) fn expression_references_variable(
    expression: &expression::Expr,
    variable: &str,
) -> bool {
    use expression::{Expr, InterpolatedPart};
    match expression {
        Expr::Variable(name) => name == variable,
        Expr::Literal(_) | Expr::Parameter(_) => false,
        Expr::List(values) => values
            .iter()
            .any(|value| expression_references_variable(value, variable)),
        Expr::Map(entries) => entries
            .values()
            .any(|value| expression_references_variable(value, variable)),
        Expr::Property(base, _) => expression_references_variable(base, variable),
        Expr::Function { args, .. } => args
            .iter()
            .any(|value| expression_references_variable(value, variable)),
        Expr::IsNull { value, .. }
        | Expr::NormalizedPredicate { value, .. }
        | Expr::TypePredicate { value, .. }
        | Expr::LabelPredicate { value, .. }
        | Expr::Unary(_, value) => expression_references_variable(value, variable),
        Expr::PatternPredicate(node)
        | Expr::PatternComprehension(node)
        | Expr::Subquery { node, .. } => node.descendants().any(|node| {
            node.kind == AstKind::Variable
                && node
                    .text
                    .as_deref()
                    .map(crate::cypher::unescape_identifier)
                    .as_deref()
                    == Some(variable)
        }),
        Expr::Interpolated(parts) => parts.iter().any(|part| match part {
            InterpolatedPart::Text(_) => false,
            InterpolatedPart::Expression(value) => expression_references_variable(value, variable),
        }),
        Expr::Binary(_, left, right) => {
            expression_references_variable(left, variable)
                || expression_references_variable(right, variable)
        }
        _ => nested_expression_references_variable(expression, variable),
    }
}

fn nested_expression_references_variable(expression: &expression::Expr, variable: &str) -> bool {
    use expression::Expr;
    match expression {
        Expr::Case {
            operand,
            alternatives,
            fallback,
        } => case_references_variable(
            operand.as_deref(),
            alternatives,
            fallback.as_deref(),
            variable,
        ),
        Expr::ListComprehension {
            collection,
            predicate,
            projection,
            ..
        } => collection_expression_references_variable(
            collection,
            predicate.as_deref(),
            projection.as_deref(),
            variable,
        ),
        Expr::ListPredicate {
            collection,
            predicate,
            ..
        } => collection_expression_references_variable(
            collection,
            predicate.as_deref(),
            None,
            variable,
        ),
        Expr::Reduce {
            initial,
            collection,
            reduction,
            ..
        } => reduction_references_variable(initial, collection, reduction, None, variable),
        Expr::AllReduce {
            initial,
            collection,
            reduction,
            predicate,
            ..
        } => {
            reduction_references_variable(initial, collection, reduction, Some(predicate), variable)
        }
        Expr::MapProjection { base, entries, .. } => {
            base == variable
                || entries
                    .iter()
                    .any(|(_, value)| expression_references_variable(value, variable))
        }
        Expr::Subscript {
            base, start, end, ..
        } => {
            expression_references_variable(base, variable)
                || optional_expression_references_variable(start.as_deref(), variable)
                || optional_expression_references_variable(end.as_deref(), variable)
        }
        _ => false,
    }
}

fn case_references_variable(
    operand: Option<&expression::Expr>,
    alternatives: &[(expression::Expr, expression::Expr)],
    fallback: Option<&expression::Expr>,
    variable: &str,
) -> bool {
    optional_expression_references_variable(operand, variable)
        || alternatives.iter().any(|(when, then)| {
            expression_references_variable(when, variable)
                || expression_references_variable(then, variable)
        })
        || optional_expression_references_variable(fallback, variable)
}

fn collection_expression_references_variable(
    collection: &expression::Expr,
    predicate: Option<&expression::Expr>,
    projection: Option<&expression::Expr>,
    variable: &str,
) -> bool {
    expression_references_variable(collection, variable)
        || optional_expression_references_variable(predicate, variable)
        || optional_expression_references_variable(projection, variable)
}

fn reduction_references_variable(
    initial: &expression::Expr,
    collection: &expression::Expr,
    reduction: &expression::Expr,
    predicate: Option<&expression::Expr>,
    variable: &str,
) -> bool {
    expression_references_variable(initial, variable)
        || expression_references_variable(collection, variable)
        || expression_references_variable(reduction, variable)
        || optional_expression_references_variable(predicate, variable)
}

fn optional_expression_references_variable(
    expression: Option<&expression::Expr>,
    variable: &str,
) -> bool {
    expression.is_some_and(|value| expression_references_variable(value, variable))
}

pub(super) fn search_limit(value: Value) -> QueryResult<usize> {
    let Value::Integer(limit) = value else {
        return Err(QueryError::semantic("SEARCH LIMIT requires an Integer"));
    };
    if !(0..=i64::from(i32::MAX)).contains(&limit) {
        return Err(QueryError::semantic(format!(
            "SEARCH LIMIT {limit} is outside 0..={} ",
            i32::MAX
        )));
    }
    usize::try_from(limit).map_err(|_| QueryError::semantic("SEARCH LIMIT is too large"))
}

pub(super) fn semantic_binding(entity: SemanticEntity) -> BindingValue {
    match entity {
        SemanticEntity::Node(node) => BindingValue::Node(node),
        SemanticEntity::Relationship(relationship) => BindingValue::Relationship(relationship),
    }
}
