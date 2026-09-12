use super::*;

pub(super) fn pattern_expression_clause(
    pattern: &AstNode,
    assignment: Option<AstNode>,
) -> QueryResult<AstNode> {
    if pattern.kind != AstKind::Pattern {
        return Err(QueryError::internal(
            "pattern expression lowering received a non-pattern node",
        ));
    }
    let mut part_children = Vec::new();
    if let Some(assignment) = assignment {
        part_children.push(assignment);
    }
    part_children.push(pattern.clone());
    let part = AstNode {
        kind: AstKind::PatternPart,
        span: pattern.span,
        text: None,
        children: part_children,
    };
    Ok(AstNode {
        kind: AstKind::Clause(ClauseKind::Match),
        span: pattern.span,
        text: None,
        children: vec![AstNode {
            kind: AstKind::Pattern,
            span: pattern.span,
            text: None,
            children: vec![part],
        }],
    })
}

pub(super) fn yield_items(clause: &AstNode) -> Vec<(String, String)> {
    clause
        .descendants()
        .filter(|node| node.kind == AstKind::YieldItem)
        .filter_map(|item| {
            let source = item
                .descendants()
                .find(|node| node.kind == AstKind::YieldName)
                .and_then(|node| node.text.clone())?;
            let output = item
                .descendants()
                .find(|node| node.kind == AstKind::ProjectionAlias)
                .and_then(|node| node.text.clone())
                .unwrap_or_else(|| source.clone());
            Some((source, output))
        })
        .collect()
}

pub(super) fn single_column_rows(
    column: &str,
    values: impl IntoIterator<Item = String>,
) -> Vec<BTreeMap<String, Value>> {
    values
        .into_iter()
        .map(|value| BTreeMap::from([(column.to_owned(), Value::String(value))]))
        .collect()
}

pub(super) fn function_registry_rows() -> QueryResult<Vec<BindingRow>> {
    let mut definitions = crate::query::registry::functions().collect::<Vec<_>>();
    definitions.sort_by_key(|definition| definition.name.to_ascii_lowercase());
    definitions
        .into_iter()
        .map(|definition| {
            let arguments = definition.arguments().ok_or_else(|| {
                QueryError::internal(format!(
                    "registered function {} is missing frozen argument metadata",
                    definition.display_name()
                ))
            })?;
            let arguments = arguments
                .into_iter()
                .map(|argument| {
                    Value::Map(BTreeMap::from([
                        ("name".to_owned(), Value::String(argument.name)),
                        (
                            "type".to_owned(),
                            Value::String(argument.value_type.to_owned()),
                        ),
                        ("default".to_owned(), Value::Null),
                        ("isDeprecated".to_owned(), Value::Boolean(false)),
                        (
                            "description".to_owned(),
                            Value::String(argument.description.to_owned()),
                        ),
                    ]))
                })
                .collect();
            let signature = definition.signature().ok_or_else(|| {
                QueryError::internal(format!(
                    "registered function {} is missing frozen signature metadata",
                    definition.display_name()
                ))
            })?;
            let return_description = definition.return_description().ok_or_else(|| {
                QueryError::internal(format!(
                    "registered function {} is missing frozen return metadata",
                    definition.display_name()
                ))
            })?;
            Ok(registry_binding([
                ("name", Value::String(definition.display_name().to_owned())),
                ("category", Value::String(definition.category.to_owned())),
                (
                    "description",
                    Value::String(definition.description.to_owned()),
                ),
                ("signature", Value::String(signature)),
                ("isBuiltIn", Value::Boolean(true)),
                ("argumentDescription", Value::List(arguments)),
                (
                    "returnDescription",
                    Value::String(return_description.to_owned()),
                ),
                ("aggregating", Value::Boolean(definition.aggregating)),
                ("rolesExecution", Value::Null),
                ("rolesBoostedExecution", Value::Null),
                ("isDeprecated", Value::Boolean(definition.is_deprecated())),
                (
                    "deprecatedBy",
                    definition
                        .deprecated_by()
                        .map_or(Value::Null, |name| Value::String(name.to_owned())),
                ),
            ]))
        })
        .collect()
}

