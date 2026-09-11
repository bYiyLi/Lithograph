use super::ast::{AstKind, AstNode, ExpressionKind, LiteralKind};
use super::error::{FrontendError, FrontendErrorKind, Span};
use super::value::{Value, VectorCoordinateType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CypherType {
    Null,
    Boolean,
    Integer,
    Float,
    String,
    List(Box<CypherType>),
    Map,
    Node,
    Relationship,
    Path,
    Date,
    LocalTime,
    Time,
    LocalDateTime,
    ZonedDateTime,
    Duration,
    Point,
    Vector(Option<VectorCoordinateType>, Option<usize>),
    Uuid,
    Any,
}

impl CypherType {
    pub fn of_value(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Boolean(_) => Self::Boolean,
            Value::Integer(_) => Self::Integer,
            Value::Float(_) => Self::Float,
            Value::String(_) => Self::String,
            Value::List(values) => Self::List(Box::new(common_list_type(values))),
            Value::Map(_) => Self::Map,
            Value::Node(_) => Self::Node,
            Value::Relationship(_) => Self::Relationship,
            Value::Path(_) => Self::Path,
            Value::Date(_) => Self::Date,
            Value::LocalTime(_) => Self::LocalTime,
            Value::Time(_) => Self::Time,
            Value::LocalDateTime(_) => Self::LocalDateTime,
            Value::ZonedDateTime(_) => Self::ZonedDateTime,
            Value::Duration(_) => Self::Duration,
            Value::Point(_) => Self::Point,
            Value::Vector(value) => {
                Self::Vector(Some(value.coordinate_type()), Some(value.dimension()))
            }
            Value::Uuid(_) => Self::Uuid,
        }
    }

    pub fn is_property_type(&self) -> bool {
        match self {
            Self::Boolean
            | Self::Integer
            | Self::Float
            | Self::String
            | Self::Date
            | Self::LocalTime
            | Self::Time
            | Self::LocalDateTime
            | Self::ZonedDateTime
            | Self::Duration
            | Self::Point
            | Self::Vector(_, _)
            | Self::Uuid => true,
            Self::List(element) => element.is_property_list_element_type(),
            Self::Null | Self::Map | Self::Node | Self::Relationship | Self::Path | Self::Any => {
                false
            }
        }
    }

    fn is_property_list_element_type(&self) -> bool {
        matches!(
            self,
            Self::Boolean
                | Self::Integer
                | Self::Float
                | Self::String
                | Self::Date
                | Self::LocalTime
                | Self::Time
                | Self::LocalDateTime
                | Self::ZonedDateTime
                | Self::Duration
                | Self::Point
                | Self::Uuid
        )
    }
}

pub(super) fn infer_expression(node: &AstNode, source: &str) -> Result<CypherType, FrontendError> {
    let node = peel_transparent_expression(node);
    match node.kind {
        AstKind::Literal(kind) => super::types_literal::infer_literal(node, kind, source),
        AstKind::Parameter | AstKind::Variable | AstKind::PropertyKey => Ok(CypherType::Any),
        AstKind::ComparisonSuffix => infer_from_children(node, source),
        AstKind::Expression(_) => infer_expression_node(node, source),
        _ => infer_from_children(node, source),
    }
}

fn peel_transparent_expression(mut node: &AstNode) -> &AstNode {
    loop {
        if !matches!(node.kind, AstKind::Expression(_))
            || node.children.iter().any(|child| {
                matches!(
                    child.kind,
                    AstKind::Operator
                        | AstKind::ComparisonSuffix
                        | AstKind::Subscript
                        | AstKind::PropertyKey
                )
            })
            || (matches!(
                node.kind,
                AstKind::Expression(super::ast::ExpressionKind::Postfix)
            ) && node
                .descendants()
                .any(|child| child.kind == AstKind::TypePredicate))
        {
            return node;
        }
        let mut semantic_children = node
            .children
            .iter()
            .filter(|child| is_expression_like(child));
        let Some(child) = semantic_children.next() else {
            return node;
        };
        if semantic_children.next().is_some() {
            return node;
        }
        if matches!(
            node.kind,
            AstKind::Expression(
                super::ast::ExpressionKind::List
                    | super::ast::ExpressionKind::Map
                    | super::ast::ExpressionKind::Case
                    | super::ast::ExpressionKind::FunctionCall
                    | super::ast::ExpressionKind::InterpolatedString
            )
        ) {
            return node;
        }
        node = child;
    }
}

