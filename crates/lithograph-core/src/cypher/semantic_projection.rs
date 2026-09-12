use super::ast::{AstKind, AstNode, ExpressionKind};
use super::semantic::{BindingKind, unescape_identifier};
use super::types::{CypherType, infer_expression};

pub(super) fn projection_items(node: &AstNode) -> Vec<&AstNode> {
    let mut output = Vec::new();
    collect_projection_items(node, &mut output);
    output
}

fn collect_projection_items<'a>(node: &'a AstNode, output: &mut Vec<&'a AstNode>) {
    if node.kind == AstKind::ProjectionItem {
        output.push(node);
        return;
    }
    if matches!(node.kind, AstKind::Subquery(_)) {
        return;
    }
    for child in &node.children {
        collect_projection_items(child, output);
    }
}

pub(super) fn has_star_projection(node: &AstNode) -> bool {
    contains_surface_kind(node, AstKind::StarProjection)
}

fn contains_surface_kind(node: &AstNode, kind: AstKind) -> bool {
    if matches!(node.kind, AstKind::Subquery(_)) {
        return false;
    }
    node.kind == kind
        || node
            .children
            .iter()
            .any(|child| contains_surface_kind(child, kind.clone()))
}

pub(super) fn simple_projection_variable(item: &AstNode) -> Option<&str> {
    if item.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::PropertyKey
                | AstKind::FunctionName
                | AstKind::Operator
                | AstKind::Literal(_)
                | AstKind::Pattern
                | AstKind::Subquery(_)
                | AstKind::Subscript
                | AstKind::Expression(ExpressionKind::FunctionCall)
                | AstKind::Expression(ExpressionKind::List)
                | AstKind::Expression(ExpressionKind::Map)
                | AstKind::Expression(ExpressionKind::Case)
                | AstKind::Expression(ExpressionKind::InterpolatedString)
        )
    }) {
        return None;
    }
    let mut variables = item
        .descendants()
        .filter(|node| node.kind == AstKind::Variable)
        .filter_map(|node| node.text.as_deref());
    let first = variables.next()?;
    variables.next().is_none().then_some(first)
}

pub(super) fn find_descendant(node: &AstNode, kind: AstKind) -> Option<&AstNode> {
    node.descendants().find(|node| node.kind == kind)
}

pub(super) fn expression_binding_kind(expression: &AstNode, source: &str) -> BindingKind {
    match infer_expression(expression, source) {
        Ok(CypherType::Node) => BindingKind::Node,
        Ok(CypherType::Relationship) => BindingKind::Relationship,
        Ok(CypherType::Path) => BindingKind::Path,
        Ok(CypherType::Any | CypherType::Null) => BindingKind::Unknown,
        Ok(CypherType::List(_)) => BindingKind::List,
        Ok(CypherType::Map) => BindingKind::Map,
        Ok(_) | Err(_) => BindingKind::Value,
    }
}

pub(super) fn yield_output_name(item: &AstNode) -> String {
    item.descendants()
        .find(|node| node.kind == AstKind::ProjectionAlias)
        .or_else(|| {
            item.descendants()
                .find(|node| node.kind == AstKind::YieldName)
        })
        .and_then(|node| node.text.as_deref())
        .map(unescape_identifier)
        .unwrap_or_default()
}
