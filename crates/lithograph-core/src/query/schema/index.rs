use std::cmp::Reverse;
use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::cypher::{Value, VectorCoordinateType};
use crate::storage::{
    ConstraintDefinitionKind, HashId, IndexDefinition, IndexTarget, PropertyType, SchemaState,
    SchemaTarget, StandardIndexKind,
};

use super::super::QueryResult;
use super::super::expression::{BinaryOp, Expr, UnaryOp};
use super::super::plan::MatchStep;

mod cache;

pub(crate) use cache::{
    ensure_standard_indexes_for_commit, scan_node_index_after, scan_relationship_index_after,
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StandardIndexSeek {
    pub(crate) index_name: String,
    pub(crate) kind: StandardIndexKind,
    pub(crate) predicates: Vec<(usize, StandardIndexPredicate)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StandardIndexPredicate {
    Lookup,
    Equal(Value),
    In(Vec<Value>),
    Less(Value, bool),
    Greater(Value, bool),
    IsNotNull,
    StartsWith(String),
    EndsWith(String),
    Contains(String),
    WithinBBox {
        lower: Value,
        upper: Value,
    },
    Distance {
        center: Value,
        radius: Value,
        inclusive: bool,
    },
}

pub(crate) fn select_standard_index_seeks(
    connection: &Connection,
    commit: HashId,
    matches: &mut [MatchStep],
    params: &BTreeMap<String, Value>,
) -> QueryResult<()> {
    let schema = SchemaState::load(connection, commit)?;
    for step in matches {
        let candidates = step
            .predicate
            .as_ref()
            .map(|predicate| predicate_candidates(predicate, params))
            .unwrap_or_default();
        for part in &mut step.parts {
            part.start.index_seek = choose_node_seek(
                &schema,
                part.start.variable.as_deref(),
                &part.start.label_names,
                &candidates,
            );
            if let Some(relationship) = part.relationship.as_mut() {
                relationship.index_seek = choose_relationship_seek(
                    &schema,
                    relationship.variable.as_deref(),
                    relationship.type_name.as_deref(),
                    &candidates,
                );
            }
        }
    }
    Ok(())
}

fn choose_node_seek(
    schema: &SchemaState,
    variable: Option<&str>,
    labels: &[String],
    candidates: &[(String, String, StandardIndexPredicate)],
) -> Option<StandardIndexSeek> {
    if let Some(seek) = best_property_seek(
        schema,
        variable,
        candidates,
        PropertySeekScope::Node(labels),
    ) {
        return Some(seek);
    }
    if labels.is_empty() {
        return None;
    }
    lookup_seek(schema, |target| matches!(target, IndexTarget::NodeLookup))
}

fn choose_relationship_seek(
    schema: &SchemaState,
    variable: Option<&str>,
    relationship_type: Option<&str>,
    candidates: &[(String, String, StandardIndexPredicate)],
) -> Option<StandardIndexSeek> {
    if let Some(seek) = best_property_seek(
        schema,
        variable,
        candidates,
        PropertySeekScope::Relationship(relationship_type),
    ) {
        return Some(seek);
    }
    relationship_type?;
    lookup_seek(schema, |target| {
        matches!(target, IndexTarget::RelationshipLookup)
    })
}

#[derive(Clone, Copy)]
enum PropertySeekScope<'a> {
    Node(&'a [String]),
    Relationship(Option<&'a str>),
}

fn best_property_seek(
    schema: &SchemaState,
    variable: Option<&str>,
    candidates: &[(String, String, StandardIndexPredicate)],
    scope: PropertySeekScope<'_>,
) -> Option<StandardIndexSeek> {
    let mut selected = schema
        .indexes
        .values()
        .filter_map(|index| {
            choose_property_seek(
                schema,
                index,
                variable,
                scoped_index_properties(index, scope)?,
                candidates,
            )
        })
        .collect::<Vec<_>>();
    selected.sort_by_key(index_seek_priority);
    selected.into_iter().next()
}

fn scoped_index_properties<'a>(
    index: &'a IndexDefinition,
    scope: PropertySeekScope<'_>,
) -> Option<&'a [String]> {
    match (&index.target, scope) {
        (IndexTarget::NodeProperties { label, properties }, PropertySeekScope::Node(labels))
            if labels.contains(label) =>
        {
            Some(properties)
        }
        (
            IndexTarget::RelationshipProperties {
                relationship_type,
                properties,
            },
            PropertySeekScope::Relationship(expected),
        ) if expected == Some(relationship_type.as_str()) => Some(properties),
        _ => None,
    }
}

fn lookup_seek(
    schema: &SchemaState,
    target_matches: impl Fn(&IndexTarget) -> bool,
) -> Option<StandardIndexSeek> {
    schema
        .indexes
        .values()
        .find(|index| index.kind == StandardIndexKind::Lookup && target_matches(&index.target))
        .map(|index| StandardIndexSeek {
            index_name: index.name.clone(),
            kind: StandardIndexKind::Lookup,
            predicates: vec![(0, StandardIndexPredicate::Lookup)],
        })
}

fn choose_property_seek(
    schema: &SchemaState,
    index: &IndexDefinition,
    variable: Option<&str>,
    properties: &[String],
    candidates: &[(String, String, StandardIndexPredicate)],
) -> Option<StandardIndexSeek> {
    let variable = variable?;
    let mut predicates = Vec::with_capacity(properties.len());
    for (ordinal, property) in properties.iter().enumerate() {
        let predicate = candidates
            .iter()
            .filter(|(candidate_variable, candidate_property, predicate)| {
                candidate_variable == variable
                    && candidate_property == property
                    && predicate_supported(schema, index, property, predicate)
            })
            .map(|(_, _, predicate)| predicate.clone())
            .min_by_key(predicate_priority)?;
        predicates.push((ordinal, predicate));
    }
    Some(StandardIndexSeek {
        index_name: index.name.clone(),
        kind: index.kind,
        predicates,
    })
}

fn index_seek_priority(seek: &StandardIndexSeek) -> (u8, Reverse<usize>, String) {
    let predicate = &seek.predicates[0].1;
    let kind = match (seek.kind, predicate) {
        (
            StandardIndexKind::Text,
            StandardIndexPredicate::EndsWith(_) | StandardIndexPredicate::Contains(_),
        ) => 0,
        (StandardIndexKind::Range, _) | (StandardIndexKind::Point, _) => 1,
        (StandardIndexKind::Text, _) => 2,
        (StandardIndexKind::Lookup, _) => 3,
    };
    (
        kind,
        Reverse(seek.predicates.len()),
        seek.index_name.clone(),
    )
}

fn predicate_priority(predicate: &StandardIndexPredicate) -> u8 {
    match predicate {
        StandardIndexPredicate::Equal(_) => 0,
        StandardIndexPredicate::In(_) => 1,
        StandardIndexPredicate::WithinBBox { .. } | StandardIndexPredicate::Distance { .. } => 2,
        StandardIndexPredicate::Less(_, _) | StandardIndexPredicate::Greater(_, _) => 3,
        StandardIndexPredicate::StartsWith(_) => 4,
        StandardIndexPredicate::EndsWith(_) | StandardIndexPredicate::Contains(_) => 5,
        StandardIndexPredicate::IsNotNull => 6,
        StandardIndexPredicate::Lookup => 7,
    }
}

fn predicate_supported(
    schema: &SchemaState,
    index: &IndexDefinition,
    property: &str,
    predicate: &StandardIndexPredicate,
) -> bool {
    match (index.kind, predicate) {
        (StandardIndexKind::Range, StandardIndexPredicate::Equal(value)) => {
            range_equality_value(schema, index, property, value)
        }
        (StandardIndexKind::Range, StandardIndexPredicate::In(values)) => {
            !values.is_empty()
                && values
                    .iter()
                    .all(|value| range_equality_value(schema, index, property, value))
        }
        (
            StandardIndexKind::Range,
            StandardIndexPredicate::Less(value, _) | StandardIndexPredicate::Greater(value, _),
        ) => {
            range_order_value(value)
                && property_type_proves(schema, index, property, |property_type| {
                    property_type_order_compatible(property_type, value)
                })
        }
        (StandardIndexKind::Range, StandardIndexPredicate::IsNotNull) => true,
        (StandardIndexKind::Range, StandardIndexPredicate::StartsWith(_)) => {
            property_type_proves(schema, index, property, |property_type| {
                matches!(property_type, PropertyType::String)
            })
        }
        (StandardIndexKind::Point, StandardIndexPredicate::Equal(Value::Point(_))) => true,
        (StandardIndexKind::Point, StandardIndexPredicate::In(values)) => {
            !values.is_empty() && values.iter().all(|value| matches!(value, Value::Point(_)))
        }
        (StandardIndexKind::Point, StandardIndexPredicate::WithinBBox { lower, upper }) => {
            matches!((lower, upper), (Value::Point(_), Value::Point(_)))
                && property_type_proves(schema, index, property, |property_type| {
                    matches!(property_type, PropertyType::Point)
                })
        }
        (StandardIndexKind::Point, StandardIndexPredicate::Distance { center, radius, .. }) => {
            matches!(center, Value::Point(_))
                && matches!(radius, Value::Integer(_) | Value::Float(_))
                && property_type_proves(schema, index, property, |property_type| {
                    matches!(property_type, PropertyType::Point)
                })
        }
        (StandardIndexKind::Text, StandardIndexPredicate::Equal(Value::String(_))) => true,
        (
            StandardIndexKind::Text,
            StandardIndexPredicate::StartsWith(_)
            | StandardIndexPredicate::EndsWith(_)
            | StandardIndexPredicate::Contains(_),
        ) => property_type_proves(schema, index, property, |property_type| {
            matches!(property_type, PropertyType::String)
        }),
        (StandardIndexKind::Text, StandardIndexPredicate::In(values)) => {
            !values.is_empty() && values.iter().all(|value| matches!(value, Value::String(_)))
        }
        _ => false,
    }
}

fn property_type_proves(
    schema: &SchemaState,
    index: &IndexDefinition,
    property: &str,
    accepts: impl Fn(&PropertyType) -> bool,
) -> bool {
    schema.constraints.values().any(|constraint| {
        constraint.properties.len() == 1
            && constraint.properties[0] == property
            && constraint_target_matches_index(&constraint.target, &index.target)
            && matches!(
                &constraint.kind,
                ConstraintDefinitionKind::Type { rule }
                    if property_type_all_members_match(&rule.property_type, &accepts)
            )
    })
}

fn constraint_target_matches_index(target: &SchemaTarget, index_target: &IndexTarget) -> bool {
    matches!(
        (target, index_target),
        (
            SchemaTarget::Node { label: constraint_label },
            IndexTarget::NodeProperties { label: index_label, .. }
        ) if constraint_label == index_label
    ) || matches!(
        (target, index_target),
        (
            SchemaTarget::Relationship {
                relationship_type: constraint_type,
            },
            IndexTarget::RelationshipProperties {
                relationship_type: index_type,
                ..
            }
        ) if constraint_type == index_type
    )
}

fn property_type_all_members_match(
    property_type: &PropertyType,
    accepts: &impl Fn(&PropertyType) -> bool,
) -> bool {
    match property_type {
        PropertyType::Union { members } => {
            !members.is_empty()
                && members
                    .iter()
                    .all(|member| property_type_all_members_match(member, accepts))
        }
        property_type => accepts(property_type),
    }
}

fn property_type_order_compatible(property_type: &PropertyType, value: &Value) -> bool {
    match value {
        Value::Integer(_) | Value::Float(_) => {
            matches!(property_type, PropertyType::Integer | PropertyType::Float)
        }
        Value::Boolean(_) => matches!(property_type, PropertyType::Boolean),
        Value::String(_) => matches!(property_type, PropertyType::String),
        Value::Date(_) => matches!(property_type, PropertyType::Date),
        Value::LocalTime(_) => matches!(property_type, PropertyType::LocalTime),
        Value::Time(_) => matches!(property_type, PropertyType::ZonedTime),
        Value::LocalDateTime(_) => matches!(property_type, PropertyType::LocalDateTime),
        Value::ZonedDateTime(_) => matches!(property_type, PropertyType::ZonedDateTime),
        _ => false,
    }
}

fn range_equality_value(
    schema: &SchemaState,
    index: &IndexDefinition,
    property: &str,
    value: &Value,
) -> bool {
    if !value.is_property_value() {
        return false;
    }
    let Value::Vector(vector) = value else {
        return true;
    };
    property_type_proves(schema, index, property, |property_type| {
        vector_equality_compatible(property_type, vector.coordinate_type(), vector.dimension())
    })
}

fn vector_equality_compatible(
    property_type: &PropertyType,
    coordinate_type: VectorCoordinateType,
    dimension: usize,
) -> bool {
    let PropertyType::Vector {
        coordinate,
        dimension: constrained_dimension,
    } = property_type
    else {
        return !matches!(property_type, PropertyType::Any);
    };
    VectorCoordinateType::parse(coordinate) == Some(coordinate_type)
        && usize::try_from(*constrained_dimension).ok() == Some(dimension)
}

fn range_order_value(value: &Value) -> bool {
    matches!(
        value,
        Value::Boolean(_)
            | Value::Integer(_)
            | Value::Float(_)
            | Value::String(_)
            | Value::Date(_)
            | Value::LocalTime(_)
            | Value::Time(_)
            | Value::LocalDateTime(_)
            | Value::ZonedDateTime(_)
    )
}

fn predicate_candidates(
    expression: &Expr,
    params: &BTreeMap<String, Value>,
) -> Vec<(String, String, StandardIndexPredicate)> {
    let mut candidates = Vec::new();
    collect_predicate_candidates(expression, params, &mut candidates);
    candidates
}

fn collect_predicate_candidates(
    expression: &Expr,
    params: &BTreeMap<String, Value>,
    output: &mut Vec<(String, String, StandardIndexPredicate)>,
) {
    if let Expr::Binary(BinaryOp::And, left, right) = expression {
        collect_predicate_candidates(left, params, output);
        collect_predicate_candidates(right, params, output);
        return;
    }
    if collect_special_predicate_candidate(expression, params, output) {
        return;
    }
    let Expr::Binary(operator, left, right) = expression else {
        return;
    };
    collect_binary_predicate_candidate(*operator, left, right, params, output);
}

fn collect_special_predicate_candidate(
    expression: &Expr,
    params: &BTreeMap<String, Value>,
    output: &mut Vec<(String, String, StandardIndexPredicate)>,
) -> bool {
    if let Expr::IsNull {
        value,
        negated: true,
    } = expression
        && let Some((variable, property)) = property_name(value)
    {
        output.push((variable, property, StandardIndexPredicate::IsNotNull));
        return true;
    }
    if let Some((variable, property, lower, upper)) = point_bbox_candidate(expression, params) {
        output.push((
            variable,
            property,
            StandardIndexPredicate::WithinBBox { lower, upper },
        ));
        return true;
    }
    if let Some((variable, property, center, radius, inclusive)) =
        point_distance_candidate(expression, params)
    {
        output.push((
            variable,
            property,
            StandardIndexPredicate::Distance {
                center,
                radius,
                inclusive,
            },
        ));
        return true;
    }
    false
}

fn collect_binary_predicate_candidate(
    operator: BinaryOp,
    left: &Expr,
    right: &Expr,
    params: &BTreeMap<String, Value>,
    output: &mut Vec<(String, String, StandardIndexPredicate)>,
) {
    match operator {
        BinaryOp::Equal => collect_equality_candidate(left, right, params, output),
        BinaryOp::In => collect_in_candidate(left, right, params, output),
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            collect_order_candidate(operator, left, right, params, output);
        }
        BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains => {
            collect_text_candidate(operator, left, right, params, output);
        }
        _ => {}
    }
}

