use std::collections::{BTreeMap, BTreeSet};

use super::controlled::{clone_string_controlled, value_bytes_with_control};
use crate::cypher::{AstNode, PathValue, Value};
use crate::query::access::PropertyRules;
use crate::query::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, map_entry_bytes,
};
use crate::query::{QueryError, QueryErrorKind, QueryResult};
use crate::storage::Snapshot;

use super::operators::evaluate_unary;
use super::{
    BinaryOp, BindingRow, BindingValue, Expr, InterpolatedPart, TypeKind, TypeSpec, TypeTermSpec,
    compile_expression,
};

#[derive(Clone, Copy)]
struct EvalContext<'data, 'connection> {
    snapshot: &'data Snapshot<'connection>,
    row: &'data BindingRow,
    params: &'data BTreeMap<String, Value>,
    aliases: &'data BTreeMap<String, Value>,
    query_access: &'data PropertyRules,
    control: Option<&'data ExecutionControl<'data>>,
}

impl<'data, 'connection> EvalContext<'data, 'connection> {
    fn control(self) -> QueryResult<&'data ExecutionControl<'data>> {
        self.control
            .ok_or_else(|| QueryError::internal("expression execution control is missing"))
    }

    fn evaluate(self, expression: &Expr) -> QueryResult<EvaluatedValue> {
        evaluate_in_context(expression, self)
    }

    fn with_aliases<'scope>(
        self,
        aliases: &'scope BTreeMap<String, Value>,
    ) -> EvalContext<'scope, 'connection>
    where
        'data: 'scope,
    {
        EvalContext {
            snapshot: self.snapshot,
            row: self.row,
            params: self.params,
            aliases,
            query_access: self.query_access,
            control: self.control,
        }
    }
}

#[derive(Debug)]
pub(crate) struct EvaluatedValue {
    pub(crate) value: Value,
    pub(crate) reservation: MemoryReservation,
}

#[cfg(test)]
#[path = "evaluate_tests.rs"]
mod tests;

