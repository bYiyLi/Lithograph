use std::collections::{BTreeMap, BTreeSet};

use crate::cypher::{self, AstNode, PathValue, Value};
use crate::query::graph::{
    materialize_node, materialize_relationship, node_property, relationship_property,
};
use crate::query::{QueryError, QueryErrorKind, QueryResult};
use crate::storage::Snapshot;
use unicode_normalization::{is_nfc, is_nfd, is_nfkc, is_nfkd};

use super::operators::{evaluate_binary, evaluate_unary};
use super::{
    BindingRow, BindingValue, Expr, InterpolatedPart, ListPredicateKind, NormalizationForm,
    TypeKind, TypeSpec, TypeTermSpec, compile_expression,
};

#[derive(Clone, Copy)]
struct EvalContext<'data, 'connection> {
    snapshot: &'data Snapshot<'connection>,
    row: &'data BindingRow,
    params: &'data BTreeMap<String, Value>,
    aliases: &'data BTreeMap<String, Value>,
}

impl<'data, 'connection> EvalContext<'data, 'connection> {
    fn evaluate(self, expression: &Expr) -> QueryResult<Value> {
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
        }
    }
}

pub(crate) fn evaluate(
    expression: &Expr,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
) -> QueryResult<Value> {
    evaluate_with_aliases(expression, snapshot, row, params, &BTreeMap::new())
}

pub(crate) fn evaluate_with_aliases(
    expression: &Expr,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
    aliases: &BTreeMap<String, Value>,
) -> QueryResult<Value> {
    evaluate_in_context(
        expression,
        EvalContext {
            snapshot,
            row,
            params,
            aliases,
        },
    )
}

fn evaluate_in_context(expression: &Expr, context: EvalContext<'_, '_>) -> QueryResult<Value> {
    match expression {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::List(items) => items
            .iter()
            .map(|item| context.evaluate(item))
            .collect::<QueryResult<Vec<_>>>()
            .map(Value::List),
        Expr::Map(entries) => entries
            .iter()
            .map(|(key, value)| Ok((key.clone(), context.evaluate(value)?)))
            .collect::<QueryResult<BTreeMap<_, _>>>()
            .map(Value::Map),
        Expr::Variable(name) => match context.aliases.get(name) {
            Some(value) => Ok(value.clone()),
            None => binding_value(
                context.snapshot,
                context.row.values.get(name).unwrap_or(&BindingValue::Null),
            ),
        },
        Expr::Parameter(name) => Ok(context.params.get(name).cloned().unwrap_or(Value::Null)),
        Expr::Property(base, key) => evaluate_property(base, key, context),
        Expr::Function {
            name,
            args,
            distinct: _,
            star: _,
        } => evaluate_function(name, args, context),
        Expr::Case {
            operand,
            alternatives,
            fallback,
        } => evaluate_case(
            operand.as_deref(),
            alternatives,
            fallback.as_deref(),
            context,
        ),
        _ => evaluate_collection_expression(expression, context),
    }
}