pub(super) fn procedure_registry_rows() -> Vec<BindingRow> {
    crate::query::registry::procedures()
        .map(|definition| {
            let returned = definition
                .outputs
                .iter()
                .map(|name| {
                    Value::Map(BTreeMap::from([
                        ("name".to_owned(), Value::String((*name).to_owned())),
                        ("type".to_owned(), Value::String("STRING".to_owned())),
                        ("isDeprecated".to_owned(), Value::Boolean(false)),
                        (
                            "description".to_owned(),
                            Value::String("Procedure output field.".to_owned()),
                        ),
                    ]))
                })
                .collect();
            registry_binding([
                ("name", Value::String(definition.name.to_owned())),
                (
                    "description",
                    Value::String(definition.description.to_owned()),
                ),
                ("mode", Value::String(definition.mode.to_owned())),
                ("worksOnSystem", Value::Boolean(definition.works_on_system)),
                ("signature", Value::String(definition.signature.to_owned())),
                ("argumentDescription", Value::List(Vec::new())),
                ("returnDescription", Value::List(returned)),
                ("admin", Value::Boolean(definition.admin)),
                ("rolesExecution", Value::Null),
                ("rolesBoostedExecution", Value::Null),
                ("isDeprecated", Value::Boolean(false)),
                ("deprecatedBy", Value::Null),
                (
                    "option",
                    Value::Map(BTreeMap::from([(
                        "deprecated".to_owned(),
                        Value::Boolean(false),
                    )])),
                ),
            ])
        })
        .collect()
}

pub(super) fn registry_binding<const N: usize>(values: [(&str, Value); N]) -> BindingRow {
    let mut row = BindingRow::default();
    for (name, value) in values {
        row.insert(name.to_owned(), BindingValue::Scalar(value));
    }
    row
}

pub(super) fn project_named_columns(mut row: BindingRow, columns: &[String]) -> BindingRow {
    row.values.retain(|name, _| columns.contains(name));
    row.order.retain(|name| columns.contains(name));
    row
}

pub(super) fn cross_join_registry(input: RowSet, registry: RowSet) -> RowSet {
    let mut columns = input.columns.clone();
    for column in &registry.columns {
        if !columns.contains(column) {
            columns.push(column.clone());
        }
    }
    let mut rows = Vec::new();
    for outer in input.rows {
        for shown in &registry.rows {
            let mut combined = outer.clone();
            for column in &registry.columns {
                combined.insert(
                    column.clone(),
                    shown
                        .values
                        .get(column)
                        .cloned()
                        .unwrap_or(BindingValue::Null),
                );
            }
            rows.push(combined);
        }
    }
    RowSet { columns, rows }
}

#[derive(Debug)]
pub(super) struct ProjectionSpec {
    pub(super) column: String,
    pub(super) expression: expression::Expr,
    pub(super) aggregate: bool,
}

#[derive(Debug, Clone)]
pub(super) struct ProjectedRow {
    pub(super) output: BindingRow,
    pub(super) group: Vec<BindingRow>,
}

pub(super) fn expr_contains_aggregate(expression: &expression::Expr) -> bool {
    use expression::Expr;
    match expression {
        Expr::Function { .. }
        | Expr::List(_)
        | Expr::Map(_)
        | Expr::Case { .. }
        | Expr::ListComprehension { .. }
        | Expr::ListPredicate { .. }
        | Expr::Reduce { .. }
        | Expr::AllReduce { .. }
        | Expr::MapProjection { .. } => aggregate_in_composite(expression),
        _ => aggregate_in_operator(expression),
    }
}