pub(crate) fn evaluate_with_access_control(
    expression: &Expr,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<EvaluatedValue> {
    evaluate_with_aliases_and_access_control(
        expression,
        snapshot,
        row,
        params,
        &BTreeMap::new(),
        query_access,
        control,
    )
}

pub(crate) fn evaluate_with_aliases_and_access_control(
    expression: &Expr,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
    aliases: &BTreeMap<String, Value>,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<EvaluatedValue> {
    control.check()?;
    evaluate_in_context(
        expression,
        EvalContext {
            snapshot,
            row,
            params,
            aliases,
            query_access,
            control: Some(control),
        },
    )
}

fn scalar(value: Value, context: EvalContext<'_, '_>) -> QueryResult<EvaluatedValue> {
    Ok(EvaluatedValue {
        value,
        reservation: context
            .control()?
            .reserve(std::mem::size_of::<Value>() as u64)?,
    })
}

fn evaluate_in_context(
    expression: &Expr,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let control = context.control()?;
    control.work(1)?;
    match expression {
        Expr::Literal(value) => clone_filtered_value(value, context),
        Expr::List(items) => evaluate_list(items, context),
        Expr::Map(entries) => evaluate_map(entries, context),
        Expr::Variable(name) => match context.aliases.get(name) {
            Some(value) => clone_filtered_value(value, context),
            None => binding_value_with_memory(
                context.snapshot,
                context.row.values.get(name).unwrap_or(&BindingValue::Null),
                context.query_access,
                control,
            ),
        },
        Expr::Parameter(name) => {
            clone_filtered_value(context.params.get(name).unwrap_or(&Value::Null), context)
        }
        Expr::Property(base, key) => evaluate_property(base, key, context),
        Expr::Function { name, args, .. } => evaluate_function(name, args, context),
        Expr::Case {
            operand,
            alternatives,
            fallback,
        } => evaluate_case_in_context(
            operand.as_deref(),
            alternatives,
            fallback.as_deref(),
            context,
        ),
        _ => evaluate_collection_expression(expression, context),
    }
}

fn evaluate_list(items: &[Expr], context: EvalContext<'_, '_>) -> QueryResult<EvaluatedValue> {
    let control = context.control()?;
    let mut reservation = control.reserve(checked_add(
        allocation_bytes::<Value>(items.len())?,
        std::mem::size_of::<Value>() as u64,
    )?)?;
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        let value = context.evaluate(item)?;
        reservation.merge(value.reservation)?;
        values.push(value.value);
    }
    Ok(EvaluatedValue {
        value: Value::List(values),
        reservation,
    })
}

fn evaluate_map(
    entries: &BTreeMap<String, Expr>,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let control = context.control()?;
    let mut reservation = control.reserve(std::mem::size_of::<Value>() as u64)?;
    let mut values = BTreeMap::new();
    for (key, expression) in entries {
        reservation.grow(checked_add(key.len() as u64, map_entry_bytes::<Value>()?)?)?;
        let value = context.evaluate(expression)?;
        reservation.merge(value.reservation)?;
        values.insert(clone_string_controlled(key, Some(control))?, value.value);
    }
    Ok(EvaluatedValue {
        value: Value::Map(values),
        reservation,
    })
}

fn evaluate_case_in_context(
    operand: Option<&Expr>,
    alternatives: &[(Expr, Expr)],
    fallback: Option<&Expr>,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let operand = operand
        .map(|expression| context.evaluate(expression))
        .transpose()?;
    for (condition, result) in alternatives {
        let condition = context.evaluate(condition)?;
        let take = match &operand {
            Some(operand) => super::operators::cypher_equals_controlled(
                &operand.value,
                &condition.value,
                context.control,
            )?
            .unwrap_or(false),
            None => match condition.value {
                Value::Boolean(value) => value,
                Value::Null => false,
                _ => {
                    return Err(QueryError::new(
                        QueryErrorKind::Type,
                        "searched CASE condition must be Boolean or null",
                    ));
                }
            },
        };
        if take {
            return context.evaluate(result);
        }
    }
    fallback.map_or_else(
        || scalar(Value::Null, context),
        |expression| context.evaluate(expression),
    )
}

fn clone_filtered_value(
    value: &Value,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let control = context.control()?;
    let reservation = control.reserve(value_bytes_with_control(value, Some(control))?)?;
    let value = super::controlled::clone_filtered_value_unreserved_controlled(
        value,
        context.query_access,
        Some(control),
    )?;
    Ok(EvaluatedValue { value, reservation })
}

pub(crate) fn binding_value_with_memory(
    snapshot: &Snapshot<'_>,
    value: &BindingValue,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<EvaluatedValue> {
    control.check()?;
    match value {
        BindingValue::Scalar(value) => {
            let reservation = control.reserve(value_bytes_with_control(value, Some(control))?)?;
            let value = super::controlled::clone_filtered_value_unreserved_controlled(
                value,
                query_access,
                Some(control),
            )?;
            Ok(EvaluatedValue { value, reservation })
        }
        BindingValue::Node(id) => {
            let (value, reservation) = crate::query::graph::materialize_node_with_control(
                snapshot,
                *id,
                query_access,
                control,
            )?;
            Ok(EvaluatedValue {
                value: Value::Node(value),
                reservation,
            })
        }
        BindingValue::Relationship(record) => {
            let (value, reservation) = crate::query::graph::materialize_relationship_with_control(
                snapshot,
                *record,
                query_access,
                control,
            )?;
            Ok(EvaluatedValue {
                value: Value::Relationship(value),
                reservation,
            })
        }
        BindingValue::Path {
            nodes,
            relationships,
        } => binding_path_with_memory(snapshot, nodes, relationships, query_access, control),
        BindingValue::Null => Ok(EvaluatedValue {
            value: Value::Null,
            reservation: control.reserve(std::mem::size_of::<Value>() as u64)?,
        }),
    }
}

fn binding_path_with_memory(
    snapshot: &Snapshot<'_>,
    nodes: &[i64],
    relationships: &[crate::storage::RelationshipRecord],
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<EvaluatedValue> {
    let mut reservation = control.reserve(checked_add(
        checked_add(
            allocation_bytes::<crate::cypher::NodeValue>(nodes.len())?,
            allocation_bytes::<crate::cypher::RelationshipValue>(relationships.len())?,
        )?,
        std::mem::size_of::<Value>() as u64,
    )?)?;
    let mut output_nodes = Vec::with_capacity(nodes.len());
    let mut output_relationships = Vec::with_capacity(relationships.len());
    for id in nodes {
        control.work(1)?;
        let (value, memory) = crate::query::graph::materialize_node_with_control(
            snapshot,
            *id,
            query_access,
            control,
        )?;
        reservation.merge(memory)?;
        output_nodes.push(value);
    }
    for record in relationships {
        control.work(1)?;
        let (value, memory) = crate::query::graph::materialize_relationship_with_control(
            snapshot,
            *record,
            query_access,
            control,
        )?;
        reservation.merge(memory)?;
        output_relationships.push(value);
    }
    Ok(EvaluatedValue {
        value: Value::Path(PathValue {
            nodes: output_nodes,
            relationships: output_relationships,
        }),
        reservation,
    })
}

fn evaluate_collection_expression(
    expression: &Expr,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    match expression {
        Expr::ListComprehension { .. }
        | Expr::ListPredicate { .. }
        | Expr::Reduce { .. }
        | Expr::AllReduce { .. } => {
            let mut reservation = context
                .control()?
                .reserve(std::mem::size_of::<Value>() as u64)?;
            let mut result_memory = None;
            let value = super::scoped::evaluate_scoped_expression_with_memory(
                expression,
                context.aliases,
                &mut |expression, aliases| context.with_aliases(aliases).evaluate(expression),
                context.control()?,
                &mut result_memory,
            )?;
            if let Some(memory) = result_memory {
                reservation.merge(memory)?;
            }
            Ok(EvaluatedValue { value, reservation })
        }
        Expr::MapProjection {
            base,
            include_all,
            entries,
        } => evaluate_map_projection(base, *include_all, entries, context),
        _ => evaluate_postfix_expression(expression, context),
    }
}

fn evaluate_postfix_expression(
    expression: &Expr,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    match expression {
        Expr::Subscript {
            base,
            start,
            end,
            slice,
        } => {
            let base = context.evaluate(base)?;
            let start = start
                .as_deref()
                .map(|value| context.evaluate(value))
                .transpose()?;
            let end = end
                .as_deref()
                .map(|value| context.evaluate(value))
                .transpose()?;
            evaluate_subscript_tracked(base, start, end, *slice, context)
        }
        Expr::IsNull { .. }
        | Expr::NormalizedPredicate { .. }
        | Expr::TypePredicate { .. }
        | Expr::LabelPredicate { .. } => evaluate_predicate_expression(expression, context),
        Expr::Interpolated(parts) => evaluate_interpolated(parts, context),
        Expr::Unary(op, value) => {
            let value = context.evaluate(value)?;
            Ok(EvaluatedValue {
                value: evaluate_unary(*op, value.value)?,
                reservation: value.reservation,
            })
        }
        Expr::Binary(BinaryOp::And, left, right) => evaluate_boolean(left, right, true, context),
        Expr::Binary(BinaryOp::Or, left, right) => evaluate_boolean(left, right, false, context),
        Expr::Binary(op, left, right) => {
            let left = context.evaluate(left)?;
            let right = context.evaluate(right)?;
            let mut reservation = context
                .control()?
                .reserve(std::mem::size_of::<Value>() as u64)?;
            let mut result_memory = None;
            let value = super::operators::evaluate_binary_with_memory(
                *op,
                left.value,
                right.value,
                context.control,
                &mut result_memory,
            )?;
            if let Some(memory) = result_memory {
                reservation.merge(memory)?;
            }
            Ok(EvaluatedValue { value, reservation })
        }
        Expr::PatternPredicate(_) | Expr::PatternComprehension(_) | Expr::Subquery { .. } => Err(
            QueryError::internal("graph expression reached scalar evaluation"),
        ),
        _ => Err(QueryError::internal(
            "expression dispatch reached an inconsistent evaluator branch",
        )),
    }
}

fn evaluate_boolean(
    left: &Expr,
    right: &Expr,
    and: bool,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let left = context.evaluate(left)?;
    let left = match left.value {
        Value::Boolean(value) => Some(value),
        Value::Null => None,
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "Boolean operator requires Boolean or null",
            ));
        }
    };
    if left == Some(!and) {
        return scalar(Value::Boolean(!and), context);
    }
    let right = context.evaluate(right)?;
    let value = match right.value {
        Value::Boolean(value) if value == !and => Value::Boolean(value),
        Value::Boolean(value) if left.is_some() => Value::Boolean(value),
        Value::Boolean(_) | Value::Null => Value::Null,
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "Boolean operator requires Boolean or null",
            ));
        }
    };
    scalar(value, context)
}