fn collect_equality_candidate(
    left: &Expr,
    right: &Expr,
    params: &BTreeMap<String, Value>,
    output: &mut Vec<(String, String, StandardIndexPredicate)>,
) {
    let candidate =
        property_constant(left, right, params).or_else(|| property_constant(right, left, params));
    if let Some((property, value)) = candidate {
        output.push((property.0, property.1, StandardIndexPredicate::Equal(value)));
    }
}

fn collect_in_candidate(
    left: &Expr,
    right: &Expr,
    params: &BTreeMap<String, Value>,
    output: &mut Vec<(String, String, StandardIndexPredicate)>,
) {
    if let Some((variable, property)) = property_name(left)
        && let Some(Value::List(values)) = constant_value(right, params)
    {
        output.push((variable, property, StandardIndexPredicate::In(values)));
    }
}

fn collect_order_candidate(
    operator: BinaryOp,
    left: &Expr,
    right: &Expr,
    params: &BTreeMap<String, Value>,
    output: &mut Vec<(String, String, StandardIndexPredicate)>,
) {
    if let Some((property, value)) = property_constant(left, right, params) {
        output.push((
            property.0,
            property.1,
            direct_order_predicate(operator, value),
        ));
    } else if let Some((property, value)) = property_constant(right, left, params) {
        output.push((
            property.0,
            property.1,
            reversed_order_predicate(operator, value),
        ));
    }
}