fn evaluate_collection_expression(
    expression: &Expr,
    context: EvalContext<'_, '_>,
) -> QueryResult<Value> {
    match expression {
        Expr::ListComprehension {
            variable,
            collection,
            predicate,
            projection,
        } => evaluate_list_comprehension(
            variable,
            collection,
            predicate.as_deref(),
            projection.as_deref(),
            context,
        ),
        Expr::ListPredicate {
            kind,
            variable,
            collection,
            predicate,
        } => evaluate_list_predicate(*kind, variable, collection, predicate.as_deref(), context),
        Expr::Reduce {
            accumulator,
            initial,
            variable,
            collection,
            reduction,
        } => evaluate_reduction(
            accumulator,
            initial,
            variable,
            collection,
            reduction,
            None,
            context,
        ),
        Expr::AllReduce {
            accumulator,
            initial,
            variable,
            collection,
            reduction,
            predicate,
        } => evaluate_reduction(
            accumulator,
            initial,
            variable,
            collection,
            reduction,
            Some(predicate),
            context,
        ),
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
) -> QueryResult<Value> {
    match expression {
        Expr::Subscript {
            base,
            start,
            end,
            slice,
        } => evaluate_subscript(
            context.evaluate(base)?,
            start
                .as_deref()
                .map(|value| context.evaluate(value))
                .transpose()?,
            end.as_deref()
                .map(|value| context.evaluate(value))
                .transpose()?,
            *slice,
        ),
        Expr::IsNull { .. }
        | Expr::NormalizedPredicate { .. }
        | Expr::TypePredicate { .. }
        | Expr::LabelPredicate { .. } => evaluate_predicate_expression(expression, context),
        Expr::PatternPredicate(_) | Expr::PatternComprehension(_) => Err(QueryError::internal(
            "graph pattern expression reached the scalar evaluator",
        )),
        Expr::Interpolated(parts) => evaluate_interpolated(parts, context),
        Expr::Subquery { .. } => Err(QueryError::internal(
            "subquery expression reached the scalar evaluator",
        )),
        Expr::Unary(op, value) => evaluate_unary(*op, context.evaluate(value)?),
        Expr::Binary(op, left, right) => {
            let left = context.evaluate(left)?;
            let right = context.evaluate(right)?;
            evaluate_binary(*op, left, right)
        }
        Expr::Literal(_)
        | Expr::List(_)
        | Expr::Map(_)
        | Expr::Variable(_)
        | Expr::Parameter(_)
        | Expr::Property(_, _)
        | Expr::Function { .. }
        | Expr::Case { .. }
        | Expr::ListComprehension { .. }
        | Expr::ListPredicate { .. }
        | Expr::Reduce { .. }
        | Expr::AllReduce { .. }
        | Expr::MapProjection { .. } => Err(QueryError::internal(
            "expression dispatch reached an inconsistent evaluator branch",
        )),
    }
}

fn evaluate_predicate_expression(
    expression: &Expr,
    context: EvalContext<'_, '_>,
) -> QueryResult<Value> {
    match expression {
        Expr::IsNull { value, negated } => {
            let is_null = matches!(context.evaluate(value)?, Value::Null);
            Ok(Value::Boolean(if *negated { !is_null } else { is_null }))
        }
        Expr::NormalizedPredicate {
            value,
            form,
            negated,
        } => {
            let Value::String(value) = context.evaluate(value)? else {
                return Ok(Value::Null);
            };
            let normalized = match form {
                NormalizationForm::Nfc => is_nfc(&value),
                NormalizationForm::Nfd => is_nfd(&value),
                NormalizationForm::Nfkc => is_nfkc(&value),
                NormalizationForm::Nfkd => is_nfkd(&value),
            };
            Ok(Value::Boolean(if *negated {
                !normalized
            } else {
                normalized
            }))
        }
        Expr::TypePredicate {
            value,
            type_spec,
            negated,
        } => {
            let value = context.evaluate(value)?;
            let matches = value_matches_type(&value, type_spec);
            Ok(Value::Boolean(if *negated { !matches } else { matches }))
        }
        Expr::LabelPredicate {
            value,
            name_expression,
            negated,
        } => evaluate_label_predicate(value, name_expression, *negated, context),
        _ => Err(QueryError::internal(
            "non-predicate expression reached predicate evaluation",
        )),
    }
}

fn evaluate_case(
    operand: Option<&Expr>,
    alternatives: &[(Expr, Expr)],
    fallback: Option<&Expr>,
    context: EvalContext<'_, '_>,
) -> QueryResult<Value> {
    let operand = operand.map(|value| context.evaluate(value)).transpose()?;
    for (condition, result) in alternatives {
        let condition = context.evaluate(condition)?;
        let take = match &operand {
            Some(operand) => cypher::cypher_equals(operand, &condition)?.unwrap_or(false),
            None => match condition {
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
    fallback.map_or(Ok(Value::Null), |fallback| context.evaluate(fallback))
}

fn evaluate_list_comprehension(
    variable: &str,
    collection: &Expr,
    predicate_expression: Option<&Expr>,
    projection: Option<&Expr>,
    context: EvalContext<'_, '_>,
) -> QueryResult<Value> {
    let Some(values) = evaluate_list_input(collection, context, "list comprehension")? else {
        return Ok(Value::Null);
    };
    let mut output = Vec::new();
    for value in values {
        let mut local = context.aliases.clone();
        local.insert(variable.to_owned(), value.clone());
        let local_context = context.with_aliases(&local);
        let keep = predicate_expression.map_or(Ok(true), |predicate_expression| {
            let value = local_context.evaluate(predicate_expression)?;
            match value {
                Value::Boolean(value) => Ok(value),
                Value::Null => Ok(false),
                _ => Err(QueryError::new(
                    QueryErrorKind::Type,
                    "list comprehension WHERE must be Boolean or null",
                )),
            }
        })?;
        if keep {
            output.push(
                projection.map_or(Ok(value), |projection| local_context.evaluate(projection))?,
            );
        }
    }
    Ok(Value::List(output))
}

fn evaluate_list_predicate(
    kind: ListPredicateKind,
    variable: &str,
    collection: &Expr,
    predicate_expression: Option<&Expr>,
    context: EvalContext<'_, '_>,
) -> QueryResult<Value> {
    let Some(values) = evaluate_list_input(collection, context, "list predicate")? else {
        return Ok(Value::Null);
    };
    let counts = evaluate_list_predicate_values(variable, values, predicate_expression, context)?;
    Ok(list_predicate_result(kind, counts))
}

#[derive(Default)]
struct PredicateCounts {
    true_count: usize,
    false_count: usize,
    has_null: bool,
}

fn evaluate_list_predicate_values(
    variable: &str,
    values: Vec<Value>,
    predicate_expression: Option<&Expr>,
    context: EvalContext<'_, '_>,
) -> QueryResult<PredicateCounts> {
    let mut counts = PredicateCounts::default();
    for value in values {
        let mut local = context.aliases.clone();
        local.insert(variable.to_owned(), value.clone());
        let tested = match predicate_expression {
            Some(predicate_expression) => context
                .with_aliases(&local)
                .evaluate(predicate_expression)?,
            None => value,
        };
        match tested {
            Value::Boolean(true) => counts.true_count += 1,
            Value::Boolean(false) => counts.false_count += 1,
            Value::Null => counts.has_null = true,
            _ => {
                return Err(QueryError::new(
                    QueryErrorKind::Type,
                    "list predicate WHERE must be Boolean or null",
                ));
            }
        }
    }
    Ok(counts)
}

fn list_predicate_result(kind: ListPredicateKind, counts: PredicateCounts) -> Value {
    let result = match kind {
        ListPredicateKind::All if counts.false_count > 0 => Some(false),
        ListPredicateKind::All if counts.has_null => None,
        ListPredicateKind::All => Some(true),
        ListPredicateKind::Any if counts.true_count > 0 => Some(true),
        ListPredicateKind::Any if counts.has_null => None,
        ListPredicateKind::Any => Some(false),
        ListPredicateKind::None if counts.true_count > 0 => Some(false),
        ListPredicateKind::None if counts.has_null => None,
        ListPredicateKind::None => Some(true),
        ListPredicateKind::Single if counts.true_count > 1 => Some(false),
        ListPredicateKind::Single if counts.has_null => None,
        ListPredicateKind::Single => Some(counts.true_count == 1),
    };
    result.map(Value::Boolean).unwrap_or(Value::Null)
}

fn evaluate_reduction(
    accumulator_name: &str,
    initial: &Expr,
    variable: &str,
    collection: &Expr,
    reduction: &Expr,
    predicate_expression: Option<&Expr>,
    context: EvalContext<'_, '_>,
) -> QueryResult<Value> {
    let mut accumulator = context.evaluate(initial)?;
    let Some(values) = evaluate_list_input(collection, context, "reduction")? else {
        return Ok(Value::Null);
    };
    let mut has_null_predicate = false;
    for value in values {
        let mut local = context.aliases.clone();
        local.insert(accumulator_name.to_owned(), accumulator);
        local.insert(variable.to_owned(), value);
        accumulator = context.with_aliases(&local).evaluate(reduction)?;
        if let Some(predicate_expression) = predicate_expression {
            local.insert(accumulator_name.to_owned(), accumulator.clone());
            match context
                .with_aliases(&local)
                .evaluate(predicate_expression)?
            {
                Value::Boolean(true) => {}
                Value::Boolean(false) => return Ok(Value::Boolean(false)),
                Value::Null => has_null_predicate = true,
                _ => {
                    return Err(QueryError::new(
                        QueryErrorKind::Type,
                        "allReduce() predicate must be Boolean or null",
                    ));
                }
            }
        }
    }
    if predicate_expression.is_some() {
        Ok(if has_null_predicate {
            Value::Null
        } else {
            Value::Boolean(true)
        })
    } else {
        Ok(accumulator)
    }
}

fn evaluate_list_input(
    expression: &Expr,
    context: EvalContext<'_, '_>,
    operation: &str,
) -> QueryResult<Option<Vec<Value>>> {
    match context.evaluate(expression)? {
        Value::List(values) => Ok(Some(values)),
        Value::Null => Ok(None),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("{operation} IN input must be a List or null"),
        )),
    }
}

fn evaluate_map_projection(
    base: &str,
    include_all: bool,
    entries: &[(String, Expr)],
    context: EvalContext<'_, '_>,
) -> QueryResult<Value> {
    let base_value = context.evaluate(&Expr::Variable(base.to_owned()))?;
    if matches!(base_value, Value::Null) {
        return Ok(Value::Null);
    }
    let mut output = if include_all {
        match base_value {
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
        output.insert(key.clone(), context.evaluate(expression)?);
    }
    Ok(Value::Map(output))
}

fn evaluate_subscript(
    base: Value,
    start: Option<Value>,
    end: Option<Value>,
    slice: bool,
) -> QueryResult<Value> {
    if matches!(base, Value::Null) {
        return Ok(Value::Null);
    }
    if slice {
        return evaluate_slice(base, start, end);
    }
    let Some(index) = start else {
        return Ok(Value::Null);
    };
    match (base, index) {
        (Value::List(values), Value::Integer(index)) => Ok(normalize_index(index, values.len())
            .and_then(|index| values.get(index).cloned())
            .unwrap_or(Value::Null)),
        (Value::String(value), Value::Integer(index)) => {
            let characters = value.chars().collect::<Vec<_>>();
            Ok(normalize_index(index, characters.len())
                .and_then(|index| characters.get(index).copied())
                .map(|value| Value::String(value.to_string()))
                .unwrap_or(Value::Null))
        }
        (Value::Map(values), Value::String(key)) => {
            Ok(values.get(&key).cloned().unwrap_or(Value::Null))
        }
        (Value::Node(node), Value::String(key)) => {
            Ok(node.properties.get(&key).cloned().unwrap_or(Value::Null))
        }
        (Value::Relationship(relationship), Value::String(key)) => Ok(relationship
            .properties
            .get(&key)
            .cloned()
            .unwrap_or(Value::Null)),
        (_, Value::Null) => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "subscript requires List/String with Integer or Map/Node/Relationship with String",
        )),
    }
}

fn evaluate_slice(base: Value, start: Option<Value>, end: Option<Value>) -> QueryResult<Value> {
    if matches!(start, Some(Value::Null)) || matches!(end, Some(Value::Null)) {
        return Ok(Value::Null);
    }
    let start = optional_index(start)?;
    let end = optional_index(end)?;
    match base {
        Value::List(values) => {
            let (start, end) = slice_bounds(start, end, values.len());
            Ok(Value::List(values[start..end].to_vec()))
        }
        Value::String(value) => {
            let values = value.chars().collect::<Vec<_>>();
            let (start, end) = slice_bounds(start, end, values.len());
            Ok(Value::String(values[start..end].iter().collect()))
        }
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "slice requires a List or String",
        )),
    }
}