fn evaluate_predicate_expression(
    expression: &Expr,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let value = match expression {
        Expr::IsNull { value, negated } => {
            Value::Boolean(matches!(context.evaluate(value)?.value, Value::Null) != *negated)
        }
        Expr::NormalizedPredicate {
            value,
            form,
            negated,
        } => {
            let value = context.evaluate(value)?;
            let Value::String(value) = &value.value else {
                return scalar(Value::Null, context);
            };
            let normalized = crate::query::functions::normalized_predicate_controlled(
                value,
                *form,
                context.control,
            )?;
            Value::Boolean(normalized != *negated)
        }
        Expr::TypePredicate {
            value,
            type_spec,
            negated,
        } => {
            let value = context.evaluate(value)?;
            Value::Boolean(
                value_matches_type(&value.value, type_spec, context.control)? != *negated,
            )
        }
        Expr::LabelPredicate {
            value,
            name_expression,
            negated,
        } => return evaluate_label_predicate(value, name_expression, *negated, context),
        _ => {
            return Err(QueryError::internal(
                "non-predicate expression reached predicate evaluation",
            ));
        }
    };
    scalar(value, context)
}

fn evaluate_map_projection(
    base: &str,
    include_all: bool,
    entries: &[(String, Expr)],
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let base = context.evaluate(&Expr::Variable(base.to_owned()))?;
    if matches!(base.value, Value::Null) {
        return Ok(base);
    }
    let mut reservation = base.reservation;
    let mut output = if include_all {
        match base.value {
            Value::Map(values) => values,
            Value::Node(node) => node.properties,
            Value::Relationship(relationship) => relationship.properties,
            _ => {
                return Err(QueryError::new(
                    QueryErrorKind::Type,
                    "map projection .* requires a Map, Node, or Relationship",
                ));
            }
        }
    } else {
        BTreeMap::new()
    };
    for (key, expression) in entries {
        reservation.grow(checked_add(key.len() as u64, map_entry_bytes::<Value>()?)?)?;
        let value = context.evaluate(expression)?;
        reservation.merge(value.reservation)?;
        output.insert(clone_string_controlled(key, context.control)?, value.value);
    }
    Ok(EvaluatedValue {
        value: Value::Map(output),
        reservation,
    })
}