fn aggregate_in_composite(expression: &expression::Expr) -> bool {
    use expression::Expr;
    match expression {
        Expr::Function { name, args, .. } => {
            crate::query::registry::is_aggregating(name) || args.iter().any(expr_contains_aggregate)
        }
        Expr::List(items) => items.iter().any(expr_contains_aggregate),
        Expr::Map(entries) => entries.values().any(expr_contains_aggregate),
        Expr::Case {
            operand,
            alternatives,
            fallback,
        } => aggregate_in_case(operand.as_deref(), alternatives, fallback.as_deref()),
        Expr::ListComprehension {
            collection,
            predicate,
            projection,
            ..
        } => aggregate_in_list_collection(collection, predicate.as_deref(), projection.as_deref()),
        Expr::ListPredicate {
            collection,
            predicate,
            ..
        } => aggregate_in_list_collection(collection, predicate.as_deref(), None),
        Expr::Reduce {
            initial,
            collection,
            reduction,
            ..
        } => aggregate_in_reduction(initial, collection, reduction, None),
        Expr::AllReduce {
            initial,
            collection,
            reduction,
            predicate,
            ..
        } => aggregate_in_reduction(initial, collection, reduction, Some(predicate)),
        Expr::MapProjection { entries, .. } => entries
            .iter()
            .any(|(_, value)| expr_contains_aggregate(value)),
        _ => unreachable!("composite aggregate dispatch is exhaustive"),
    }
}

fn aggregate_in_operator(expression: &expression::Expr) -> bool {
    use expression::Expr;
    match expression {
        Expr::Subscript {
            base, start, end, ..
        } => aggregate_in_list_collection(base, start.as_deref(), end.as_deref()),
        Expr::IsNull { value, .. }
        | Expr::NormalizedPredicate { value, .. }
        | Expr::TypePredicate { value, .. }
        | Expr::LabelPredicate { value, .. } => expr_contains_aggregate(value),
        Expr::Interpolated(parts) => parts.iter().any(|part| match part {
            expression::InterpolatedPart::Text(_) => false,
            expression::InterpolatedPart::Expression(value) => expr_contains_aggregate(value),
        }),
        Expr::Subquery { .. } | Expr::PatternPredicate(_) | Expr::PatternComprehension(_) => false,
        Expr::Property(base, _) | Expr::Unary(_, base) => expr_contains_aggregate(base),
        Expr::Binary(_, left, right) => {
            expr_contains_aggregate(left) || expr_contains_aggregate(right)
        }
        Expr::Literal(_) | Expr::Variable(_) | Expr::Parameter(_) => false,
        _ => unreachable!("operator aggregate dispatch is exhaustive"),
    }
}

fn aggregate_in_case(
    operand: Option<&expression::Expr>,
    alternatives: &[(expression::Expr, expression::Expr)],
    fallback: Option<&expression::Expr>,
) -> bool {
    operand.is_some_and(expr_contains_aggregate)
        || alternatives.iter().any(|(condition, result)| {
            expr_contains_aggregate(condition) || expr_contains_aggregate(result)
        })
        || fallback.is_some_and(expr_contains_aggregate)
}

fn aggregate_in_list_collection(
    collection: &expression::Expr,
    predicate: Option<&expression::Expr>,
    projection: Option<&expression::Expr>,
) -> bool {
    expr_contains_aggregate(collection)
        || predicate.is_some_and(expr_contains_aggregate)
        || projection.is_some_and(expr_contains_aggregate)
}

fn aggregate_in_reduction(
    initial: &expression::Expr,
    collection: &expression::Expr,
    reduction: &expression::Expr,
    predicate: Option<&expression::Expr>,
) -> bool {
    expr_contains_aggregate(initial)
        || expr_contains_aggregate(collection)
        || expr_contains_aggregate(reduction)
        || predicate.is_some_and(expr_contains_aggregate)
}

