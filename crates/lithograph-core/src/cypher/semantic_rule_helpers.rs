use super::ast::{AstKind, AstNode, ExpressionKind};
use super::semantic::unescape_identifier;
use super::semantic_expression::function_name;

pub(super) fn top_level_argument_expressions(node: &AstNode) -> Vec<&AstNode> {
    let Some(arguments) = node
        .descendants()
        .find(|child| child.kind == AstKind::ArgumentList)
    else {
        return Vec::new();
    };
    arguments
        .children
        .iter()
        .filter(|child| matches!(child.kind, AstKind::Expression(ExpressionKind::Expression)))
        .collect()
}

pub(super) fn simple_expression_variable(node: &AstNode) -> Option<&str> {
    if node.descendants().any(|child| {
        matches!(
            child.kind,
            AstKind::PropertyKey
                | AstKind::Operator
                | AstKind::LabelName
                | AstKind::RelationshipTypeName
        )
    }) {
        return None;
    }
    let mut variables = node
        .descendants()
        .filter(|child| child.kind == AstKind::Variable)
        .filter_map(|child| child.text.as_deref());
    let first = variables.next()?;
    variables.next().is_none().then_some(first)
}

pub(super) fn statically_negative_integer(node: &AstNode) -> bool {
    let literals = node
        .descendants()
        .filter(|child| {
            matches!(
                child.kind,
                AstKind::Literal(super::ast::LiteralKind::Integer)
            )
        })
        .collect::<Vec<_>>();
    if literals.len() != 1 {
        return false;
    }
    if node.descendants().any(|child| {
        !std::ptr::eq(child, literals[0])
            && matches!(
                child.kind,
                AstKind::Variable | AstKind::Parameter | AstKind::Literal(_)
            )
    }) {
        return false;
    }

    let mut negative = literals[0]
        .text
        .as_deref()
        .is_some_and(|text| text.starts_with('-'));
    for operator in node
        .descendants()
        .filter(|child| child.kind == AstKind::Operator)
        .filter_map(|child| child.text.as_deref())
    {
        match operator {
            "-" => negative = !negative,
            "+" => {}
            _ => return false,
        }
    }
    negative
}

pub(super) fn reference_key(node: &AstNode) -> Option<String> {
    if node.descendants().any(|child| {
        matches!(
            child.kind,
            AstKind::FunctionName
                | AstKind::Operator
                | AstKind::Literal(_)
                | AstKind::Pattern
                | AstKind::Subquery(_)
                | AstKind::Subscript
                | AstKind::Expression(ExpressionKind::List)
                | AstKind::Expression(ExpressionKind::Map)
                | AstKind::Expression(ExpressionKind::Case)
                | AstKind::Expression(ExpressionKind::InterpolatedString)
        )
    }) {
        return None;
    }

    let mut variables = node
        .descendants()
        .filter(|child| child.kind == AstKind::Variable)
        .filter_map(|child| child.text.as_deref())
        .map(unescape_identifier);
    let variable = variables.next()?;
    if variables.next().is_some() {
        return None;
    }

    let properties = node
        .descendants()
        .filter(|child| child.kind == AstKind::PropertyKey)
        .filter_map(|child| child.text.as_deref())
        .map(unescape_identifier);
    Some(
        std::iter::once(variable)
            .chain(properties)
            .fold(String::new(), |mut key, part| {
                use std::fmt::Write as _;
                let _ = write!(key, "{}:{part};", part.len());
                key
            }),
    )
}

pub(super) fn contains_aggregate(node: &AstNode) -> bool {
    if matches!(node.kind, AstKind::Subquery(_)) {
        return false;
    }
    is_aggregate_call(node) || node.children.iter().any(contains_aggregate)
}

pub(super) fn is_aggregate_call(node: &AstNode) -> bool {
    function_name(node).is_some_and(|name| {
        matches!(
            name.as_str(),
            "avg"
                | "collect"
                | "count"
                | "max"
                | "min"
                | "percentilecont"
                | "percentiledisc"
                | "stdev"
                | "stdevp"
                | "sum"
        )
    })
}