fn direct_order_predicate(operator: BinaryOp, value: Value) -> StandardIndexPredicate {
    match operator {
        BinaryOp::Less => StandardIndexPredicate::Less(value, false),
        BinaryOp::LessEqual => StandardIndexPredicate::Less(value, true),
        BinaryOp::Greater => StandardIndexPredicate::Greater(value, false),
        BinaryOp::GreaterEqual => StandardIndexPredicate::Greater(value, true),
        _ => unreachable!("order predicate helper only accepts comparison operators"),
    }
}

fn reversed_order_predicate(operator: BinaryOp, value: Value) -> StandardIndexPredicate {
    match operator {
        BinaryOp::Less => StandardIndexPredicate::Greater(value, false),
        BinaryOp::LessEqual => StandardIndexPredicate::Greater(value, true),
        BinaryOp::Greater => StandardIndexPredicate::Less(value, false),
        BinaryOp::GreaterEqual => StandardIndexPredicate::Less(value, true),
        _ => unreachable!("order predicate helper only accepts comparison operators"),
    }
}

fn collect_text_candidate(
    operator: BinaryOp,
    left: &Expr,
    right: &Expr,
    params: &BTreeMap<String, Value>,
    output: &mut Vec<(String, String, StandardIndexPredicate)>,
) {
    let Some((variable, property)) = property_name(left) else {
        return;
    };
    let Some(Value::String(value)) = constant_value(right, params) else {
        return;
    };
    let predicate = match operator {
        BinaryOp::StartsWith => StandardIndexPredicate::StartsWith(value),
        BinaryOp::EndsWith => StandardIndexPredicate::EndsWith(value),
        BinaryOp::Contains => StandardIndexPredicate::Contains(value),
        _ => unreachable!("text predicate helper only accepts text operators"),
    };
    output.push((variable, property, predicate));
}