fn evaluate_subscript_tracked(
    base: EvaluatedValue,
    start: Option<EvaluatedValue>,
    end: Option<EvaluatedValue>,
    slice: bool,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    if matches!(base.value, Value::Null) {
        return scalar(Value::Null, context);
    }
    if slice {
        return evaluate_slice_tracked(base, start, end, context);
    }
    let Some(index) = start else {
        return scalar(Value::Null, context);
    };
    let value = match (&base.value, index.value) {
        (Value::List(values), Value::Integer(index)) => {
            normalize_index(index, values.len()).and_then(|index| values.get(index))
        }
        (Value::Map(values), Value::String(key)) => values.get(&key),
        (Value::Node(node), Value::String(key)) => node.properties.get(&key),
        (Value::Relationship(value), Value::String(key)) => value.properties.get(&key),
        (_, Value::Null) => None,
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "subscript requires List with Integer or Map/Node/Relationship with String",
            ));
        }
    };
    clone_filtered_value(value.unwrap_or(&Value::Null), context)
}

fn evaluate_slice_tracked(
    base: EvaluatedValue,
    start: Option<EvaluatedValue>,
    end: Option<EvaluatedValue>,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    if start
        .as_ref()
        .is_some_and(|value| matches!(value.value, Value::Null))
        || end
            .as_ref()
            .is_some_and(|value| matches!(value.value, Value::Null))
    {
        return scalar(Value::Null, context);
    }
    let start = optional_index(start.as_ref().map(|value| &value.value))?;
    let end = optional_index(end.as_ref().map(|value| &value.value))?;
    let Value::List(values) = &base.value else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "slice requires a List",
        ));
    };
    let (start, end) = slice_bounds(start, end, values.len());
    let mut reservation = context.control()?.reserve(checked_add(
        allocation_bytes::<Value>(end - start)?,
        std::mem::size_of::<Value>() as u64,
    )?)?;
    let mut output = Vec::with_capacity(end - start);
    for value in &values[start..end] {
        reservation.grow(value_bytes_with_control(value, context.control)?)?;
        output.push(super::clone_value_unreserved_controlled(
            value,
            context.control,
        )?);
    }
    Ok(EvaluatedValue {
        value: Value::List(output),
        reservation,
    })
}

fn optional_index(value: Option<&Value>) -> QueryResult<Option<i64>> {
    match value {
        None => Ok(None),
        Some(Value::Integer(value)) => Ok(Some(*value)),
        Some(_) => Err(QueryError::new(
            QueryErrorKind::Type,
            "slice bounds must be Integer values",
        )),
    }
}

fn normalize_index(index: i64, length: usize) -> Option<usize> {
    let length = i64::try_from(length).ok()?;
    let index = if index < 0 {
        length.checked_add(index)?
    } else {
        index
    };
    usize::try_from(index)
        .ok()
        .filter(|index| *index < length as usize)
}

fn slice_bounds(start: Option<i64>, end: Option<i64>, length: usize) -> (usize, usize) {
    let length_i64 = i64::try_from(length).unwrap_or(i64::MAX);
    let normalize = |value: i64| {
        let value = if value < 0 {
            length_i64.saturating_add(value)
        } else {
            value
        };
        value.clamp(0, length_i64) as usize
    };
    let start = start.map_or(0, normalize);
    let end = end.map_or(length, normalize);
    (start, end.max(start))
}