fn optional_index(value: Option<Value>) -> QueryResult<Option<i64>> {
    match value {
        None => Ok(None),
        Some(Value::Integer(value)) => Ok(Some(value)),
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

fn value_matches_type(value: &Value, spec: &TypeSpec) -> bool {
    spec.terms
        .iter()
        .any(|term| value_matches_term(value, term))
}

fn value_matches_term(value: &Value, term: &TypeTermSpec) -> bool {
    if matches!(value, Value::Null) {
        return term.nullable;
    }
    match &term.kind {
        TypeKind::List(inner) => match value {
            Value::List(values) => values.iter().all(|value| value_matches_type(value, inner)),
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
            "PROPERTY VALUE" | "ANY PROPERTY VALUE" => value.is_property_value(),
            _ => false,
        },
    }
}

fn evaluate_label_predicate(
    value: &Expr,
    name_expression: &AstNode,
    negated: bool,
    context: EvalContext<'_, '_>,
) -> QueryResult<Value> {
    let value = context.evaluate(value)?;
    let names = match value {
        Value::Node(node) => node.labels.into_iter().collect(),
        Value::Relationship(relationship) => BTreeSet::from([relationship.relationship_type]),
        Value::Null => return Ok(Value::Null),
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "label predicate requires Node, Relationship, or null",
            ));
        }
    };
    let matched = super::super::name_expression::matches(name_expression, &names, |dynamic| {
        context.evaluate(&compile_expression(dynamic)?)
    })?;
    Ok(Value::Boolean(if negated { !matched } else { matched }))
}