fn point_bbox_candidate(
    expression: &Expr,
    params: &BTreeMap<String, Value>,
) -> Option<(String, String, Value, Value)> {
    let Expr::Function { name, args, .. } = expression else {
        return None;
    };
    if !name.eq_ignore_ascii_case("point.withinbbox") || args.len() != 3 {
        return None;
    }
    let (variable, property) = property_name(&args[0])?;
    Some((
        variable,
        property,
        constant_value(&args[1], params)?,
        constant_value(&args[2], params)?,
    ))
}

fn point_distance_candidate(
    expression: &Expr,
    params: &BTreeMap<String, Value>,
) -> Option<(String, String, Value, Value, bool)> {
    let Expr::Binary(operator, left, right) = expression else {
        return None;
    };
    let (function, radius, inclusive) = match operator {
        BinaryOp::Less => (left.as_ref(), right.as_ref(), false),
        BinaryOp::LessEqual => (left.as_ref(), right.as_ref(), true),
        BinaryOp::Greater => (right.as_ref(), left.as_ref(), false),
        BinaryOp::GreaterEqual => (right.as_ref(), left.as_ref(), true),
        _ => return None,
    };
    let Expr::Function { name, args, .. } = function else {
        return None;
    };
    if !name.eq_ignore_ascii_case("point.distance") || args.len() != 2 {
        return None;
    }
    let (variable, property, center) = if let Some((variable, property)) = property_name(&args[0]) {
        (variable, property, constant_value(&args[1], params)?)
    } else {
        let (variable, property) = property_name(&args[1])?;
        (variable, property, constant_value(&args[0], params)?)
    };
    Some((
        variable,
        property,
        center,
        constant_value(radius, params)?,
        inclusive,
    ))
}