fn value_matches_type(
    value: &Value,
    spec: &TypeSpec,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    for term in &spec.terms {
        if let Some(control) = control {
            control.work(1)?;
        }
        if value_matches_term(value, term, control)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn value_matches_term(
    value: &Value,
    term: &TypeTermSpec,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    if matches!(value, Value::Null) {
        return Ok(term.nullable);
    }
    Ok(match &term.kind {
        TypeKind::List(inner) => match value {
            Value::List(values) => {
                for value in values {
                    if let Some(control) = control {
                        control.work(1)?;
                    }
                    if !value_matches_type(value, inner, control)? {
                        return Ok(false);
                    }
                }
                true
            }
            _ => false,
        },
        TypeKind::Vector {
            coordinate_type,
            dimension,
        } => match value {
            Value::Vector(value) => {
                coordinate_type.is_none_or(|kind| kind == value.coordinate_type())
                    && dimension.is_none_or(|dimension| dimension == value.dimension())
            }
            _ => false,
        },
        TypeKind::Named(name) => match name.as_str() {
            "ANY" | "ANY VALUE" => true,
            "NOTHING" | "NULL" => false,
            "BOOLEAN" | "BOOL" => matches!(value, Value::Boolean(_)),
            "INTEGER" | "INT" | "SIGNED INTEGER" => matches!(value, Value::Integer(_)),
            "FLOAT" => matches!(value, Value::Float(_)),
            "STRING" | "VARCHAR" => matches!(value, Value::String(_)),
            "MAP" => matches!(value, Value::Map(_)),
            "NODE" | "ANY NODE" | "VERTEX" | "ANY VERTEX" => {
                matches!(value, Value::Node(_))
            }
            "RELATIONSHIP" | "ANY RELATIONSHIP" | "EDGE" | "ANY EDGE" => {
                matches!(value, Value::Relationship(_))
            }
            "PATH" => matches!(value, Value::Path(_)),
            "DATE" => matches!(value, Value::Date(_)),
            "LOCAL TIME" | "TIME WITHOUT TIME ZONE" | "TIME WITHOUT TIMEZONE" => {
                matches!(value, Value::LocalTime(_))
            }
            "ZONED TIME" | "TIME WITH TIME ZONE" | "TIME WITH TIMEZONE" => {
                matches!(value, Value::Time(_))
            }
            "LOCAL DATETIME" | "TIMESTAMP WITHOUT TIME ZONE" | "TIMESTAMP WITHOUT TIMEZONE" => {
                matches!(value, Value::LocalDateTime(_))
            }
            "ZONED DATETIME" | "TIMESTAMP WITH TIME ZONE" | "TIMESTAMP WITH TIMEZONE" => {
                matches!(value, Value::ZonedDateTime(_))
            }
            "DURATION" => matches!(value, Value::Duration(_)),
            "POINT" => matches!(value, Value::Point(_)),
            "UUID" => matches!(value, Value::Uuid(_)),
            "PROPERTY VALUE" | "ANY PROPERTY VALUE" => property_value_matches(value, control)?,
            _ => false,
        },
    })
}

fn property_value_matches(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    let Value::List(values) = value else {
        return Ok(value.is_property_value());
    };
    let Some(first) = values.first() else {
        return Ok(true);
    };
    let family = std::mem::discriminant(first);
    if matches!(first, Value::List(_) | Value::Vector(_)) || !first.is_property_value() {
        return Ok(false);
    }
    for value in values {
        if let Some(control) = control {
            control.work(1)?;
        }
        if std::mem::discriminant(value) != family {
            return Ok(false);
        }
    }
    Ok(true)
}

fn evaluate_label_predicate(
    value: &Expr,
    name_expression: &AstNode,
    negated: bool,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let value = context.evaluate(value)?;
    let control = context.control()?;
    let Some((names, _names_memory)) = label_names(value.value, control)? else {
        return scalar(Value::Null, context);
    };
    let mut dynamic_memory = control.reserve(0)?;
    let matched = super::super::name_expression::matches_controlled(
        name_expression,
        &names,
        |dynamic| {
            dynamic_memory.resize(0)?;
            let value = context.evaluate(&compile_expression(dynamic)?)?;
            dynamic_memory.merge(value.reservation)?;
            Ok(value.value)
        },
        Some(control),
    )?;
    scalar(Value::Boolean(matched != negated), context)
}

fn label_names(
    value: Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(BTreeSet<String>, MemoryReservation)>> {
    let mut names = BTreeSet::new();
    let mut memory = control.reserve(0)?;
    match value {
        Value::Node(node) => {
            for label in node.labels {
                control.work(1)?;
                memory.grow(map_entry_bytes::<()>()?)?;
                names.insert(label);
            }
        }
        Value::Relationship(relationship) => {
            memory.grow(map_entry_bytes::<()>()?)?;
            names.insert(relationship.relationship_type);
        }
        Value::Null => return Ok(None),
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "label predicate requires Node, Relationship, or null",
            ));
        }
    }
    Ok(Some((names, memory)))
}

