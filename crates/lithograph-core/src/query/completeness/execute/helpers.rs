use super::*;

pub(super) fn null_extend_row(mut row: BindingRow, columns: &[String]) -> BindingRow {
    for column in columns {
        if !row.values.contains_key(column) {
            row.insert(column.clone(), BindingValue::Null);
        }
    }
    row
}

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

mod registry;
pub(super) use registry::{function_registry_rows, procedure_registry_rows};

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

pub(super) fn aggregate_sum(
    values: &[Value],
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    if matches!(values.first(), Some(Value::Duration(_))) {
        return aggregate_duration(values, None, control).map(Value::Duration);
    }
    let mut integer = 0_i64;
    let mut floating = 0.0_f64;
    let mut has_float = false;
    for value in values {
        control.work(1)?;
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

pub(super) fn aggregate_average(
    values: &[Value],
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    if matches!(values.first(), Some(Value::Duration(_))) {
        return aggregate_duration(values, Some(values.len()), control).map(Value::Duration);
    }
    let total = numeric_total(values, control)?;
    Ok(Value::Float(total / values.len() as f64))
}

fn numeric_value(value: &Value) -> QueryResult<f64> {
    match value {
        Value::Integer(value) => Ok(*value as f64),
        Value::Float(value) => Ok(*value),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "aggregate requires numeric values",
        )),
    }
}

fn numeric_total(values: &[Value], control: &ExecutionControl<'_>) -> QueryResult<f64> {
    let mut total = 0.0;
    for value in values {
        control.work(1)?;
        total += numeric_value(value)?;
    }
    Ok(total)
}