pub(super) fn aggregate_sum(values: &[Value]) -> QueryResult<Value> {
    if matches!(values.first(), Some(Value::Duration(_))) {
        return aggregate_duration(values, None).map(Value::Duration);
    }
    let mut integer = 0_i64;
    let mut floating = 0.0_f64;
    let mut has_float = false;
    for value in values {
        match value {
            Value::Integer(value) if !has_float => {
                integer = integer.checked_add(*value).ok_or_else(|| {
                    QueryError::new(QueryErrorKind::Type, "INTEGER64 sum overflow")
                })?;
            }
            Value::Integer(value) => floating += *value as f64,
            Value::Float(value) => {
                if !has_float {
                    floating = integer as f64;
                    has_float = true;
                }
                floating += value;
            }
            _ => {
                return Err(QueryError::new(
                    QueryErrorKind::Type,
                    "sum() requires numeric values",
                ));
            }
        }
    }
    Ok(if has_float {
        Value::Float(floating)
    } else {
        Value::Integer(integer)
    })
}

pub(super) fn aggregate_average(values: &[Value]) -> QueryResult<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    if matches!(values.first(), Some(Value::Duration(_))) {
        return aggregate_duration(values, Some(values.len())).map(Value::Duration);
    }
    let total = numeric_values(values)?.into_iter().sum::<f64>();
    Ok(Value::Float(total / values.len() as f64))
}

pub(super) fn aggregate_duration(
    values: &[Value],
    divisor: Option<usize>,
) -> QueryResult<crate::cypher::DurationValue> {
    let mut total = collect_duration_components(values)?;
    match divisor {
        Some(divisor) => average_duration_components(&mut total, divisor)?,
        None => normalize_duration_components(&mut total)?,
    }
    duration_from_aggregate(total)
}

#[derive(Default)]
struct DurationAggregate {
    months: i128,
    days: i128,
    seconds: i128,
    nanoseconds: i128,
}

fn collect_duration_components(values: &[Value]) -> QueryResult<DurationAggregate> {
    let mut total = DurationAggregate::default();
    for value in values {
        let Value::Duration(value) = value else {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "duration aggregation cannot mix numeric and Duration values",
            ));
        };
        let (value_months, value_days, value_seconds, value_nanoseconds) = value.components();
        total.months = total
            .months
            .checked_add(i128::from(value_months))
            .ok_or_else(duration_aggregate_overflow)?;
        total.days = total
            .days
            .checked_add(i128::from(value_days))
            .ok_or_else(duration_aggregate_overflow)?;
        total.seconds = total
            .seconds
            .checked_add(i128::from(value_seconds))
            .ok_or_else(duration_aggregate_overflow)?;
        total.nanoseconds = total
            .nanoseconds
            .checked_add(i128::from(value_nanoseconds))
            .ok_or_else(duration_aggregate_overflow)?;
    }
    Ok(total)
}

fn average_duration_components(total: &mut DurationAggregate, divisor: usize) -> QueryResult<()> {
    const NANOS_PER_SECOND: i128 = 1_000_000_000;
    const SECONDS_PER_DAY: i128 = 86_400;
    // Cypher uses the average Gregorian month (365.2425 / 12 days) when a
    // fractional month must overflow into the smaller duration groups.
    const SECONDS_PER_AVERAGE_MONTH: i128 = 2_629_746;

    let divisor = i128::try_from(divisor).map_err(|_| duration_aggregate_overflow())?;
    let month_remainder = total.months % divisor;
    total.months /= divisor;
    let day_remainder = total.days % divisor;
    total.days /= divisor;
    total.nanoseconds = total
        .nanoseconds
        .checked_add(
            total
                .seconds
                .checked_mul(NANOS_PER_SECOND)
                .ok_or_else(duration_aggregate_overflow)?,
        )
        .and_then(|value| {
            value.checked_add(day_remainder.checked_mul(SECONDS_PER_DAY * NANOS_PER_SECOND)?)
        })
        .and_then(|value| {
            value.checked_add(
                month_remainder.checked_mul(SECONDS_PER_AVERAGE_MONTH * NANOS_PER_SECOND)?,
            )
        })
        .ok_or_else(duration_aggregate_overflow)?
        / divisor;
    total.seconds = total.nanoseconds / NANOS_PER_SECOND;
    total.nanoseconds %= NANOS_PER_SECOND;
    Ok(())
}