fn property_constant(
    property: &Expr,
    constant: &Expr,
    params: &BTreeMap<String, Value>,
) -> Option<((String, String), Value)> {
    Some((property_name(property)?, constant_value(constant, params)?))
}

fn property_name(expression: &Expr) -> Option<(String, String)> {
    let Expr::Property(base, property) = expression else {
        return None;
    };
    let Expr::Variable(variable) = base.as_ref() else {
        return None;
    };
    Some((variable.clone(), property.clone()))
}

fn constant_value(expression: &Expr, params: &BTreeMap<String, Value>) -> Option<Value> {
    match expression {
        Expr::Literal(value) => Some(value.clone()),
        Expr::Parameter(name) => params.get(name).cloned(),
        Expr::Unary(UnaryOp::Positive, value) => constant_value(value, params),
        Expr::Unary(UnaryOp::Negative, value) => match constant_value(value, params)? {
            Value::Integer(value) => value.checked_neg().map(Value::Integer),
            Value::Float(value) => Some(Value::Float(-value)),
            _ => None,
        },
        Expr::List(values) => values
            .iter()
            .map(|value| constant_value(value, params))
            .collect::<Option<Vec<_>>>()
            .map(Value::List),
        Expr::Map(entries) => entries
            .iter()
            .map(|(key, value)| Some((key.clone(), constant_value(value, params)?)))
            .collect::<Option<BTreeMap<_, _>>>()
            .map(Value::Map),
        Expr::Function {
            name,
            args,
            distinct: false,
            star: false,
        } => {
            let values = args
                .iter()
                .map(|argument| constant_value(argument, params))
                .collect::<Option<Vec<_>>>()?;
            super::super::functions::evaluate(name, &values)?.ok()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_extraction_handles_equality_and_text_operations() {
        let equality = Expr::Binary(
            BinaryOp::Equal,
            Box::new(Expr::Property(
                Box::new(Expr::Variable("n".to_owned())),
                "name".to_owned(),
            )),
            Box::new(Expr::Literal(Value::String("Alice".to_owned()))),
        );
        assert_eq!(
            predicate_candidates(&equality, &BTreeMap::new()),
            vec![(
                "n".to_owned(),
                "name".to_owned(),
                StandardIndexPredicate::Equal(Value::String("Alice".to_owned()))
            )]
        );
    }
}