fn evaluate_interpolated(
    parts: &[InterpolatedPart],
    context: EvalContext<'_, '_>,
) -> QueryResult<Value> {
    let mut output = String::new();
    for part in parts {
        match part {
            InterpolatedPart::Text(text) => output.push_str(text),
            InterpolatedPart::Expression(expression) => {
                let value = context.evaluate(expression)?;
                output.push_str(&value_to_string(&value)?);
            }
        }
    }
    Ok(Value::String(output))
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

pub(crate) fn binding_value(snapshot: &Snapshot<'_>, value: &BindingValue) -> QueryResult<Value> {
    match value {
        BindingValue::Node(id) => Ok(Value::Node(materialize_node(snapshot, *id)?)),
        BindingValue::Relationship(record) => Ok(Value::Relationship(materialize_relationship(
            snapshot, *record,
        )?)),
        BindingValue::Path {
            nodes,
            relationships,
        } => Ok(Value::Path(PathValue {
            nodes: nodes
                .iter()
                .map(|id| materialize_node(snapshot, *id))
                .collect::<QueryResult<Vec<_>>>()?,
            relationships: relationships
                .iter()
                .map(|record| materialize_relationship(snapshot, *record))
                .collect::<QueryResult<Vec<_>>>()?,
        })),
        BindingValue::Scalar(value) => Ok(value.clone()),
        BindingValue::Null => Ok(Value::Null),
    }
}

pub(crate) fn binding_from_value(
    snapshot: &Snapshot<'_>,
    value: Value,
) -> QueryResult<BindingValue> {
    match value {
        Value::Null => Ok(BindingValue::Null),
        Value::Node(node) => Ok(BindingValue::Node(element_numeric_id(
            &node.element_id,
            "n:",
            "Node",
        )?)),
        Value::Relationship(relationship) => {
            let id = element_numeric_id(&relationship.element_id, "r:", "Relationship")?;
            let record = snapshot.relationship(id)?.ok_or_else(|| {
                QueryError::semantic(format!(
                    "Relationship {} is not present in the active Snapshot",
                    relationship.element_id
                ))
            })?;
            Ok(BindingValue::Relationship(record))
        }
        Value::Path(path) => {
            let nodes = path
                .nodes
                .iter()
                .map(|node| element_numeric_id(&node.element_id, "n:", "Node"))
                .collect::<QueryResult<Vec<_>>>()?;
            let relationships = path
                .relationships
                .iter()
                .map(|relationship| {
                    let id = element_numeric_id(&relationship.element_id, "r:", "Relationship")?;
                    snapshot.relationship(id)?.ok_or_else(|| {
                        QueryError::semantic(format!(
                            "Relationship {} is not present in the active Snapshot",
                            relationship.element_id
                        ))
                    })
                })
                .collect::<QueryResult<Vec<_>>>()?;
            Ok(BindingValue::Path {
                nodes,
                relationships,
            })
        }
        scalar => Ok(BindingValue::Scalar(scalar)),
    }
}

fn element_numeric_id(element_id: &str, prefix: &str, kind: &str) -> QueryResult<i64> {
    let id = element_id
        .strip_prefix(prefix)
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| QueryError::semantic(format!("invalid {kind} elementId {element_id:?}")))?;
    Ok(id)
}