fn infer_expression_node(node: &AstNode, source: &str) -> Result<CypherType, FrontendError> {
    let AstKind::Expression(kind) = node.kind else {
        return infer_from_children(node, source);
    };
    match kind {
        ExpressionKind::Postfix => infer_postfix_expression(node, source),
        ExpressionKind::Unary if super::types_literal::is_i64_min_unary_expression(node) => {
            Ok(CypherType::Integer)
        }
        ExpressionKind::Comparison => infer_comparison_expression(node, source),
        ExpressionKind::InterpolatedString => Ok(CypherType::String),
        ExpressionKind::List => infer_list_expression(node, source),
        ExpressionKind::Map => Ok(CypherType::Map),
        ExpressionKind::Case => infer_case_expression(node, source),
        ExpressionKind::FunctionCall => super::types_function::infer_function(node, source),
        _ => infer_from_children(node, source),
    }
}

fn infer_postfix_expression(node: &AstNode, source: &str) -> Result<CypherType, FrontendError> {
    if node
        .descendants()
        .any(|child| child.kind == AstKind::TypePredicate)
    {
        for child in expression_children(node) {
            let _ = infer_expression(child, source)?;
        }
        return Ok(CypherType::Boolean);
    }
    if node
        .descendants()
        .any(|child| matches!(child.kind, AstKind::Subscript | AstKind::PropertyKey))
    {
        return Ok(CypherType::Any);
    }
    infer_from_children(node, source)
}

fn infer_comparison_expression(node: &AstNode, source: &str) -> Result<CypherType, FrontendError> {
    let suffixes = node
        .children
        .iter()
        .filter(|child| child.kind == AstKind::ComparisonSuffix)
        .collect::<Vec<_>>();
    if suffixes.is_empty() {
        return infer_from_children(node, source);
    }
    for child in expression_children(node) {
        let _ = infer_expression(child, source)?;
    }
    Ok(CypherType::Boolean)
}

fn infer_list_expression(node: &AstNode, source: &str) -> Result<CypherType, FrontendError> {
    let types = immediate_expressions(node)
        .into_iter()
        .map(|child| infer_expression(child, source))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CypherType::List(Box::new(common_type(&types))))
}

fn infer_case_expression(node: &AstNode, source: &str) -> Result<CypherType, FrontendError> {
    let Some(first_alternative) = node
        .children
        .iter()
        .position(|child| child.kind == AstKind::CaseAlternative)
    else {
        let types = expression_children(node)
            .map(|child| infer_expression(child, source))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(common_type(&types));
    };

    let mut types = Vec::new();
    for (index, child) in node.children.iter().enumerate() {
        if child.kind == AstKind::CaseAlternative {
            if let Some(result) = child
                .children
                .iter()
                .rfind(|nested| matches!(nested.kind, AstKind::Expression(_)))
            {
                types.push(infer_expression(result, source)?);
            }
        } else if index > first_alternative && matches!(child.kind, AstKind::Expression(_)) {
            // The only direct expression after CASE alternatives is ELSE. A simple CASE
            // operand appears before the first alternative and is not a result value.
            types.push(infer_expression(child, source)?);
        }
    }
    Ok(common_type(&types))
}

fn infer_from_children(node: &AstNode, source: &str) -> Result<CypherType, FrontendError> {
    let types = expression_children(node)
        .map(|child| infer_expression(child, source))
        .collect::<Result<Vec<_>, _>>()?;
    if types.is_empty() {
        return Ok(CypherType::Any);
    }
    let operators = direct_operators(node);
    if let Some(result) = infer_predicate_operator(node, source, &types, &operators) {
        return result;
    }
    if let Some(result) = infer_arithmetic_operator(node, source, &types, &operators) {
        return result;
    }
    Ok(common_type(&types))
}

