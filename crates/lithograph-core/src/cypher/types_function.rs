use super::ast::{AstKind, AstNode, ExpressionKind, LiteralKind};
use super::error::{FrontendError, Span};
use super::types::{
    CypherType, immediate_expressions, infer_expression, require_numeric, type_error,
};
use super::value::VectorCoordinateType;

pub(super) fn infer_function(node: &AstNode, source: &str) -> Result<CypherType, FrontendError> {
    let name = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::FunctionName)
        .and_then(|child| child.text.as_deref())
        .or_else(|| node.text.as_deref().and_then(special_function_name))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let arguments = function_arguments(node);
    if let Some(value) = infer_constructor_function(&name, &arguments, node, source)? {
        return Ok(value);
    }
    if let Some(value) = infer_structural_function(&name, &arguments, node, source)? {
        return Ok(value);
    }
    if !super::is_builtin_function(&name) {
        return Err(type_error(
            source,
            node.span,
            format!("unknown current-graph function {name}"),
        ));
    }
    match name.as_str() {
        "tostring" => typed_unary_result(
            &name,
            &arguments,
            node,
            source,
            &[
                CypherType::Integer,
                CypherType::Float,
                CypherType::Boolean,
                CypherType::String,
                CypherType::Point,
                CypherType::Duration,
                CypherType::Date,
                CypherType::Time,
                CypherType::LocalTime,
                CypherType::LocalDateTime,
                CypherType::ZonedDateTime,
            ],
            CypherType::String,
            "toString() input type cannot be converted to String",
        ),
        "tointeger" => typed_unary_result(
            &name,
            &arguments,
            node,
            source,
            &[
                CypherType::Boolean,
                CypherType::String,
                CypherType::Integer,
                CypherType::Float,
            ],
            CypherType::Integer,
            "toInteger() expects a Boolean, String, Integer, or Float",
        ),
        "tofloat" => typed_unary_result(
            &name,
            &arguments,
            node,
            source,
            &[CypherType::String, CypherType::Integer, CypherType::Float],
            CypherType::Float,
            "toFloat() expects a String, Integer, or Float",
        ),
        "isnan" => {
            require_arity(&name, &arguments, 1, node.span, source)?;
            let types = arguments
                .iter()
                .map(|argument| infer_expression(argument, source))
                .collect::<Result<Vec<_>, _>>()?;
            require_numeric(&types, node.span, source)?;
            Ok(CypherType::Boolean)
        }
        "exists" => unary_result(&name, &arguments, node, source, CypherType::Boolean),
        "property_exists" => {
            require_arity(&name, &arguments, 1, node.span, source)?;
            require_type(
                &arguments,
                &[CypherType::Node, CypherType::Relationship],
                node.span,
                source,
                "PROPERTY_EXISTS() expects a Node or Relationship",
            )?;
            Ok(CypherType::Boolean)
        }
        "count" => Ok(CypherType::Integer),
        "all" | "any" | "none" | "single" => Ok(CypherType::Boolean),
        "allreduce" => Ok(CypherType::Boolean),
        "reduce" => arguments.first().map_or(Ok(CypherType::Any), |initial| {
            infer_expression(initial, source)
        }),
        _ => Ok(CypherType::Any),
    }
}

fn function_arguments(node: &AstNode) -> Vec<&AstNode> {
    node.children
        .iter()
        .find(|child| child.kind == AstKind::ArgumentList)
        .map(immediate_expressions)
        .unwrap_or_else(|| {
            node.children
                .iter()
                .filter(|child| matches!(child.kind, AstKind::Expression(_)))
                .collect()
        })
}

