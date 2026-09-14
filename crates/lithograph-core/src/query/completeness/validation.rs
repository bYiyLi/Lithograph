use std::collections::BTreeMap;

use crate::cypher::{AstKind, AstNode, ClauseKind, Value};

use super::super::expression::{
    Expr, compile_expression, surface_expressions_without_nested_queries, transform_expression,
};
use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::{direct_projection_items, has_surface_star, projection_body, surface_expressions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaticPropertyBase {
    Structural,
    Invalid,
    Unknown,
}

pub(super) fn has_graph_expression(node: &AstNode, expression_context: bool) -> bool {
    if expression_context && node.kind == AstKind::Pattern {
        return true;
    }
    let expression_context = expression_context || matches!(node.kind, AstKind::Expression(_));
    node.children
        .iter()
        .any(|child| has_graph_expression(child, expression_context))
}

pub(super) fn validate_static_property_accesses(root: &AstNode) -> QueryResult<()> {
    for query in root
        .descendants()
        .filter(|node| node.kind == AstKind::SingleQuery)
    {
        validate_single_query_static_property_accesses(query)?;
    }
    Ok(())
}

fn validate_single_query_static_property_accesses(query: &AstNode) -> QueryResult<()> {
    let mut aliases = BTreeMap::<String, StaticPropertyBase>::new();
    for clause in query
        .children
        .iter()
        .filter(|node| matches!(node.kind, AstKind::Clause(_)))
    {
        for expression in surface_expressions_without_nested_queries(clause) {
            validate_static_property_expression(&compile_expression(expression)?, &aliases)?;
        }
        if clause.kind == AstKind::Clause(ClauseKind::With) {
            aliases = static_projection_aliases(clause, &aliases)?;
        }
    }
    Ok(())
}

fn static_projection_aliases(
    clause: &AstNode,
    input: &BTreeMap<String, StaticPropertyBase>,
) -> QueryResult<BTreeMap<String, StaticPropertyBase>> {
    let body = projection_body(clause)?;
    let mut output = if has_surface_star(body) {
        input.clone()
    } else {
        BTreeMap::new()
    };
    for item in direct_projection_items(body) {
        let Some(alias) = item
            .descendants()
            .find(|node| node.kind == AstKind::ProjectionAlias)
            .and_then(|node| node.text.clone())
        else {
            continue;
        };
        let Some(value) = surface_expressions(item).into_iter().next() else {
            continue;
        };
        output.insert(
            alias,
            static_property_base(&compile_expression(value)?, input),
        );
    }
    Ok(output)
}

fn validate_static_property_expression(
    expression: &Expr,
    aliases: &BTreeMap<String, StaticPropertyBase>,
) -> QueryResult<()> {
    let _ = transform_expression(expression, &mut |candidate| {
        if let Expr::Property(base, _) = candidate
            && static_property_base(base, aliases) == StaticPropertyBase::Invalid
        {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "property access requires a Map, Node, Relationship, or null",
            ));
        }
        Ok(None)
    })?;
    Ok(())
}

fn static_property_base(
    expression: &Expr,
    aliases: &BTreeMap<String, StaticPropertyBase>,
) -> StaticPropertyBase {
    match expression {
        Expr::Variable(name) => aliases
            .get(name)
            .copied()
            .unwrap_or(StaticPropertyBase::Unknown),
        Expr::Literal(Value::Null) => StaticPropertyBase::Structural,
        Expr::Literal(Value::Map(_)) | Expr::Map(_) | Expr::MapProjection { .. } => {
            StaticPropertyBase::Structural
        }
        Expr::Literal(
            Value::Boolean(_)
            | Value::Integer(_)
            | Value::Float(_)
            | Value::String(_)
            | Value::List(_)
            | Value::Date(_)
            | Value::LocalTime(_)
            | Value::Time(_)
            | Value::LocalDateTime(_)
            | Value::ZonedDateTime(_)
            | Value::Duration(_)
            | Value::Point(_)
            | Value::Vector(_)
            | Value::Uuid(_),
        )
        | Expr::List(_) => StaticPropertyBase::Invalid,
        Expr::Literal(Value::Node(_) | Value::Relationship(_) | Value::Path(_)) => {
            StaticPropertyBase::Structural
        }
        Expr::Subscript { base, .. } => static_list_element_base(base, aliases),
        _ => StaticPropertyBase::Unknown,
    }
}

fn static_list_element_base(
    base: &Expr,
    aliases: &BTreeMap<String, StaticPropertyBase>,
) -> StaticPropertyBase {
    let Expr::List(values) = base else {
        return StaticPropertyBase::Unknown;
    };
    let mut values = values
        .iter()
        .map(|value| static_property_base(value, aliases));
    let Some(first) = values.next() else {
        return StaticPropertyBase::Unknown;
    };
    if values.all(|value| value == first) {
        first
    } else {
        StaticPropertyBase::Unknown
    }
}

pub(super) fn query_body_has_public_result(node: &AstNode) -> bool {
    crate::cypher::query_body_terminal_matches(node, single_query_has_public_result)
}

fn single_query_has_public_result(node: &AstNode) -> bool {
    node.children
        .iter()
        .rev()
        .find_map(|child| match child.kind {
            AstKind::Clause(kind) => Some(kind),
            _ => None,
        })
        .is_some_and(|kind| {
            matches!(
                kind,
                ClauseKind::Return | ClauseKind::Show | ClauseKind::Call
            )
        })
}