fn evaluate_interpolated(
    parts: &[InterpolatedPart],
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let control = context.control()?;
    let mut reservation = control.reserve(std::mem::size_of::<Value>() as u64)?;
    let mut output = String::new();
    for part in parts {
        let value;
        let text;
        let _text_memory;
        let input = match part {
            InterpolatedPart::Text(text) => text.as_str(),
            InterpolatedPart::Expression(expression) => {
                value = context.evaluate(expression)?;
                _text_memory = control.reserve(checked_add(
                    value_bytes_with_control(&value.value, Some(control))?,
                    256,
                )?)?;
                text = match &value.value {
                    Value::String(text) => clone_string_controlled(text, Some(control))?,
                    _ => value_to_string(&value.value)?,
                };
                text.as_str()
            }
        };
        reservation.grow(input.len() as u64)?;
        output.reserve_exact(input.len());
        crate::query::functions::append_text_controlled(&mut output, input, Some(control))?;
    }
    Ok(EvaluatedValue {
        value: Value::String(output),
        reservation,
    })
}

pub(crate) fn value_to_string(value: &Value) -> QueryResult<String> {
    match value {
        Value::Null => Ok("null".to_owned()),
        Value::Boolean(value) => Ok(value.to_string()),
        Value::Integer(value) => Ok(value.to_string()),
        Value::Float(value) => Ok(value.to_string()),
        Value::String(value) => Ok(value.clone()),
        Value::Date(value) => Ok(value.as_str().to_owned()),
        Value::LocalTime(value) => Ok(value.as_str().to_owned()),
        Value::Time(value) => Ok(value.as_str().to_owned()),
        Value::LocalDateTime(value) => Ok(value.as_str().to_owned()),
        Value::ZonedDateTime(value)
            if value.zone() == "Z"
                || value.zone().starts_with('+')
                || value.zone().starts_with('-') =>
        {
            Ok(value.value().to_owned())
        }
        Value::ZonedDateTime(value) => Ok(format!("{}[{}]", value.value(), value.zone())),
        Value::Duration(value) => Ok(value.as_str().to_owned()),
        Value::Uuid(value) => Ok(value.to_canonical()),
        Value::Point(value) => Ok(format!(
            "point({{srid: {}, coordinates: {:?}}})",
            value.srid(),
            value.coordinates()
        )),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "value is not supported by toString()/string interpolation",
        )),
    }
}

pub(crate) fn predicate(value: Value) -> QueryResult<bool> {
    match value {
        Value::Boolean(value) => Ok(value),
        Value::Null => Ok(false),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "WHERE expression must evaluate to Boolean or null",
        )),
    }
}