fn infer_constructor_function(
    name: &str,
    arguments: &[&AstNode],
    node: &AstNode,
    source: &str,
) -> Result<Option<CypherType>, FrontendError> {
    let value = match name {
        "uuid" => Some(infer_uuid_constructor(arguments, node, source)?),
        "vector" => Some(infer_vector_constructor(arguments, node, source)?),
        "date" => Some(infer_temporal_constructor(
            name,
            arguments,
            node,
            source,
            CypherType::Date,
            true,
        )?),
        "localtime" | "local_time" => Some(infer_temporal_constructor(
            name,
            arguments,
            node,
            source,
            CypherType::LocalTime,
            true,
        )?),
        "time" | "zoned_time" => Some(infer_temporal_constructor(
            name,
            arguments,
            node,
            source,
            CypherType::Time,
            true,
        )?),
        "datetime" | "zoned_datetime" => Some(infer_temporal_constructor(
            name,
            arguments,
            node,
            source,
            CypherType::ZonedDateTime,
            true,
        )?),
        "localdatetime" | "local_datetime" => Some(infer_temporal_constructor(
            name,
            arguments,
            node,
            source,
            CypherType::LocalDateTime,
            true,
        )?),
        "duration" => Some(infer_duration_constructor(arguments, node, source)?),
        "point" => Some(infer_point_constructor(arguments, node, source)?),
        _ => None,
    };
    Ok(value)
}

fn infer_uuid_constructor(
    arguments: &[&AstNode],
    node: &AstNode,
    source: &str,
) -> Result<CypherType, FrontendError> {
    match arguments {
        [] => Ok(CypherType::Uuid),
        [name] => {
            require_expression_type(
                name,
                &[CypherType::String],
                source,
                "uuid() one-argument overload expects a String",
            )?;
            Ok(CypherType::Uuid)
        }
        [most, least] => {
            for argument in [most, least] {
                require_expression_type(
                    argument,
                    &[CypherType::Integer],
                    source,
                    "uuid() two-argument overload expects two Integers",
                )?;
            }
            Ok(CypherType::Uuid)
        }
        _ => Err(type_error(
            source,
            node.span,
            "uuid() expects zero arguments, one String, or two Integers",
        )),
    }
}

fn infer_temporal_constructor(
    name: &str,
    arguments: &[&AstNode],
    node: &AstNode,
    source: &str,
    result: CypherType,
    allow_zero: bool,
) -> Result<CypherType, FrontendError> {
    match arguments {
        [] if allow_zero => Ok(result),
        [input] => {
            let _ = infer_expression(input, source)?;
            Ok(result)
        }
        [input, pattern] => {
            require_expression_type(
                input,
                &[CypherType::String],
                source,
                &format!("{name}() input must be a String when a pattern is provided"),
            )?;
            require_expression_type(
                pattern,
                &[CypherType::String],
                source,
                &format!("{name}() pattern must be a String"),
            )?;
            Ok(result)
        }
        [] => Err(type_error(
            source,
            node.span,
            format!("{name}() requires an input argument"),
        )),
        _ => Err(type_error(
            source,
            node.span,
            format!("{name}() accepts at most input and pattern arguments"),
        )),
    }
}

fn infer_point_constructor(
    arguments: &[&AstNode],
    node: &AstNode,
    source: &str,
) -> Result<CypherType, FrontendError> {
    require_arity("point", arguments, 1, node.span, source)?;
    require_expression_type(
        arguments[0],
        &[CypherType::Map],
        source,
        "point() expects a Map",
    )?;
    Ok(CypherType::Point)
}

fn infer_duration_constructor(
    arguments: &[&AstNode],
    node: &AstNode,
    source: &str,
) -> Result<CypherType, FrontendError> {
    match arguments {
        [input] => {
            let _ = infer_expression(input, source)?;
            Ok(CypherType::Duration)
        }
        [input, pattern] => {
            require_expression_type(
                input,
                &[CypherType::String],
                source,
                "duration() input must be a String when a pattern is provided",
            )?;
            require_expression_type(
                pattern,
                &[CypherType::String],
                source,
                "duration() pattern must be a String",
            )?;
            Ok(CypherType::Duration)
        }
        _ => Err(type_error(
            source,
            node.span,
            "duration() expects input and an optional pattern",
        )),
    }
}