fn direct_operators(node: &AstNode) -> Vec<String> {
    node.children
        .iter()
        .filter(|child| child.kind == AstKind::Operator)
        .filter_map(|child| child.text.as_deref())
        .map(str::to_ascii_uppercase)
        .collect()
}

fn infer_predicate_operator(
    node: &AstNode,
    source: &str,
    types: &[CypherType],
    operators: &[String],
) -> Option<Result<CypherType, FrontendError>> {
    if operators.iter().any(|operator| operator == "IN") {
        return Some(validate_in_operator(node, source, types));
    }
    if operators
        .iter()
        .any(|operator| matches!(operator.as_str(), "CONTAINS" | "STARTS" | "ENDS"))
    {
        return Some(validate_string_predicate(node, source, types));
    }
    if operators
        .iter()
        .any(|operator| matches!(operator.as_str(), "AND" | "OR" | "XOR" | "NOT"))
    {
        return Some(require_boolean(types, node.span, source).map(|()| CypherType::Boolean));
    }
    operators
        .iter()
        .any(|operator| {
            matches!(
                operator.as_str(),
                "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=" | "=~"
            )
        })
        .then_some(Ok(CypherType::Boolean))
}

fn validate_in_operator(
    node: &AstNode,
    source: &str,
    types: &[CypherType],
) -> Result<CypherType, FrontendError> {
    let right = types.last().cloned().unwrap_or(CypherType::Any);
    if matches!(
        right,
        CypherType::List(_) | CypherType::Any | CypherType::Null
    ) {
        Ok(CypherType::Boolean)
    } else {
        Err(type_error(
            source,
            node.span,
            "IN requires a List-compatible right operand",
        ))
    }
}

fn validate_string_predicate(
    node: &AstNode,
    source: &str,
    types: &[CypherType],
) -> Result<CypherType, FrontendError> {
    let contains_null_literal = node
        .descendants()
        .any(|child| matches!(child.kind, AstKind::Literal(LiteralKind::Null)));
    let compatible = types.iter().all(|value| {
        matches!(
            value,
            CypherType::String | CypherType::Any | CypherType::Null
        )
    });
    if contains_null_literal || types.contains(&CypherType::Null) || compatible {
        Ok(CypherType::Boolean)
    } else {
        Err(type_error(
            source,
            node.span,
            "string predicate requires String-compatible operands",
        ))
    }
}

fn infer_arithmetic_operator(
    node: &AstNode,
    source: &str,
    types: &[CypherType],
    operators: &[String],
) -> Option<Result<CypherType, FrontendError>> {
    if operators.iter().any(|operator| operator == "/") {
        return Some(require_numeric(types, node.span, source).map(|()| CypherType::Float));
    }
    if operators
        .iter()
        .any(|operator| matches!(operator.as_str(), "-" | "*" | "%" | "^"))
    {
        return Some(
            require_numeric(types, node.span, source).map(|()| numeric_result_type(types)),
        );
    }
    if operators.iter().any(|operator| operator == "+") {
        return Some(plus_result_type(types, node.span, source));
    }
    if operators.iter().any(|operator| operator == "||") {
        return Some(concat_result_type(types, node.span, source));
    }
    None
}

pub(super) fn require_numeric(
    types: &[CypherType],
    span: Span,
    source: &str,
) -> Result<(), FrontendError> {
    if types.iter().all(|value| {
        matches!(
            value,
            CypherType::Integer | CypherType::Float | CypherType::Any | CypherType::Null
        )
    }) {
        Ok(())
    } else {
        Err(type_error(
            source,
            span,
            "numeric operator received a non-numeric operand",
        ))
    }
}

fn require_boolean(types: &[CypherType], span: Span, source: &str) -> Result<(), FrontendError> {
    if types.iter().all(|value| {
        matches!(
            value,
            CypherType::Boolean | CypherType::Any | CypherType::Null
        )
    }) {
        Ok(())
    } else {
        Err(type_error(
            source,
            span,
            "boolean operator received a non-boolean operand",
        ))
    }
}