pub(crate) fn binding_from_value_controlled(
    snapshot: &Snapshot<'_>,
    value: Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<BindingValue> {
    control.check()?;
    match value {
        Value::Null => Ok(BindingValue::Null),
        Value::Node(node) => Ok(BindingValue::Node(element_numeric_id(
            &node.element_id,
            "n:",
            "Node",
        )?)),
        Value::Relationship(relationship) => Ok(BindingValue::Relationship(binding_relationship(
            snapshot,
            &relationship,
        )?)),
        Value::Path(path) => binding_from_path(snapshot, path, control),
        scalar => Ok(BindingValue::Scalar(scalar)),
    }
}

fn binding_from_path(
    snapshot: &Snapshot<'_>,
    path: PathValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<BindingValue> {
    // The caller's output lease continues to cover the compact binding. This
    // extra claim covers its arrays while the original graph value is alive.
    let output_memory = control.reserve(checked_add(
        allocation_bytes::<i64>(path.nodes.len())?,
        allocation_bytes::<crate::storage::RelationshipRecord>(path.relationships.len())?,
    )?)?;
    let mut nodes = Vec::with_capacity(path.nodes.len());
    for node in &path.nodes {
        control.work(1)?;
        nodes.push(element_numeric_id(&node.element_id, "n:", "Node")?);
    }
    let mut relationships = Vec::with_capacity(path.relationships.len());
    for relationship in &path.relationships {
        control.work(1)?;
        relationships.push(binding_relationship(snapshot, relationship)?);
    }
    drop(path);
    drop(output_memory);
    Ok(BindingValue::Path {
        nodes,
        relationships,
    })
}

fn binding_relationship(
    snapshot: &Snapshot<'_>,
    relationship: &crate::cypher::RelationshipValue,
) -> QueryResult<crate::storage::RelationshipRecord> {
    let id = element_numeric_id(&relationship.element_id, "r:", "Relationship")?;
    snapshot.relationship(id)?.ok_or_else(|| {
        QueryError::semantic(format!(
            "Relationship {} is not present in the active Snapshot",
            relationship.element_id
        ))
    })
}

fn element_numeric_id(element_id: &str, prefix: &str, kind: &str) -> QueryResult<i64> {
    let id = element_id
        .strip_prefix(prefix)
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| QueryError::semantic(format!("invalid {kind} elementId {element_id:?}")))?;
    Ok(id)
}

fn evaluate_property(
    base: &Expr,
    key: &str,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let control = context.control()?;
    if let Expr::Variable(name) = base {
        if let Some(value) = context.aliases.get(name) {
            return value_property_tracked(value, key, context);
        }
        return match context.row.values.get(name) {
            Some(BindingValue::Node(id)) => {
                let (value, reservation) = crate::query::graph::node_property_with_control(
                    context.snapshot,
                    *id,
                    key,
                    context.query_access,
                    control,
                )?;
                Ok(EvaluatedValue { value, reservation })
            }
            Some(BindingValue::Relationship(record)) => {
                let (value, reservation) = crate::query::graph::relationship_property_with_control(
                    context.snapshot,
                    record.id,
                    key,
                    context.query_access,
                    control,
                )?;
                Ok(EvaluatedValue { value, reservation })
            }
            Some(BindingValue::Scalar(value)) => value_property_tracked(value, key, context),
            Some(BindingValue::Null) | None => scalar(Value::Null, context),
            Some(BindingValue::Path { .. }) => Err(QueryError::new(
                QueryErrorKind::Type,
                "property access requires Node, Relationship, Map, or null",
            )),
        };
    }
    let base = context.evaluate(base)?;
    value_property_tracked(&base.value, key, context)
}

fn value_property_tracked(
    value: &Value,
    key: &str,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    if matches!(value, Value::Node(_) | Value::Relationship(_)) && context.query_access.denies(key)
    {
        return scalar(Value::Null, context);
    }
    let child = match value {
        Value::Node(node) => Some(node.properties.get(key)),
        Value::Relationship(relationship) => Some(relationship.properties.get(key)),
        Value::Map(values) => Some(values.get(key)),
        _ => None,
    };
    if let Some(child) = child {
        return clone_filtered_value(child.unwrap_or(&Value::Null), context);
    }
    let reservation = context.control()?.reserve(checked_add(
        value_bytes_with_control(value, context.control)?,
        256,
    )?)?;
    Ok(EvaluatedValue {
        value: value_property(value, key)?,
        reservation,
    })
}

pub(crate) fn value_property(value: &Value, key: &str) -> QueryResult<Value> {
    match value {
        Value::Node(node) => Ok(node.properties.get(key).cloned().unwrap_or(Value::Null)),
        Value::Relationship(relationship) => Ok(relationship
            .properties
            .get(key)
            .cloned()
            .unwrap_or(Value::Null)),
        Value::Map(values) => Ok(values.get(key).cloned().unwrap_or(Value::Null)),
        Value::Date(_)
        | Value::LocalTime(_)
        | Value::Time(_)
        | Value::LocalDateTime(_)
        | Value::ZonedDateTime(_)
        | Value::Duration(_)
        | Value::Point(_) => super::properties::value_property(value, key),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "property access requires a structural value or null",
        )),
    }
}

fn evaluate_function(
    name: &str,
    args: &[Expr],
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let control = context.control()?;
    let _name_memory = control.reserve(name.len() as u64)?;
    let lower = name.to_ascii_lowercase();
    if let Some(value) = evaluate_bound_metadata(&lower, args, context)? {
        return Ok(value);
    }
    let (values, _arguments_memory) = evaluate_arguments(args, context)?;
    if matches!(lower.as_str(), "startnode" | "endnode") {
        return evaluate_relationship_endpoint(&lower, &values, context);
    }
    if is_load_csv_metadata(&lower, args) {
        return evaluate_load_csv_metadata(&lower, context);
    }
    let mut reservation = control.reserve(std::mem::size_of::<Value>() as u64)?;
    let mut result_memory = None;
    let value = crate::query::functions::evaluate_with_memory(
        name,
        &values,
        Some(control),
        &mut result_memory,
    )
    .unwrap_or_else(|| {
        Err(QueryError::internal(format!(
            "registered function {name} has no runtime implementation"
        )))
    })?;
    if let Some(memory) = result_memory {
        reservation.merge(memory)?;
    }
    Ok(EvaluatedValue { value, reservation })
}

