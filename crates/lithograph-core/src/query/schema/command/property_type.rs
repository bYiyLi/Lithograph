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
    let rules = terms
        .into_iter()
        .map(parse_property_type_rule)
        .collect::<QueryResult<Vec<_>>>()?;
    if let [rule] = rules.as_slice() {
        return Ok(normalize_property_rule(rule.clone()));
    }
    let required = rules.iter().all(|rule| rule.required);
    let members = rules.into_iter().map(|rule| rule.property_type).collect();
    Ok(normalize_property_rule(PropertyRule {
        property_type: PropertyType::Union { members },
        required,
    }))
}

fn parse_property_type_rule(term: &AstNode) -> QueryResult<PropertyRule> {
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
        return parse_dynamic_union_rule(parameters);
    }
    let property_type = if name == "LIST" || name == "ARRAY" {
        parse_list_property_type(parameters)?
    } else if suffixes == 1 {
        PropertyType::List {
            element: Box::new(simple_property_type(&name)?),
        }
    } else {
        match name.as_str() {
            "VECTOR" => parse_vector_property_type(type_node)?,
            "ANY" => PropertyType::Any,
            _ => simple_property_type(&name)?,
        }
    };
    Ok(PropertyRule {
        property_type,
        required: term_outer_non_null(term),
    })
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

fn parse_dynamic_union_rule(parameters: Option<&AstNode>) -> QueryResult<PropertyRule> {
    let nested = nested_type_expression(parameters, "ANY dynamic union is missing its members")?;
    parse_property_rule(nested)
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
    let normalized = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase()
        .replace("SIGNED", "");
    match normalized.as_str() {
        "I8" | "INT8" | "INTEGER8" => "INTEGER8".to_owned(),
        "I16" | "INT16" | "INTEGER16" => "INTEGER16".to_owned(),
        "I32" | "INT32" | "INTEGER32" => "INTEGER32".to_owned(),
        "I64" | "INT" | "INT64" | "INTEGER" | "INTEGER64" => "INTEGER64".to_owned(),
        "F32" | "FLOAT32" => "FLOAT32".to_owned(),
        "F64" | "FLOAT" | "FLOAT64" => "FLOAT64".to_owned(),
        _ => normalized,
    }
}

fn normalize_property_rule(rule: PropertyRule) -> PropertyRule {
    PropertyRule {
        property_type: normalize_property_type(rule.property_type),
        required: rule.required,
    }
}

fn normalize_property_type(property_type: PropertyType) -> PropertyType {
    match property_type {
        PropertyType::List { element } => PropertyType::List {
            element: Box::new(normalize_property_type(*element)),
        },
        PropertyType::Union { members } => normalize_union_members(members),
        property_type => property_type,
    }
}

fn normalize_union_members(members: Vec<PropertyType>) -> PropertyType {
    let mut flattened = Vec::new();
    for member in members.into_iter().map(normalize_property_type) {
        match member {
            PropertyType::Union { members } => flattened.extend(members),
            member => flattened.push(member),
        }
    }
    flattened.sort_by_key(property_type_sort_key);
    flattened.dedup();
    let members = flattened
        .iter()
        .enumerate()
        .filter(|(candidate_index, candidate)| {
            !flattened.iter().enumerate().any(|(other_index, other)| {
                candidate_index != &other_index && property_type_subsumes(other, candidate)
            })
        })
        .map(|(_, member)| member.clone())
        .collect::<Vec<_>>();
    match members.as_slice() {
        [member] => member.clone(),
        _ => PropertyType::Union { members },
    }
}

fn property_type_subsumes(container: &PropertyType, candidate: &PropertyType) -> bool {
    if container == candidate || matches!(container, PropertyType::Any) {
        return true;
    }
    match (container, candidate) {
        (PropertyType::List { element: left }, PropertyType::List { element: right }) => {
            property_type_subsumes(left, right)
        }
        (
            PropertyType::Union { members },
            PropertyType::Union {
                members: candidates,
            },
        ) => candidates.iter().all(|candidate| {
            members
                .iter()
                .any(|member| property_type_subsumes(member, candidate))
        }),
        (PropertyType::Union { members }, candidate) => members
            .iter()
            .any(|member| property_type_subsumes(member, candidate)),
        (container, PropertyType::Union { members }) => members
            .iter()
            .all(|member| property_type_subsumes(container, member)),
        _ => false,
    }
}

fn property_type_sort_key(property_type: &PropertyType) -> String {
    match property_type {
        PropertyType::Boolean => "00".to_owned(),
        PropertyType::String => "01".to_owned(),
        PropertyType::Uuid => "02".to_owned(),
        PropertyType::Integer => "03".to_owned(),
        PropertyType::Float => "04".to_owned(),
        PropertyType::Date => "05".to_owned(),
        PropertyType::LocalTime => "06".to_owned(),
        PropertyType::ZonedTime => "07".to_owned(),
        PropertyType::LocalDateTime => "08".to_owned(),
        PropertyType::ZonedDateTime => "09".to_owned(),
        PropertyType::Duration => "10".to_owned(),
        PropertyType::Point => "11".to_owned(),
        PropertyType::Vector {
            coordinate,
            dimension,
        } => format!(
            "12:{:02}:{dimension:020}",
            vector_coordinate_order(coordinate)
        ),
        PropertyType::List { element } => format!("13:{}", property_type_sort_key(element)),
        PropertyType::Union { members } => format!(
            "14:{}",
            members
                .iter()
                .map(property_type_sort_key)
                .collect::<Vec<_>>()
                .join("|")
        ),
        PropertyType::Any => "15".to_owned(),
    }
}

fn vector_coordinate_order(coordinate: &str) -> u8 {
    match coordinate {
        "INTEGER8" => 0,
        "INTEGER16" => 1,
        "INTEGER32" => 2,
        "INTEGER64" => 3,
        "FLOAT32" => 4,
        "FLOAT64" => 5,
        _ => u8::MAX,
    }
}