fn normalize_duration_components(total: &mut DurationAggregate) -> QueryResult<()> {
    const NANOS_PER_SECOND: i128 = 1_000_000_000;
    total.seconds = total
        .seconds
        .checked_add(total.nanoseconds / NANOS_PER_SECOND)
        .ok_or_else(duration_aggregate_overflow)?;
    total.nanoseconds %= NANOS_PER_SECOND;
    Ok(())
}

fn duration_from_aggregate(total: DurationAggregate) -> QueryResult<crate::cypher::DurationValue> {
    Ok(crate::cypher::DurationValue::from_components(
        i64::try_from(total.months).map_err(|_| duration_aggregate_overflow())?,
        i64::try_from(total.days).map_err(|_| duration_aggregate_overflow())?,
        i64::try_from(total.seconds).map_err(|_| duration_aggregate_overflow())?,
        i64::try_from(total.nanoseconds).map_err(|_| duration_aggregate_overflow())?,
    ))
}

pub(super) fn duration_aggregate_overflow() -> QueryError {
    QueryError::new(QueryErrorKind::Type, "duration aggregation overflow")
}

pub(super) fn aggregate_extreme(values: &[Value], maximum: bool) -> QueryResult<Value> {
    let Some(first) = values.first() else {
        return Ok(Value::Null);
    };
    let mut result = first.clone();
    for value in values.iter().skip(1) {
        let ordering = crate::cypher::cypher_order_compare(value, &result)?;
        if (maximum && ordering == Ordering::Greater) || (!maximum && ordering == Ordering::Less) {
            result = value.clone();
        }
    }
    Ok(result)
}

pub(super) fn aggregate_percentile(
    values: &[Value],
    percentile: Value,
    continuous: bool,
) -> QueryResult<Value> {
    let percentile = match percentile {
        Value::Integer(value) => value as f64,
        Value::Float(value) => value,
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "percentile must be numeric",
            ));
        }
    };
    if !(0.0..=1.0).contains(&percentile) || percentile.is_nan() {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "percentile must be between 0.0 and 1.0",
        ));
    }
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let mut numbers = values
        .iter()
        .cloned()
        .map(|value| {
            let number = match value {
                Value::Integer(number) => number as f64,
                Value::Float(number) => number,
                _ => {
                    return Err(QueryError::new(
                        QueryErrorKind::Type,
                        "percentile aggregate requires numeric values",
                    ));
                }
            };
            Ok((number, value))
        })
        .collect::<QueryResult<Vec<_>>>()?;
    numbers.sort_by(|left, right| left.0.total_cmp(&right.0));
    if continuous {
        let position = percentile * numbers.len().saturating_sub(1) as f64;
        let lower = position.floor() as usize;
        let upper = position.ceil() as usize;
        let fraction = position - lower as f64;
        Ok(Value::Float(
            numbers[lower].0 + (numbers[upper].0 - numbers[lower].0) * fraction,
        ))
    } else {
        let index = (percentile * numbers.len() as f64).ceil() as usize;
        Ok(numbers[index.saturating_sub(1)].1.clone())
    }
}

pub(super) fn aggregate_deviation(values: &[Value], sample: bool) -> QueryResult<Value> {
    let numbers = numeric_values(values)?;
    if numbers.is_empty() {
        return Ok(Value::Null);
    }
    if sample && numbers.len() == 1 {
        return Ok(Value::Float(0.0));
    }
    let mean = numbers.iter().sum::<f64>() / numbers.len() as f64;
    let sum = numbers
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>();
    let divisor = if sample {
        numbers.len().saturating_sub(1)
    } else {
        numbers.len()
    };
    Ok(Value::Float((sum / divisor as f64).sqrt()))
}

pub(super) fn numeric_values(values: &[Value]) -> QueryResult<Vec<f64>> {
    values
        .iter()
        .map(|value| match value {
            Value::Integer(value) => Ok(*value as f64),
            Value::Float(value) => Ok(*value),
            _ => Err(QueryError::new(
                QueryErrorKind::Type,
                "aggregate requires numeric values",
            )),
        })
        .collect()
}