fn evaluate_bound_metadata(
    name: &str,
    args: &[Expr],
    context: EvalContext<'_, '_>,
) -> QueryResult<Option<EvaluatedValue>> {
    let [Expr::Variable(variable)] = args else {
        return Ok(None);
    };
    // Lexical aliases take precedence over row bindings and already own their
    // graph metadata. Ordinary evaluation handles their actual value/type.
    if context.aliases.contains_key(variable) {
        return Ok(None);
    }
    let value = match (name, context.row.values.get(variable)) {
        ("type", Some(BindingValue::Relationship(record))) => {
            crate::storage::with_relationship_type_name(
                context.snapshot.connection_for_query(),
                record.type_id,
                |name| {
                    let name = name.ok_or_else(|| {
                        QueryError::internal("Relationship Type dictionary entry is missing")
                    })?;
                    metadata_string(name, context)
                },
            )?
        }
        ("elementid", Some(BindingValue::Node(id))) => metadata_element_id("n", *id, context)?,
        ("elementid", Some(BindingValue::Relationship(record))) => {
            metadata_element_id("r", record.id, context)?
        }
        ("elementid", Some(BindingValue::Path { .. })) => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "elementId() requires Node or Relationship",
            ));
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn metadata_string(text: &str, context: EvalContext<'_, '_>) -> QueryResult<EvaluatedValue> {
    let control = context.control()?;
    let reservation = control.reserve(checked_add(
        std::mem::size_of::<Value>() as u64,
        text.len() as u64,
    )?)?;
    Ok(EvaluatedValue {
        value: Value::String(clone_string_controlled(text, Some(control))?),
        reservation,
    })
}

fn metadata_element_id(
    prefix: &str,
    id: i64,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let reservation = context
        .control()?
        .reserve(checked_add(std::mem::size_of::<Value>() as u64, 32)?)?;
    Ok(EvaluatedValue {
        value: Value::String(format!("{prefix}:{id}")),
        reservation,
    })
}

fn evaluate_arguments(
    args: &[Expr],
    context: EvalContext<'_, '_>,
) -> QueryResult<(Vec<Value>, MemoryReservation)> {
    let mut memory = context
        .control()?
        .reserve(allocation_bytes::<Value>(args.len())?)?;
    let mut values = Vec::with_capacity(args.len());
    for arg in args {
        let value = context.evaluate(arg)?;
        memory.merge(value.reservation)?;
        values.push(value.value);
    }
    Ok((values, memory))
}

fn evaluate_relationship_endpoint(
    name: &str,
    values: &[Value],
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let [value] = values else {
        return Err(QueryError::semantic(format!(
            "{name}() expects one argument"
        )));
    };
    let element_id = match value {
        Value::Relationship(value) if name == "startnode" => &value.start,
        Value::Relationship(value) => &value.end,
        Value::Null => return scalar(Value::Null, context),
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                format!("{name}() requires Relationship"),
            ));
        }
    };
    let id = element_numeric_id(element_id, "n:", "Node")?;
    let (value, reservation) = crate::query::graph::materialize_node_with_control(
        context.snapshot,
        id,
        context.query_access,
        context.control()?,
    )?;
    Ok(EvaluatedValue {
        value: Value::Node(value),
        reservation,
    })
}

fn is_load_csv_metadata(name: &str, args: &[Expr]) -> bool {
    args.is_empty() && matches!(name, "file" | "linenumber")
}

fn evaluate_load_csv_metadata(
    lower: &str,
    context: EvalContext<'_, '_>,
) -> QueryResult<EvaluatedValue> {
    let Some(csv) = &context.row.load_csv_context else {
        return scalar(Value::Null, context);
    };
    if lower == "file" {
        let Some(file) = &csv.file else {
            return scalar(Value::Null, context);
        };
        let reservation = context.control()?.reserve(checked_add(
            file.len() as u64,
            std::mem::size_of::<Value>() as u64,
        )?)?;
        Ok(EvaluatedValue {
            value: Value::String(clone_string_controlled(file, context.control)?),
            reservation,
        })
    } else {
        scalar(csv.line.map_or(Value::Null, Value::Integer), context)
    }
}

pub(crate) fn is_count(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::Function { name, .. } if name.eq_ignore_ascii_case("count")
    )
}

pub(crate) fn count_contributes_with_access_control(
    expression: &Expr,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
    query_access: &PropertyRules,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    let Expr::Function {
        name, args, star, ..
    } = expression
    else {
        return Err(QueryError::internal(
            "non-count expression reached count aggregation",
        ));
    };
    if !name.eq_ignore_ascii_case("count") {
        return Err(QueryError::internal(
            "non-count function reached count aggregation",
        ));
    }
    match args.as_slice() {
        [] if *star => Ok(true),
        [] => Ok(true),
        [argument] => {
            let value = evaluate_in_context(
                argument,
                EvalContext {
                    snapshot,
                    row,
                    params,
                    aliases: &BTreeMap::new(),
                    query_access,
                    control,
                },
            )?;
            Ok(!matches!(value.value, Value::Null))
        }
        _ => Err(QueryError::semantic(
            "count() expects zero or one executable argument in Phase 04",
        )),
    }
}