fn infer_vector_constructor(
    arguments: &[&AstNode],
    node: &AstNode,
    source: &str,
) -> Result<CypherType, FrontendError> {
    let coordinate_type = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::VectorCoordinateTypeName)
        .and_then(|child| child.text.as_deref());
    let arity = arguments.len() + usize::from(coordinate_type.is_some());
    if arity != 3 {
        return Err(type_error(
            source,
            node.span,
            "vector() expects value, dimension, and coordinateType",
        ));
    }
    let coordinate_type = coordinate_type
        .and_then(parse_cypher_vector_coordinate_type)
        .ok_or_else(|| type_error(source, node.span, "vector() coordinateType is unsupported"))?;

    let value_type = infer_expression(arguments[0], source)?;
    if !matches!(
        value_type,
        CypherType::String | CypherType::List(_) | CypherType::Any | CypherType::Null
    ) {
        return Err(type_error(
            source,
            arguments[0].span,
            "vector() value must be a String or a List of numeric values",
        ));
    }
    if let CypherType::List(element_type) = &value_type
        && !matches!(
            element_type.as_ref(),
            CypherType::Integer | CypherType::Float | CypherType::Any | CypherType::Null
        )
    {
        return Err(type_error(
            source,
            arguments[0].span,
            "vector() list entries must be Integer or Float values",
        ));
    }

    let dimension_type = infer_expression(arguments[1], source)?;
    if !matches!(
        dimension_type,
        CypherType::Integer | CypherType::Any | CypherType::Null
    ) {
        return Err(type_error(
            source,
            arguments[1].span,
            "vector() dimension must be an Integer",
        ));
    }
    let dimension = static_integer(arguments[1]);
    if let Some(dimension) = dimension
        && !(1..=4_096).contains(&dimension)
    {
        return Err(type_error(
            source,
            arguments[1].span,
            "vector() dimension must be between 1 and 4096",
        ));
    }

    if let (Some(dimension), Some(element_count)) =
        (dimension, static_list_element_count(arguments[0]))
        && usize::try_from(dimension).ok() != Some(element_count)
    {
        return Err(type_error(
            source,
            arguments[0].span,
            "vector() list length must match its declared dimension",
        ));
    }

    Ok(CypherType::Vector(
        Some(coordinate_type),
        dimension.and_then(|value| usize::try_from(value).ok()),
    ))
}

pub(super) fn parse_cypher_vector_coordinate_type(text: &str) -> Option<VectorCoordinateType> {
    match text.to_ascii_uppercase().as_str() {
        "INTEGER" | "SIGNED INTEGER" | "INT" | "INT64" | "INTEGER64" => {
            Some(VectorCoordinateType::I64)
        }
        "INT32" | "INTEGER32" => Some(VectorCoordinateType::I32),
        "INT16" | "INTEGER16" => Some(VectorCoordinateType::I16),
        "INT8" | "INTEGER8" => Some(VectorCoordinateType::I8),
        "FLOAT" | "FLOAT64" => Some(VectorCoordinateType::F64),
        "FLOAT32" => Some(VectorCoordinateType::F32),
        _ => None,
    }
}

fn static_list_element_count(node: &AstNode) -> Option<usize> {
    let list = node.descendants().find(|child| {
        matches!(child.kind, AstKind::Expression(ExpressionKind::List))
            && !child.descendants().any(|nested| {
                matches!(
                    nested.kind,
                    AstKind::BindingVariable | AstKind::PredicateVariable | AstKind::Pattern
                )
            })
    })?;
    Some(immediate_expressions(list).len())
}

fn static_integer(node: &AstNode) -> Option<i64> {
    let literals = node
        .descendants()
        .filter(|child| matches!(child.kind, AstKind::Literal(LiteralKind::Integer)))
        .collect::<Vec<_>>();
    if literals.len() != 1
        || node.descendants().any(|child| {
            !std::ptr::eq(child, literals[0])
                && matches!(
                    child.kind,
                    AstKind::Variable | AstKind::Parameter | AstKind::Literal(_)
                )
        })
    {
        return None;
    }
    let mut value = super::types_literal::parse_integer_i128(literals[0].text.as_deref()?)
        .and_then(|value| i64::try_from(value).ok())?;
    for operator in node
        .descendants()
        .filter(|child| child.kind == AstKind::Operator)
        .filter_map(|child| child.text.as_deref())
    {
        match operator {
            "-" => value = value.checked_neg()?,
            "+" => {}
            _ => return None,
        }
    }
    Some(value)
}