pub(super) fn clone_row_set(input: &RowSet) -> RowSet {
    RowSet {
        columns: input.columns.clone(),
        rows: input.rows.clone(),
    }
}

pub(super) fn union_rows(
    left: RowSet,
    right: RowSet,
    distinct: bool,
    snapshot: &Snapshot<'_>,
) -> QueryResult<RowSet> {
    let mut left = merge_union_rows(left, right)?;
    if distinct {
        left.rows = distinct_bindings(left.rows, &left.columns, snapshot)?;
    }
    Ok(left)
}

pub(crate) fn merge_union_rows(mut left: RowSet, right: RowSet) -> QueryResult<RowSet> {
    if left.columns != right.columns {
        return Err(QueryError::semantic(format!(
            "UNION column mismatch: left {:?}, right {:?}",
            left.columns, right.columns
        )));
    }
    left.rows.extend(right.rows);
    Ok(left)
}

pub(crate) fn distinct_bindings(
    rows: Vec<BindingRow>,
    columns: &[String],
    snapshot: &Snapshot<'_>,
) -> QueryResult<Vec<BindingRow>> {
    distinct_rows(rows, columns, snapshot, |row| row)
}

pub(super) fn distinct_projected(
    rows: Vec<ProjectedRow>,
    columns: &[String],
    snapshot: &Snapshot<'_>,
) -> QueryResult<Vec<ProjectedRow>> {
    distinct_rows(rows, columns, snapshot, |row| &row.output)
}

fn distinct_rows<T>(
    rows: Vec<T>,
    columns: &[String],
    snapshot: &Snapshot<'_>,
    binding: impl Fn(&T) -> &BindingRow,
) -> QueryResult<Vec<T>> {
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for row in rows {
        let values = columns
            .iter()
            .map(|column| {
                binding_value(
                    snapshot,
                    binding(&row)
                        .values
                        .get(column)
                        .unwrap_or(&BindingValue::Null),
                )
            })
            .collect::<QueryResult<Vec<_>>>()?;
        if seen.insert(distinct_row_key(&values)?) {
            output.push(row);
        }
    }
    Ok(output)
}

pub(super) fn compare_keys(
    left: &[Value],
    right: &[Value],
    directions: &[(usize, crate::cypher::OrderDirectionKind)],
    order_node: &AstNode,
) -> QueryResult<Ordering> {
    let expression_nodes = surface_expressions(order_node);
    for (index, (left, right)) in left.iter().zip(right).enumerate() {
        let mut ordering = crate::cypher::cypher_order_compare(left, right)?;
        let descending = expression_nodes.get(index).is_some_and(|expression| {
            let end = expression_nodes
                .get(index + 1)
                .map_or(order_node.span.end, |next| next.span.start);
            directions.iter().any(|(position, direction)| {
                *position >= expression.span.end
                    && *position < end
                    && *direction == crate::cypher::OrderDirectionKind::Descending
            })
        });
        if descending {
            ordering = ordering.reverse();
        }
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(Ordering::Equal)
}

pub(super) fn pagination(
    body: &AstNode,
    kind: AstKind,
    name: &str,
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
) -> QueryResult<Option<usize>> {
    let Some(node) = body.descendants().find(|node| node.kind == kind) else {
        return Ok(None);
    };
    let expression = surface_expressions(node)
        .into_iter()
        .next()
        .ok_or_else(|| QueryError::semantic(format!("{name} is missing its expression")))?;
    let value = expression::evaluate(
        &compile_expression(expression)?,
        snapshot,
        &BindingRow::default(),
        params,
    )?;
    let Value::Integer(value) = value else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            format!("{name} must evaluate to an Integer"),
        ));
    };
    usize::try_from(value)
        .map(Some)
        .map_err(|_| QueryError::new(QueryErrorKind::Type, format!("{name} must be non-negative")))
}