pub(super) fn aggregate_duration(
    values: &[Value],
    divisor: Option<usize>,
    control: &ExecutionControl<'_>,
) -> QueryResult<crate::cypher::DurationValue> {
    let mut total = collect_duration_components(values, control)?;
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

fn collect_duration_components(
    values: &[Value],
    control: &ExecutionControl<'_>,
) -> QueryResult<DurationAggregate> {
    let mut total = DurationAggregate::default();
    for value in values {
        control.work(1)?;
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

pub(super) fn aggregate_extreme(
    values: &[Value],
    maximum: bool,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    let Some(first) = values.first() else {
        return Ok(Value::Null);
    };
    let mut result = expression::clone_value_unreserved_controlled(first, Some(control))?;
    for value in values.iter().skip(1) {
        control.work(1)?;
        let ordering = expression::cypher_order_compare_controlled(value, &result, Some(control))?;
        if (maximum && ordering == Ordering::Greater) || (!maximum && ordering == Ordering::Less) {
            result = expression::clone_value_unreserved_controlled(value, Some(control))?;
        }
    }
    Ok(result)
}

pub(super) fn aggregate_percentile(
    values: &[Value],
    percentile: Value,
    continuous: bool,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    let percentile = percentile_number(percentile)?;
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let _memory = control.reserve(crate::query::resources::allocation_bytes::<(f64, usize)>(
        values.len(),
    )?)?;
    let mut numbers = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        control.work(1)?;
        let number = numeric_value(value).map_err(|_| {
            QueryError::new(
                QueryErrorKind::Type,
                "percentile aggregate requires numeric values",
            )
        })?;
        numbers.push((number, index));
    }
    sort_numeric_values(&mut numbers, control)?;
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
        expression::clone_value_unreserved_controlled(
            &values[numbers[index.saturating_sub(1)].1],
            Some(control),
        )
    }
}

fn percentile_number(percentile: Value) -> QueryResult<f64> {
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
    Ok(percentile)
}

fn sort_numeric_values(
    values: &mut [(f64, usize)],
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    for root in (0..values.len() / 2).rev() {
        sift_numeric_values(values, root, control)?;
    }
    for end in (1..values.len()).rev() {
        control.work(1)?;
        values.swap(0, end);
        sift_numeric_values(&mut values[..end], 0, control)?;
    }
    Ok(())
}

fn sift_numeric_values(
    values: &mut [(f64, usize)],
    mut root: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    loop {
        control.work(1)?;
        let left = root.saturating_mul(2).saturating_add(1);
        if left >= values.len() {
            return Ok(());
        }
        let right = left + 1;
        let child = if right < values.len()
            && compare_numeric(&values[left], &values[right]) == Ordering::Less
        {
            right
        } else {
            left
        };
        if compare_numeric(&values[root], &values[child]) != Ordering::Less {
            return Ok(());
        }
        values.swap(root, child);
        root = child;
    }
}

fn compare_numeric(left: &(f64, usize), right: &(f64, usize)) -> Ordering {
    left.0
        .total_cmp(&right.0)
        .then_with(|| left.1.cmp(&right.1))
}

pub(super) fn aggregate_deviation(
    values: &[Value],
    sample: bool,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let mean = numeric_total(values, control)? / values.len() as f64;
    if sample && values.len() == 1 {
        return Ok(Value::Float(0.0));
    }
    let mut sum = 0.0;
    for value in values {
        control.work(1)?;
        sum += (numeric_value(value)? - mean).powi(2);
    }
    let divisor = if sample {
        values.len().saturating_sub(1)
    } else {
        values.len()
    };
    Ok(Value::Float((sum / divisor as f64).sqrt()))
}

pub(super) fn clone_row_set(input: &RowSet, control: &ExecutionControl<'_>) -> QueryResult<RowSet> {
    input.try_clone(control)
}

pub(super) fn union_rows(
    left: RowSet,
    right: RowSet,
    distinct: bool,
    snapshot: &Snapshot<'_>,
    rules: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<RowSet> {
    let mut left = merge_union_rows(left, right)?;
    if distinct {
        left.rows = distinct_bindings(left.rows, &left.columns, snapshot, rules, control)?;
    }
    Ok(left)
}

pub(crate) fn merge_union_rows(mut left: RowSet, mut right: RowSet) -> QueryResult<RowSet> {
    if left.columns != right.columns {
        return Err(QueryError::semantic(format!(
            "UNION column mismatch: left {:?}, right {:?}",
            left.columns, right.columns
        )));
    }
    if let Some(memory) = right.memory.take() {
        if let Some(left_memory) = left.memory.as_mut() {
            left_memory.merge(memory)?;
        } else {
            left.memory = Some(memory);
        }
    }
    left.rows.extend(right.rows);
    Ok(left)
}

pub(crate) fn distinct_bindings(
    rows: Vec<BindingRow>,
    columns: &[String],
    snapshot: &Snapshot<'_>,
    rules: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<Vec<BindingRow>> {
    distinct_rows(rows, columns, snapshot, rules, control, |row| row)
}

pub(super) fn distinct_projected(
    rows: Vec<ProjectedRow>,
    columns: &[String],
    snapshot: &Snapshot<'_>,
    rules: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<Vec<ProjectedRow>> {
    distinct_rows(rows, columns, snapshot, rules, control, |row| &row.output)
}

fn distinct_rows<T>(
    rows: Vec<T>,
    columns: &[String],
    snapshot: &Snapshot<'_>,
    rules: &PropertyRules,
    control: &ExecutionControl<'_>,
    binding: impl Fn(&T) -> &BindingRow,
) -> QueryResult<Vec<T>> {
    let mut seen = BTreeSet::new();
    let mut seen_memory = control.reserve(0)?;
    let _output_memory =
        control.reserve(crate::query::resources::allocation_bytes::<T>(rows.len())?)?;
    let mut output = Vec::with_capacity(rows.len());
    for row in rows {
        control.work(1)?;
        let (key, key_memory) =
            binding_distinct_key(binding(&row), columns, snapshot, rules, control)?;
        if seen.insert(key) {
            seen_memory.merge(key_memory)?;
            output.push(row);
        }
    }
    Ok(output)
}

fn binding_distinct_key(
    row: &BindingRow,
    columns: &[String],
    snapshot: &Snapshot<'_>,
    rules: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(String, MemoryReservation)> {
    let mut value_memory = control.reserve(0)?;
    let values = columns
        .iter()
        .map(|column| {
            let value = expression::binding_value_with_memory(
                snapshot,
                row.values.get(column).unwrap_or(&BindingValue::Null),
                rules,
                control,
            )?;
            value_memory.merge(value.reservation)?;
            Ok(value.value)
        })
        .collect::<QueryResult<Vec<_>>>()?;
    let mut key_memory = reserve_distinct_key(&values, control)?;
    let key = distinct_row_key(&values, Some(control))?;
    key_memory.resize(crate::query::resources::checked_add(
        key.capacity() as u64,
        128,
    )?)?;
    Ok((key, key_memory))
}

pub(super) fn reserve_distinct_key(
    values: &[Value],
    control: &ExecutionControl<'_>,
) -> QueryResult<MemoryReservation> {
    let bytes = crate::query::resources::checked_add(
        crate::query::resources::checked_mul(crate::query::resources::row_bytes(values)?, 16)?,
        1024,
    )?;
    control.reserve(bytes)
}

pub(super) fn compare_keys(
    left: &[Value],
    right: &[Value],
    directions: &[(usize, crate::cypher::OrderDirectionKind)],
    order_node: &AstNode,
    control: &ExecutionControl<'_>,
) -> QueryResult<Ordering> {
    let expression_nodes = surface_expressions(order_node);
    for (index, (left, right)) in left.iter().zip(right).enumerate() {
        let mut ordering = expression::cypher_order_compare_controlled(left, right, Some(control))?;
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
    rules: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<usize>> {
    let Some(node) = body.children.iter().find(|node| node.kind == kind) else {
        return Ok(None);
    };
    let expression = surface_expressions(node)
        .into_iter()
        .next()
        .ok_or_else(|| QueryError::semantic(format!("{name} is missing its expression")))?;
    let value = expression::evaluate_with_access_control(
        &compile_expression(expression)?,
        snapshot,
        &BindingRow::default(),
        params,
        rules,
        control,
    )?;
    let Value::Integer(value) = value.value else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            format!("{name} must evaluate to an Integer"),
        ));
    };
    usize::try_from(value)
        .map(Some)
        .map_err(|_| QueryError::new(QueryErrorKind::Type, format!("{name} must be non-negative")))
}
