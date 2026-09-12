use std::collections::BTreeSet;

use crate::cypher::{AstKind, AstNode, NameExpressionKind, Value};

use super::{QueryError, QueryErrorKind, QueryResult};

pub(crate) fn matches(
    node: &AstNode,
    names: &BTreeSet<String>,
    mut evaluate_dynamic: impl FnMut(&AstNode) -> QueryResult<Value>,
) -> QueryResult<bool> {
    matches_inner(node, names, &mut evaluate_dynamic)
}

fn matches_inner(
    node: &AstNode,
    names: &BTreeSet<String>,
    evaluate_dynamic: &mut impl FnMut(&AstNode) -> QueryResult<Value>,
) -> QueryResult<bool> {
    match node.kind {
        AstKind::LabelExpression | AstKind::RelationshipTypeExpression => {
            matches_nested(node, names, false, evaluate_dynamic)
        }
        AstKind::NameExpression(NameExpressionKind::Disjunction) => {
            matches_nested(node, names, true, evaluate_dynamic)
        }
        AstKind::NameExpression(NameExpressionKind::Conjunction) => {
            matches_nested(node, names, false, evaluate_dynamic)
        }
        AstKind::NameExpression(NameExpressionKind::Negation(count)) => {
            let child = nested_name_expressions(node)
                .next()
                .ok_or_else(|| QueryError::semantic("name negation is missing its operand"))?;
            let matched = matches_inner(child, names, evaluate_dynamic)?;
            Ok(if count % 2 == 0 { matched } else { !matched })
        }
        AstKind::NameExpression(NameExpressionKind::Atom) => node
            .children
            .iter()
            .find(|child| {
                matches!(
                    child.kind,
                    AstKind::LabelName | AstKind::RelationshipTypeName | AstKind::NameExpression(_)
                )
            })
            .map_or(Ok(false), |child| {
                matches_inner(child, names, evaluate_dynamic)
            }),
        AstKind::NameExpression(NameExpressionKind::Dynamic) => {
            let expression = node
                .children
                .iter()
                .find(|child| matches!(child.kind, AstKind::Expression(_)))
                .ok_or_else(|| QueryError::semantic("dynamic name is missing its expression"))?;
            dynamic_names(evaluate_dynamic(expression)?)
                .map(|expected| expected.iter().all(|name| names.contains(name)))
        }
        AstKind::NameExpression(NameExpressionKind::Wildcard) => Ok(!names.is_empty()),
        AstKind::LabelName | AstKind::RelationshipTypeName => Ok(node
            .text
            .as_deref()
            .is_some_and(|name| names.contains(name))),
        _ => Ok(false),
    }
}

fn nested_name_expressions(node: &AstNode) -> impl Iterator<Item = &AstNode> {
    node.children
        .iter()
        .filter(|child| matches!(child.kind, AstKind::NameExpression(_)))
}

fn matches_nested(
    node: &AstNode,
    names: &BTreeSet<String>,
    short_circuit_on: bool,
    evaluate_dynamic: &mut impl FnMut(&AstNode) -> QueryResult<Value>,
) -> QueryResult<bool> {
    for child in nested_name_expressions(node) {
        if matches_inner(child, names, evaluate_dynamic)? == short_circuit_on {
            return Ok(short_circuit_on);
        }
    }
    Ok(!short_circuit_on)
}

fn dynamic_names(value: Value) -> QueryResult<Vec<String>> {
    let names = match value {
        Value::String(value) => vec![value],
        Value::List(values) => values
            .into_iter()
            .map(|value| match value {
                Value::String(value) => Ok(value),
                _ => Err(QueryError::new(
                    QueryErrorKind::Type,
                    "dynamic label/type list must contain only non-null Strings",
                )),
            })
            .collect::<QueryResult<Vec<_>>>()?,
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "dynamic label/type expression must be a non-null String or List<String>",
            ));
        }
    };
    if names.iter().any(String::is_empty) {
        return Err(QueryError::semantic(
            "dynamic label/type names must not be empty",
        ));
    }
    Ok(names)
}