fn numeric_result_type(types: &[CypherType]) -> CypherType {
    if types.contains(&CypherType::Float) {
        CypherType::Float
    } else if types
        .iter()
        .all(|value| matches!(value, CypherType::Integer | CypherType::Null))
    {
        CypherType::Integer
    } else {
        CypherType::Any
    }
}

fn plus_result_type(
    types: &[CypherType],
    span: Span,
    source: &str,
) -> Result<CypherType, FrontendError> {
    if types.iter().all(is_numeric_or_unknown) {
        return Ok(numeric_result_type(types));
    }
    if types.iter().all(|value| {
        matches!(
            value,
            CypherType::String | CypherType::Any | CypherType::Null
        )
    }) {
        return Ok(if types.contains(&CypherType::Any) {
            CypherType::Any
        } else {
            CypherType::String
        });
    }
    if types
        .iter()
        .any(|value| matches!(value, CypherType::List(_)))
    {
        return Ok(CypherType::List(Box::new(CypherType::Any)));
    }
    if types.contains(&CypherType::Any) {
        return Ok(CypherType::Any);
    }
    Err(type_error(
        source,
        span,
        "+ operands have incompatible types",
    ))
}

fn concat_result_type(
    types: &[CypherType],
    span: Span,
    source: &str,
) -> Result<CypherType, FrontendError> {
    if types.iter().all(|value| {
        matches!(
            value,
            CypherType::String | CypherType::Any | CypherType::Null
        )
    }) {
        return Ok(CypherType::String);
    }
    if types.iter().all(|value| {
        matches!(
            value,
            CypherType::List(_) | CypherType::Any | CypherType::Null
        )
    }) {
        return Ok(CypherType::List(Box::new(CypherType::Any)));
    }
    Err(type_error(
        source,
        span,
        "|| operands have incompatible types",
    ))
}

fn is_numeric_or_unknown(value: &CypherType) -> bool {
    matches!(
        value,
        CypherType::Integer | CypherType::Float | CypherType::Any | CypherType::Null
    )
}

fn common_list_type(values: &[Value]) -> CypherType {
    let types = values.iter().map(CypherType::of_value).collect::<Vec<_>>();
    common_type(&types)
}

fn common_type(types: &[CypherType]) -> CypherType {
    let mut non_null = types.iter().filter(|value| **value != CypherType::Null);
    let Some(first) = non_null.next() else {
        return CypherType::Any;
    };
    if non_null.all(|value| value == first) {
        first.clone()
    } else if types.iter().all(|value| {
        matches!(
            value,
            CypherType::Integer | CypherType::Float | CypherType::Null
        )
    }) {
        CypherType::Float
    } else {
        CypherType::Any
    }
}

pub(super) fn immediate_expressions(node: &AstNode) -> Vec<&AstNode> {
    let mut output = Vec::new();
    collect_immediate_expressions(node, &mut output);
    output
}

fn collect_immediate_expressions<'a>(node: &'a AstNode, output: &mut Vec<&'a AstNode>) {
    for child in &node.children {
        if matches!(child.kind, AstKind::Expression(_)) {
            output.push(child);
        } else if !matches!(child.kind, AstKind::Subquery(_)) {
            collect_immediate_expressions(child, output);
        }
    }
}

fn expression_children(node: &AstNode) -> impl Iterator<Item = &AstNode> {
    node.children
        .iter()
        .filter(|child| is_expression_like(child))
}

fn is_expression_like(node: &AstNode) -> bool {
    matches!(
        node.kind,
        AstKind::Expression(_)
            | AstKind::ComparisonSuffix
            | AstKind::Literal(_)
            | AstKind::Variable
            | AstKind::Parameter
    )
}

pub(super) fn type_error(source: &str, span: Span, message: impl Into<String>) -> FrontendError {
    FrontendError::new(FrontendErrorKind::Type, message, span, source)
}