fn evaluate_property(base: &Expr, key: &str, context: EvalContext<'_, '_>) -> QueryResult<Value> {
    if let Expr::Variable(name) = base {
        if let Some(value) = context.aliases.get(name) {
            return value_property(value, key);
        }
        return match context.row.values.get(name) {
            Some(BindingValue::Node(id)) => node_property(context.snapshot, *id, key),
            Some(BindingValue::Relationship(record)) => {
                relationship_property(context.snapshot, record.id, key)
            }
            Some(BindingValue::Path { .. }) => Err(QueryError::new(
                QueryErrorKind::Type,
                "property access requires Node, Relationship, Map, or null",
            )),
            Some(BindingValue::Scalar(value)) => value_property(value, key),
            Some(BindingValue::Null) | None => Ok(Value::Null),
        };
    }
    let value = context.evaluate(base)?;
    value_property(&value, key)
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
) -> QueryResult<Value> {
    let lower = name.to_ascii_lowercase();
    if matches!(lower.as_str(), "file" | "linenumber") && args.is_empty() {
        let binding = if lower == "file" {
            "__lithograph_load_csv_file"
        } else {
            "__lithograph_load_csv_line"
        };
        return match context.row.values.get(binding) {
            Some(BindingValue::Scalar(value)) => Ok(value.clone()),
            Some(BindingValue::Null) | None => Ok(Value::Null),
            Some(_) => Err(QueryError::internal(format!(
                "LOAD CSV context binding for {name}() has invalid type"
            ))),
        };
    }
    if lower == "elementid"
        && args.len() == 1
        && let Expr::Variable(variable) = &args[0]
    {
        if let Some(value) = context.aliases.get(variable) {
            return super::super::functions::element_id_value(value);
        }
        return match context.row.values.get(variable) {
            Some(BindingValue::Node(id)) => Ok(Value::String(format!("n:{id}"))),
            Some(BindingValue::Relationship(record)) => {
                Ok(Value::String(format!("r:{}", record.id)))
            }
            Some(BindingValue::Path { .. }) => Err(QueryError::new(
                QueryErrorKind::Type,
                "elementId() requires Node or Relationship",
            )),
            Some(BindingValue::Scalar(value)) => super::super::functions::element_id_value(value),
            Some(BindingValue::Null) | None => Ok(Value::Null),
        };
    }
    let values = args
        .iter()
        .map(|arg| context.evaluate(arg))
        .collect::<QueryResult<Vec<_>>>()?;
    if matches!(lower.as_str(), "startnode" | "endnode") {
        return relationship_endpoint(&lower, &values, context.snapshot);
    }
    evaluate_function_values(name, &values)
}

