use std::collections::BTreeSet;

use crate::cypher::{AstKind, AstNode, NameExpressionKind, Value, unescape_identifier};

use super::resources::{ExecutionControl, allocation_bytes};
use super::{QueryError, QueryErrorKind, QueryResult};

pub(crate) fn matches_controlled(
    node: &AstNode,
    names: &BTreeSet<String>,
    mut evaluate_dynamic: impl FnMut(&AstNode) -> QueryResult<Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    matches_inner(node, names, &mut evaluate_dynamic, control)
}

fn matches_inner(
    node: &AstNode,
    names: &BTreeSet<String>,
    evaluate_dynamic: &mut impl FnMut(&AstNode) -> QueryResult<Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    if let Some(control) = control {
        control.work(1)?;
    }
    match node.kind {
        AstKind::LabelExpression | AstKind::RelationshipTypeExpression => {
            matches_nested(node, names, false, evaluate_dynamic, control)
        }
        AstKind::NameExpression(NameExpressionKind::Disjunction) => {
            matches_nested(node, names, true, evaluate_dynamic, control)
        }
        AstKind::NameExpression(NameExpressionKind::Conjunction) => {
            matches_nested(node, names, false, evaluate_dynamic, control)
        }
        AstKind::NameExpression(NameExpressionKind::Negation(count)) => {
            let child = nested_name_expressions(node)
                .next()
                .ok_or_else(|| QueryError::semantic("name negation is missing its operand"))?;
            let matched = matches_inner(child, names, evaluate_dynamic, control)?;
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
                matches_inner(child, names, evaluate_dynamic, control)
            }),
        AstKind::NameExpression(NameExpressionKind::Dynamic) => {
            let expression = node
                .children
                .iter()
                .find(|child| matches!(child.kind, AstKind::Expression(_)))
                .ok_or_else(|| QueryError::semantic("dynamic name is missing its expression"))?;
            dynamic_matches(evaluate_dynamic(expression)?, names, control)
        }
        AstKind::NameExpression(NameExpressionKind::Wildcard) => Ok(!names.is_empty()),
        AstKind::LabelName | AstKind::RelationshipTypeName => {
            matches_named(node, names, evaluate_dynamic, control)
        }
        _ => Ok(false),
    }
}

fn matches_named(
    node: &AstNode,
    names: &BTreeSet<String>,
    evaluate_dynamic: &mut impl FnMut(&AstNode) -> QueryResult<Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    if let Some(dynamic) = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::NameExpression(NameExpressionKind::Dynamic))
    {
        return matches_inner(dynamic, names, evaluate_dynamic, control);
    }
    let Some(name) = node.text.as_deref() else {
        return Ok(false);
    };
    let _name = control
        .map(|control| control.reserve(name.len() as u64))
        .transpose()?;
    if let Some(control) = control {
        control.check()?;
    }
    let result = names.contains(&unescape_identifier(name));
    if let Some(control) = control {
        control.check()?;
    }
    Ok(result)
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
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    for child in nested_name_expressions(node) {
        if matches_inner(child, names, evaluate_dynamic, control)? == short_circuit_on {
            return Ok(short_circuit_on);
        }
    }
    Ok(!short_circuit_on)
}

fn dynamic_matches(
    value: Value,
    names: &BTreeSet<String>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    let count = match &value {
        Value::String(_) => 1,
        Value::List(values) => values.len(),
        _ => 0,
    };
    let _memory = control
        .map(|control| control.reserve(allocation_bytes::<String>(count)?))
        .transpose()?;
    let expected = dynamic_names(value, control)?;
    for name in expected {
        if let Some(control) = control {
            control.work(1)?;
        }
        if !names.contains(&name) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn dynamic_names(value: Value, control: Option<&ExecutionControl<'_>>) -> QueryResult<Vec<String>> {
    let names = match value {
        Value::String(value) => vec![value],
        Value::List(values) => {
            let mut names = Vec::with_capacity(values.len());
            for value in values {
                if let Some(control) = control {
                    control.work(1)?;
                }
                let Value::String(value) = value else {
                    return Err(QueryError::new(
                        QueryErrorKind::Type,
                        "dynamic label/type list must contain only non-null Strings",
                    ));
                };
                names.push(value);
            }
            names
        }
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "dynamic label/type expression must be a non-null String or List<String>",
            ));
        }
    };
    for name in &names {
        if let Some(control) = control {
            control.work(1)?;
        }
        if name.is_empty() {
            return Err(QueryError::semantic(
                "dynamic label/type names must not be empty",
            ));
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::resources::QueryResources;
    use std::cell::Cell;

    #[test]
    fn dynamic_names_reserve_output_slots_before_conversion() {
        let resources = QueryResources::new(Some(1024));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let value = Value::List((0..1000).map(|_| Value::String("x".into())).collect());
        let error = dynamic_matches(value, &BTreeSet::new(), Some(&control)).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn dynamic_name_conversion_is_cancelable_and_validates_before_matching() {
        let resources = QueryResources::default();
        let checks = Cell::new(0);
        let interrupted = || {
            checks.set(checks.get() + 1);
            checks.get() == 2
        };
        let control = ExecutionControl::new(&resources, &interrupted);
        let value = Value::List((0..10_000).map(|_| Value::String("x".into())).collect());
        let error = dynamic_matches(value, &BTreeSet::new(), Some(&control)).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(checks.get(), 2);
        assert_eq!(resources.used(), 0);
        let error = dynamic_matches(
            Value::List(vec![Value::String("missing".into()), Value::Null]),
            &BTreeSet::new(),
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Type);
    }
}
