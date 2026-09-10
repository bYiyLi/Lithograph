use super::ast::{AstKind, AstNode, ExpressionKind};
use super::error::{FrontendError, FrontendErrorKind};
use super::semantic::{Analyzer, BindingKind, Scope, unescape_identifier};
use super::types::{CypherType, infer_expression};

impl Analyzer<'_> {
    pub(super) fn validate_pattern_predicates(
        &self,
        node: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        let mut patterns = Vec::new();
        collect_expression_patterns(node, false, &mut patterns);
        for pattern in patterns {
            if !pattern
                .descendants()
                .any(|child| child.kind == AstKind::RelationshipPattern)
            {
                return Err(self.semantic_error(
                    pattern.span,
                    "pattern predicates must contain at least one relationship",
                ));
            }
            for binding in pattern.descendants().filter(|child| {
                matches!(
                    child.kind,
                    AstKind::PatternVariable | AstKind::RelationshipVariable
                )
            }) {
                let Some(name) = binding.text.as_deref() else {
                    continue;
                };
                let name = unescape_identifier(name);
                if !scope.contains_key(&name) {
                    return Err(self.semantic_error(
                        binding.span,
                        format!("pattern predicate cannot introduce variable {name:?}"),
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_predicate_types(
        &self,
        node: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        for predicate in node
            .descendants()
            .filter(|child| child.kind == AstKind::Where)
        {
            let Some(expression) = predicate.descendants().find(|child| {
                matches!(child.kind, AstKind::Expression(ExpressionKind::Expression))
            }) else {
                continue;
            };
            let value_type = infer_expression(expression, self.source)?;
            if !matches!(
                value_type,
                CypherType::Boolean | CypherType::Any | CypherType::Null
            ) {
                return Err(FrontendError::new(
                    FrontendErrorKind::Type,
                    "predicate expression must be Boolean-compatible",
                    expression.span,
                    self.source,
                ));
            }
            if let Some(variable) = simple_value_variable(expression)
                && matches!(
                    scope.get(variable.as_str()),
                    Some(BindingKind::Node | BindingKind::Relationship | BindingKind::Path)
                )
            {
                return Err(FrontendError::new(
                    FrontendErrorKind::Type,
                    "graph element value cannot be used directly as a predicate",
                    expression.span,
                    self.source,
                ));
            }
        }
        Ok(())
    }

    pub(super) fn reject_pattern_expressions(&self, node: &AstNode) -> Result<(), FrontendError> {
        let mut patterns = Vec::new();
        collect_expression_patterns(node, false, &mut patterns);
        if let Some(pattern) = patterns.first() {
            Err(self.semantic_error(
                pattern.span,
                "pattern expressions are only valid in predicate contexts",
            ))
        } else {
            Ok(())
        }
    }

    pub(super) fn validate_expression_categories(
        &self,
        node: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        self.validate_path_property_access(node, scope)?;
        for function in node
            .descendants()
            .filter(|node| matches!(node.kind, AstKind::Expression(ExpressionKind::FunctionCall)))
        {
            let Some(name) = function_name(function) else {
                continue;
            };
            if name == "size" && has_direct_pattern_argument(function) {
                return Err(self
                    .semantic_error(function.span, "size() does not accept a pattern expression"));
            }
            let Some(variable) = first_argument_variable(function) else {
                continue;
            };
            let Some(kind) = scope.get(variable.as_str()) else {
                continue;
            };
            let valid = match name.as_str() {
                "labels" => matches!(kind, BindingKind::Node | BindingKind::Unknown),
                "type" => matches!(kind, BindingKind::Relationship | BindingKind::Unknown),
                "length" => matches!(kind, BindingKind::Path | BindingKind::Unknown),
                "size" => *kind != BindingKind::Path,
                "properties" => matches!(
                    kind,
                    BindingKind::Node
                        | BindingKind::Relationship
                        | BindingKind::Map
                        | BindingKind::Value
                ),
                _ => true,
            };
            if !valid {
                return Err(self.semantic_error(
                    function.span,
                    format!("{name}() does not accept a {kind:?} value"),
                ));
            }
        }
        Ok(())
    }

    fn validate_path_property_access(
        &self,
        node: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        for postfix in node.descendants().filter(|node| {
            matches!(node.kind, AstKind::Expression(ExpressionKind::Postfix))
                && node
                    .descendants()
                    .any(|child| child.kind == AstKind::PropertyKey)
        }) {
            let Some(variable) = postfix
                .descendants()
                .find(|child| child.kind == AstKind::Variable)
                .and_then(|child| child.text.as_deref())
            else {
                continue;
            };
            if scope.get(&unescape_identifier(variable)) == Some(&BindingKind::Path) {
                return Err(
                    self.semantic_error(postfix.span, "Path values do not expose graph properties")
                );
            }
        }
        Ok(())
    }
}

pub(super) fn validate_local_binding_type_use(
    node: &AstNode,
    binding: &str,
    element_type: &CypherType,
    source: &str,
) -> Result<(), FrontendError> {
    for expression in node.descendants().filter(|child| {
        matches!(
            child.kind,
            AstKind::Expression(ExpressionKind::Additive)
                | AstKind::Expression(ExpressionKind::Multiplicative)
                | AstKind::Expression(ExpressionKind::Power)
        ) && child.descendants().any(|nested| {
            nested.kind == AstKind::Variable
                && nested
                    .text
                    .as_deref()
                    .is_some_and(|name| unescape_identifier(name) == binding)
        })
    }) {
        let operators = expression
            .children
            .iter()
            .filter(|child| child.kind == AstKind::Operator)
            .filter_map(|child| child.text.as_deref())
            .collect::<Vec<_>>();
        let requires_numeric = operators
            .iter()
            .any(|operator| matches!(*operator, "%" | "*" | "/" | "-" | "^"));
        let plus_incompatible = operators.contains(&"+")
            && !matches!(
                element_type,
                CypherType::Integer | CypherType::Float | CypherType::String
            );
        if (requires_numeric && !matches!(element_type, CypherType::Integer | CypherType::Float))
            || plus_incompatible
        {
            return Err(FrontendError::new(
                FrontendErrorKind::Type,
                "quantifier/comprehension predicate uses the bound element with an incompatible operator",
                expression.span,
                source,
            ));
        }
    }
    Ok(())
}

fn collect_expression_patterns<'a>(
    node: &'a AstNode,
    inside_expression: bool,
    output: &mut Vec<&'a AstNode>,
) {
    if matches!(node.kind, AstKind::Subquery(_)) {
        return;
    }
    if matches!(node.kind, AstKind::Expression(ExpressionKind::List))
        && node
            .descendants()
            .any(|child| child.kind == AstKind::Pattern)
    {
        return;
    }
    let inside_expression = inside_expression || matches!(node.kind, AstKind::Expression(_));
    if node.kind == AstKind::Pattern && inside_expression {
        output.push(node);
        return;
    }
    for child in &node.children {
        collect_expression_patterns(child, inside_expression, output);
    }
}

fn has_direct_pattern_argument(node: &AstNode) -> bool {
    let Some(arguments) = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::ArgumentList)
    else {
        return false;
    };
    contains_pattern_outside_list(arguments)
}

fn contains_pattern_outside_list(node: &AstNode) -> bool {
    if matches!(node.kind, AstKind::Expression(ExpressionKind::List)) {
        return false;
    }
    node.kind == AstKind::Pattern || node.children.iter().any(contains_pattern_outside_list)
}

fn simple_value_variable(node: &AstNode) -> Option<String> {
    if node.descendants().any(|child| {
        matches!(
            child.kind,
            AstKind::PropertyKey
                | AstKind::Operator
                | AstKind::FunctionName
                | AstKind::Pattern
                | AstKind::Subquery(_)
                | AstKind::LabelName
                | AstKind::RelationshipTypeName
                | AstKind::Subscript
        )
    }) {
        return None;
    }
    let mut variables = node
        .descendants()
        .filter(|child| child.kind == AstKind::Variable)
        .filter_map(|child| child.text.as_deref())
        .map(unescape_identifier);
    let first = variables.next()?;
    variables.next().is_none().then_some(first)
}

pub(super) fn function_name(node: &AstNode) -> Option<String> {
    if !matches!(node.kind, AstKind::Expression(ExpressionKind::FunctionCall)) {
        return None;
    }
    node.children
        .iter()
        .find(|child| child.kind == AstKind::FunctionName)
        .and_then(|child| child.text.as_deref())
        .map(|name| name.to_ascii_lowercase())
}

fn first_argument_variable(node: &AstNode) -> Option<String> {
    let arguments = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::ArgumentList)?;
    if arguments
        .descendants()
        .any(|child| matches!(child.kind, AstKind::Subscript | AstKind::PropertyKey))
    {
        return None;
    }
    arguments
        .descendants()
        .find(|child| child.kind == AstKind::Variable)
        .and_then(|child| child.text.as_deref())
        .map(unescape_identifier)
}
