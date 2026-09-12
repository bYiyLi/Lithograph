use crate::cypher::{self, AstKind, AstNode};
use crate::storage::{PropertyRule, PropertyType};

use super::super::super::QueryResult;
use super::schema_error;

pub(super) fn parse_property_rule(expression: &AstNode) -> QueryResult<PropertyRule> {
    let terms = expression
        .children
        .iter()
        .filter(|node| node.kind == AstKind::TypeTerm)
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return Err(schema_error("property type expression is empty"));
    }
    if terms.len() == 1 {
        let term = terms[0];
        return Ok(PropertyRule {
            property_type: parse_property_type_term(term)?,
            required: term_outer_non_null(term),
        });
    }
    let members = terms
        .into_iter()
        .map(parse_property_type_term)
        .collect::<QueryResult<Vec<_>>>()?;
    Ok(PropertyRule {
        property_type: PropertyType::Union { members },
        required: false,
    })
}

fn parse_property_type_term(term: &AstNode) -> QueryResult<PropertyType> {
    let type_node = term
        .children
        .iter()
        .find(|node| node.kind == AstKind::TypeName)
        .ok_or_else(|| schema_error("property type is missing its base type"))?;
    let name = cypher::unescape_identifier(type_node.text.as_deref().unwrap_or_default())
        .to_ascii_uppercase();
    let parameters = term
        .children
        .iter()
        .find(|node| node.kind == AstKind::TypeParameters);
    let suffixes = term
        .children
        .iter()
        .filter(|node| node.kind == AstKind::TypeListSuffix)
        .count();
    if name == "ANY" && parameters.is_some() {
        return parse_dynamic_union_type(parameters);
    }
    if name == "LIST" || name == "ARRAY" {
        return parse_list_property_type(parameters);
    }
    if suffixes == 1 {
        return Ok(PropertyType::List {
            element: Box::new(simple_property_type(&name)?),
        });
    }
    match name.as_str() {
        "VECTOR" => parse_vector_property_type(type_node),
        "ANY" => Ok(PropertyType::Any),
        _ => simple_property_type(&name),
    }
}

fn nested_type_expression<'a>(
    parameters: Option<&'a AstNode>,
    missing_message: &str,
) -> QueryResult<&'a AstNode> {
    parameters
        .and_then(|node| {
            node.children
                .iter()
                .find(|child| child.kind == AstKind::TypeExpression)
        })
        .ok_or_else(|| schema_error(missing_message))
}

fn parse_dynamic_union_type(parameters: Option<&AstNode>) -> QueryResult<PropertyType> {
    let nested = nested_type_expression(parameters, "ANY dynamic union is missing its members")?;
    let rule = parse_property_rule(nested)?;
    Ok(match rule.property_type {
        PropertyType::Union { members } => PropertyType::Union { members },
        member => PropertyType::Union {
            members: vec![member],
        },
    })
}

fn parse_list_property_type(parameters: Option<&AstNode>) -> QueryResult<PropertyType> {
    let nested =
        nested_type_expression(parameters, "LIST property type is missing its element type")?;
    let rule = parse_property_rule(nested)?;
    Ok(PropertyType::List {
        element: Box::new(rule.property_type),
    })
}

fn parse_vector_property_type(type_node: &AstNode) -> QueryResult<PropertyType> {
    let coordinate = type_node
        .children
        .iter()
        .find(|node| node.kind == AstKind::VectorCoordinateTypeName)
        .and_then(|node| node.text.as_deref())
        .map(cypher::unescape_identifier)
        .ok_or_else(|| schema_error("VECTOR property type is missing its coordinate type"))?;
    let dimension = type_node
        .children
        .iter()
        .find(|node| node.kind == AstKind::VectorDimension)
        .and_then(|node| node.text.as_deref())
        .ok_or_else(|| schema_error("VECTOR property type is missing its dimension"))?
        .parse::<u64>()
        .map_err(|_| schema_error("VECTOR dimension is invalid"))?;
    Ok(PropertyType::Vector {
        coordinate: normalize_vector_coordinate(&coordinate),
        dimension,
    })
}

fn simple_property_type(name: &str) -> QueryResult<PropertyType> {
    match name {
        "BOOLEAN" | "BOOL" => Ok(PropertyType::Boolean),
        "INTEGER" | "INT" | "SIGNED INTEGER" => Ok(PropertyType::Integer),
        "FLOAT" => Ok(PropertyType::Float),
        "STRING" | "VARCHAR" => Ok(PropertyType::String),
        "DATE" => Ok(PropertyType::Date),
        "LOCAL TIME" => Ok(PropertyType::LocalTime),
        "ZONED TIME" => Ok(PropertyType::ZonedTime),
        "LOCAL DATETIME" => Ok(PropertyType::LocalDateTime),
        "ZONED DATETIME" => Ok(PropertyType::ZonedDateTime),
        "DURATION" => Ok(PropertyType::Duration),
        "POINT" => Ok(PropertyType::Point),
        "UUID" => Ok(PropertyType::Uuid),
        _ => Err(schema_error(format!(
            "unsupported persistent property type {name}"
        ))),
    }
}

fn term_outer_non_null(term: &AstNode) -> bool {
    term.children
        .last()
        .is_some_and(|node| node.kind == AstKind::TypeNotNull)
}

fn normalize_vector_coordinate(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase()
        .replace("SIGNED", "")
}