pub(crate) fn relationship_endpoint(
    name: &str,
    values: &[Value],
    snapshot: &Snapshot<'_>,
) -> QueryResult<Value> {
    let [value] = values else {
        return Err(QueryError::semantic(format!(
            "{name}() expects one argument"
        )));
    };
    let element_id = match value {
        Value::Relationship(relationship) if name == "startnode" => &relationship.start,
        Value::Relationship(relationship) => &relationship.end,
        Value::Null => return Ok(Value::Null),
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                format!("{name}() requires Relationship"),
            ));
        }
    };
    let id = element_id
        .strip_prefix("n:")
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| QueryError::internal("Relationship endpoint elementId is invalid"))?;
    materialize_node(snapshot, id).map(Value::Node)
}

pub(crate) fn evaluate_function_values(name: &str, values: &[Value]) -> QueryResult<Value> {
    if name.eq_ignore_ascii_case("count") {
        return Err(QueryError::internal(
            "count() reached scalar expression evaluation",
        ));
    }
    crate::query::functions::evaluate(name, values).unwrap_or_else(|| {
        Err(QueryError::internal(format!(
            "registered function {name} has no runtime implementation"
        )))
    })
}

pub(crate) fn is_count(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::Function { name, .. } if name.eq_ignore_ascii_case("count")
    )
}

pub(crate) fn count_contributes(
    expression: &Expr,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
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
        [argument] => Ok(!matches!(
            evaluate(argument, snapshot, row, params)?,
            Value::Null
        )),
        _ => Err(QueryError::semantic(
            "count() expects zero or one executable argument in Phase 04",
        )),
    }
}