fn infer_structural_function(
    name: &str,
    arguments: &[&AstNode],
    node: &AstNode,
    source: &str,
) -> Result<Option<CypherType>, FrontendError> {
    let (accepted, result, message) = match name {
        "labels" => (
            &[CypherType::Node][..],
            CypherType::List(Box::new(CypherType::String)),
            "labels() expects a Node",
        ),
        "type" => (
            &[CypherType::Relationship][..],
            CypherType::String,
            "type() expects a Relationship",
        ),
        "length" => (
            &[CypherType::Path][..],
            CypherType::Integer,
            "length() expects a Path",
        ),
        "size" => (
            &[
                CypherType::String,
                CypherType::List(Box::new(CypherType::Any)),
                CypherType::Vector(None, None),
            ][..],
            CypherType::Integer,
            "size() expects a String, List, or Vector",
        ),
        "properties" => (
            &[CypherType::Map, CypherType::Node, CypherType::Relationship][..],
            CypherType::Map,
            "properties() expects a Map, Node, or Relationship",
        ),
        _ => return Ok(None),
    };
    require_arity(name, arguments, 1, node.span, source)?;
    require_type(arguments, accepted, node.span, source, message)?;
    Ok(Some(result))
}

fn unary_result(
    name: &str,
    arguments: &[&AstNode],
    node: &AstNode,
    source: &str,
    result: CypherType,
) -> Result<CypherType, FrontendError> {
    require_arity(name, arguments, 1, node.span, source)?;
    Ok(result)
}

fn typed_unary_result(
    name: &str,
    arguments: &[&AstNode],
    node: &AstNode,
    source: &str,
    accepted: &[CypherType],
    result: CypherType,
    message: &str,
) -> Result<CypherType, FrontendError> {
    require_arity(name, arguments, 1, node.span, source)?;
    require_type(arguments, accepted, node.span, source, message)?;
    Ok(result)
}

fn require_arity(
    name: &str,
    arguments: &[&AstNode],
    expected: usize,
    span: Span,
    source: &str,
) -> Result<(), FrontendError> {
    if arguments.len() == expected {
        Ok(())
    } else {
        Err(type_error(
            source,
            span,
            format!("{name}() expects {expected} argument(s)"),
        ))
    }
}

fn require_type(
    arguments: &[&AstNode],
    accepted: &[CypherType],
    span: Span,
    source: &str,
    message: &str,
) -> Result<(), FrontendError> {
    let Some(argument) = arguments.first() else {
        return Ok(());
    };
    let actual = infer_expression(argument, source)?;
    if matches!(actual, CypherType::Any | CypherType::Null)
        || accepted
            .iter()
            .any(|expected| type_compatible(&actual, expected))
    {
        Ok(())
    } else {
        Err(type_error(source, span, message))
    }
}

fn require_expression_type(
    argument: &AstNode,
    accepted: &[CypherType],
    source: &str,
    message: &str,
) -> Result<(), FrontendError> {
    let actual = infer_expression(argument, source)?;
    if matches!(actual, CypherType::Any | CypherType::Null)
        || accepted
            .iter()
            .any(|expected| type_compatible(&actual, expected))
    {
        Ok(())
    } else {
        Err(type_error(source, argument.span, message))
    }
}

fn type_compatible(actual: &CypherType, expected: &CypherType) -> bool {
    match (actual, expected) {
        (CypherType::List(_), CypherType::List(_)) => true,
        (CypherType::Vector(_, _), CypherType::Vector(_, _)) => true,
        _ => actual == expected,
    }
}

fn special_function_name(text: &str) -> Option<&str> {
    text.split_once('(').map(|(name, _)| name.trim())
}
