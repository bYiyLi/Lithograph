use super::*;
use crate::query::spill::distinct_row_key;
use std::cell::RefCell;

pub(super) fn evaluate_map(
    expression: &Expr,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
) -> QueryResult<BTreeMap<String, Value>> {
    match expression::evaluate(expression, snapshot, row, params)? {
        Value::Map(map) => Ok(map),
        Value::Null => Ok(BTreeMap::new()),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "property map expression must evaluate to Map or null",
        )),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "property-map mutation keeps storage, snapshot, delta, owner, and counters explicit"
)]
pub(super) fn set_property_map(
    context: &mut MutationContext<'_, '_>,
    staged: &Snapshot<'_>,
    owner_kind: OwnerKind,
    owner_id: i64,
    map: &BTreeMap<String, Value>,
    replace: bool,
) -> QueryResult<()> {
    if replace {
        replace_missing_properties(context, staged, owner_kind, owner_id, map)?;
    }
    for (key, value) in map {
        set_map_property(context, staged, owner_kind, owner_id, key, value)?;
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "property replacement keeps storage, snapshot, delta, owner, and counters explicit"
)]
fn replace_missing_properties(
    context: &mut MutationContext<'_, '_>,
    staged: &Snapshot<'_>,
    owner_kind: OwnerKind,
    owner_id: i64,
    map: &BTreeMap<String, Value>,
) -> QueryResult<()> {
    let names = map.keys().cloned().collect::<BTreeSet<_>>();
    for (key_id, _) in staged.properties(owner_kind, owner_id)? {
        let Some(key) = storage::property_key_name(context.connection, key_id)? else {
            return Err(QueryError::internal(format!(
                "PropertyKeyId {key_id} is missing from the dictionary"
            )));
        };
        if !names.contains(&key) {
            context.set_property(owner_kind, owner_id, key_id, None)?;
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "single property-map entries keep storage, snapshot, delta, owner, and counters explicit"
)]
fn set_map_property(
    context: &mut MutationContext<'_, '_>,
    staged: &Snapshot<'_>,
    owner_kind: OwnerKind,
    owner_id: i64,
    key: &str,
    value: &Value,
) -> QueryResult<()> {
    let next = property_from_value(value.clone())?;
    let key_id = match &next {
        Some(_) => storage::intern_property_key(context.connection, key)?,
        None => match storage::find_property_key(context.connection, key)? {
            Some(key_id) => key_id,
            None => return Ok(()),
        },
    };
    let previous = staged.property(owner_kind, owner_id, key_id)?;
    if property_states_equal(&previous, &next)? {
        return Ok(());
    }
    context.set_property(owner_kind, owner_id, key_id, next.clone())?;
    Ok(())
}

pub(crate) fn property_from_value(value: Value) -> QueryResult<Option<PropertyValue>> {
    let value = match value {
        Value::Null => return Ok(None),
        Value::Boolean(value) => PropertyValue::Boolean(value),
        Value::Integer(value) => PropertyValue::Integer(value),
        Value::Float(value) => PropertyValue::Float(value),
        Value::String(value) => PropertyValue::String(value),
        Value::List(values) => PropertyValue::List(
            values
                .into_iter()
                .map(|value| {
                    property_from_value(value)?.ok_or_else(|| {
                        QueryError::new(QueryErrorKind::Type, "property List cannot contain null")
                    })
                })
                .collect::<QueryResult<Vec<_>>>()?,
        ),
        Value::Date(value) => PropertyValue::Date(value.days()),
        Value::LocalTime(value) => PropertyValue::LocalTime(value.nanoseconds()),
        Value::Time(value) => {
            let (nanoseconds, offset_seconds) = value.storage_components();
            PropertyValue::Time {
                nanoseconds,
                offset_seconds,
            }
        }
        Value::LocalDateTime(value) => {
            let (day, nanoseconds) = value.storage_components();
            PropertyValue::LocalDateTime { day, nanoseconds }
        }
        Value::ZonedDateTime(value) => {
            let (epoch_seconds, nanoseconds, zone_id) = value.storage_components()?;
            PropertyValue::ZonedDateTime(storage::ZonedDateTimeValue {
                epoch_seconds,
                nanoseconds,
                zone_id: zone_id.to_owned(),
            })
        }
        Value::Duration(value) => {
            let (months, days, seconds, nanoseconds) = value.components();
            PropertyValue::Duration {
                months,
                days,
                seconds,
                nanoseconds,
            }
        }
        Value::Point(value) => PropertyValue::Point(storage::PointValue {
            crs: i64::from(value.srid()),
            coordinates: value.coordinates().to_vec(),
        }),
        Value::Vector(value) => PropertyValue::Vector(storage_vector(&value)),
        Value::Uuid(value) => PropertyValue::Uuid(*value.as_bytes()),
        Value::Map(_) | Value::Node(_) | Value::Relationship(_) | Value::Path(_) => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "graph properties cannot store Map, Node, Relationship, or Path values",
            ));
        }
    };
    if !value.is_property_value() {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "value is not a valid Cypher persistent property value",
        ));
    }
    Ok(Some(value))
}

fn storage_vector(value: &crate::cypher::VectorValue) -> StorageVectorValue {
    let (coordinate_type, packed) = match value.values() {
        VectorValues::I8(values) => (
            StorageVectorCoordinateType::I8,
            values.iter().map(|value| *value as u8).collect(),
        ),
        VectorValues::I16(values) => (
            StorageVectorCoordinateType::I16,
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
        ),
        VectorValues::I32(values) => (
            StorageVectorCoordinateType::I32,
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
        ),
        VectorValues::I64(values) => (
            StorageVectorCoordinateType::I64,
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
        ),
        VectorValues::F32(values) => (
            StorageVectorCoordinateType::F32,
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
        ),
        VectorValues::F64(values) => (
            StorageVectorCoordinateType::F64,
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
        ),
    };
    StorageVectorValue {
        coordinate_type,
        dimension: value.dimension() as u64,
        packed,
    }
}

pub(super) fn binding_owner(row: &BindingRow, variable: &str) -> QueryResult<(OwnerKind, i64)> {
    match row.values.get(variable) {
        Some(BindingValue::Node(id)) => Ok((OwnerKind::Node, *id)),
        Some(BindingValue::Relationship(record)) => Ok((OwnerKind::Relationship, record.id)),
        Some(BindingValue::Null) | None => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("mutation target {variable} is null"),
        )),
        Some(BindingValue::Path { .. }) => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("mutation target {variable} is a Path"),
        )),
        Some(BindingValue::Scalar(_)) => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("mutation target {variable} must be a Node or Relationship"),
        )),
    }
}

pub(super) fn binding_node(row: &BindingRow, variable: &str) -> QueryResult<i64> {
    match row.values.get(variable) {
        Some(BindingValue::Node(id)) => Ok(*id),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("label mutation target {variable} must be a Node"),
        )),
    }
}

pub(super) fn require_visible_owner(
    graph_view: &ResolvedGraphView,
    clause_input: &Snapshot<'_>,
    staged: &Snapshot<'_>,
    row: &BindingRow,
    variable: &str,
) -> QueryResult<()> {
    let visible = match row.values.get(variable) {
        Some(BindingValue::Node(id)) if clause_input.node_exists(*id)? => {
            graph_view.visible_node(clause_input, *id)?
        }
        Some(BindingValue::Node(id)) => staged.node_exists(*id)?,
        Some(BindingValue::Relationship(record)) => match clause_input.relationship(record.id)? {
            Some(input_record) => graph_view.visible_relationship(clause_input, input_record)?,
            None => staged.relationship(record.id)?.is_some(),
        },
        Some(BindingValue::Null) | None => true,
        Some(BindingValue::Path { .. } | BindingValue::Scalar(_)) => false,
    };
    if visible {
        Ok(())
    } else {
        Err(QueryError::graph_view_violation(format!(
            "mutation target {variable} is outside the active Graph View"
        )))
    }
}

impl TouchedElements {
    pub(super) fn insert(&mut self, owner_kind: OwnerKind, id: i64) {
        match owner_kind {
            OwnerKind::Node => {
                self.nodes.insert(id);
            }
            OwnerKind::Relationship => {
                self.relationships.insert(id);
            }
        }
    }
}

pub(super) fn validate_clause_view(
    connection: &Connection,
    base_commit: HashId,
    delta: &DeltaBuilder,
    selector: &GraphViewSelector,
    touched: &TouchedElements,
) -> QueryResult<()> {
    if selector.is_full_graph() {
        return Ok(());
    }
    let snapshot = staged_snapshot(connection, base_commit, delta)?;
    let graph_view = ResolvedGraphView::resolve(connection, selector)?;
    for node_id in &touched.nodes {
        if snapshot.node_exists(*node_id)? && !graph_view.visible_node(&snapshot, *node_id)? {
            return Err(QueryError::graph_view_violation(format!(
                "mutation leaves Node {node_id} outside the active Graph View"
            )));
        }
    }
    for relationship_id in &touched.relationships {
        if let Some(record) = snapshot.relationship(*relationship_id)?
            && !graph_view.visible_relationship(&snapshot, record)?
        {
            return Err(QueryError::graph_view_violation(format!(
                "mutation leaves Relationship {relationship_id} outside the active Graph View"
            )));
        }
    }
    Ok(())
}

pub(super) fn project_rows(
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    rows: Vec<BindingRow>,
    projection: &WriteProjection,
) -> QueryResult<Vec<Vec<Value>>> {
    if is_aggregate_projection(projection) {
        return project_aggregate_rows(snapshot, params, &rows, projection);
    }
    reject_mixed_aggregate_projection(projection)?;
    let mut projected = project_regular_rows(snapshot, params, rows, projection)?;
    if projection.distinct {
        projected = distinct_rows(projected)?;
    }
    if !projection.order.is_empty() {
        sort_projected_rows(&mut projected, &projection.order)?;
    }
    Ok(projected
        .into_iter()
        .skip(projection.skip)
        .take(projection.limit.unwrap_or(usize::MAX))
        .map(|(row, _)| row)
        .collect())
}

fn is_aggregate_projection(projection: &WriteProjection) -> bool {
    !projection.projections.is_empty()
        && projection
            .projections
            .iter()
            .all(|item| is_count(&item.expression))
}

fn reject_mixed_aggregate_projection(projection: &WriteProjection) -> QueryResult<()> {
    if projection
        .projections
        .iter()
        .any(|item| is_count(&item.expression))
    {
        Err(QueryError::semantic(
            "Phase 05 write RETURN does not mix aggregate and non-aggregate projections",
        ))
    } else {
        Ok(())
    }
}

fn project_aggregate_rows(
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    rows: &[BindingRow],
    projection: &WriteProjection,
) -> QueryResult<Vec<Vec<Value>>> {
    let mut counts = vec![0_i64; projection.projections.len()];
    for row in rows {
        for (index, item) in projection.projections.iter().enumerate() {
            if expression::count_contributes(&item.expression, snapshot, row, params)? {
                counts[index] = counts[index].saturating_add(1);
            }
        }
    }
    let projected = counts.into_iter().map(Value::Integer).collect::<Vec<_>>();
    validate_aggregate_order(snapshot, params, projection, &projected)?;
    if projection.skip > 0 || projection.limit == Some(0) {
        Ok(Vec::new())
    } else {
        Ok(vec![projected])
    }
}

fn validate_aggregate_order(
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    projection: &WriteProjection,
    projected: &[Value],
) -> QueryResult<()> {
    let aliases = projection
        .projections
        .iter()
        .map(|item| item.column.clone())
        .zip(projected.iter().cloned())
        .collect::<BTreeMap<_, _>>();
    let empty_binding = BindingRow::default();
    for item in &projection.order {
        if projection
            .projections
            .iter()
            .any(|projection| projection.expression == item.expression)
        {
            continue;
        }
        if contains_count(&item.expression) || !uses_only_aliases(&item.expression, &aliases) {
            return Err(QueryError::semantic(
                "Phase 05 aggregate ORDER BY supports projected count expressions, their aliases, and constant expressions only",
            ));
        }
        let _ = expression::evaluate_with_aliases(
            &item.expression,
            snapshot,
            &empty_binding,
            params,
            &aliases,
        )?;
    }
    Ok(())
}

fn contains_count(expression: &Expr) -> bool {
    match expression {
        Expr::Function {
            name,
            args: arguments,
            ..
        } => name.eq_ignore_ascii_case("count") || arguments.iter().any(contains_count),
        Expr::List(items) => items.iter().any(contains_count),
        Expr::Map(entries) => entries.values().any(contains_count),
        Expr::Case { .. }
        | Expr::ListComprehension { .. }
        | Expr::ListPredicate { .. }
        | Expr::Reduce { .. }
        | Expr::AllReduce { .. }
        | Expr::MapProjection { .. }
        | Expr::Subscript { .. }
        | Expr::IsNull { .. }
        | Expr::NormalizedPredicate { .. }
        | Expr::TypePredicate { .. }
        | Expr::LabelPredicate { .. }
        | Expr::Interpolated(_)
        | Expr::Subquery { .. }
        | Expr::PatternPredicate(_)
        | Expr::PatternComprehension(_) => false,
        Expr::Property(base, _) | Expr::Unary(_, base) => contains_count(base),
        Expr::Binary(_, left, right) => contains_count(left) || contains_count(right),
        Expr::Literal(_) | Expr::Variable(_) | Expr::Parameter(_) => false,
    }
}

fn uses_only_aliases(expression: &Expr, aliases: &BTreeMap<String, Value>) -> bool {
    match expression {
        Expr::Variable(name) => aliases.contains_key(name),
        Expr::List(items) => items.iter().all(|item| uses_only_aliases(item, aliases)),
        Expr::Map(entries) => entries
            .values()
            .all(|value| uses_only_aliases(value, aliases)),
        Expr::Property(base, _) | Expr::Unary(_, base) => uses_only_aliases(base, aliases),
        Expr::Function {
            args: arguments, ..
        } => arguments
            .iter()
            .all(|argument| uses_only_aliases(argument, aliases)),
        Expr::Binary(_, left, right) => {
            uses_only_aliases(left, aliases) && uses_only_aliases(right, aliases)
        }
        Expr::Case { .. }
        | Expr::ListComprehension { .. }
        | Expr::ListPredicate { .. }
        | Expr::Reduce { .. }
        | Expr::AllReduce { .. }
        | Expr::MapProjection { .. }
        | Expr::Subscript { .. }
        | Expr::IsNull { .. }
        | Expr::NormalizedPredicate { .. }
        | Expr::TypePredicate { .. }
        | Expr::LabelPredicate { .. }
        | Expr::Interpolated(_)
        | Expr::Subquery { .. }
        | Expr::PatternPredicate(_)
        | Expr::PatternComprehension(_) => false,
        Expr::Literal(_) | Expr::Parameter(_) => true,
    }
}

fn project_regular_rows(
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    rows: Vec<BindingRow>,
    projection: &WriteProjection,
) -> QueryResult<Vec<(Vec<Value>, Vec<Value>)>> {
    rows.into_iter()
        .map(|binding| project_binding(snapshot, params, &binding, projection))
        .collect()
}

fn project_binding(
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    binding: &BindingRow,
    projection: &WriteProjection,
) -> QueryResult<(Vec<Value>, Vec<Value>)> {
    let row = projection
        .projections
        .iter()
        .map(|item| expression::evaluate(&item.expression, snapshot, binding, params))
        .collect::<QueryResult<Vec<_>>>()?;
    let aliases = projection
        .projections
        .iter()
        .map(|item| item.column.clone())
        .zip(row.iter().cloned())
        .collect::<BTreeMap<_, _>>();
    let keys = projection
        .order
        .iter()
        .map(|item| {
            expression::evaluate_with_aliases(&item.expression, snapshot, binding, params, &aliases)
        })
        .collect::<QueryResult<Vec<_>>>()?;
    Ok((row, keys))
}

fn distinct_rows(
    projected: Vec<(Vec<Value>, Vec<Value>)>,
) -> QueryResult<Vec<(Vec<Value>, Vec<Value>)>> {
    let mut seen = BTreeSet::new();
    let mut unique = Vec::with_capacity(projected.len());
    for candidate in projected {
        if seen.insert(distinct_row_key(&candidate.0)?) {
            unique.push(candidate);
        }
    }
    Ok(unique)
}

fn sort_projected_rows(
    projected: &mut [(Vec<Value>, Vec<Value>)],
    order: &[OrderItem],
) -> QueryResult<()> {
    for (_, keys) in projected.iter() {
        for key in keys {
            let _ = crate::cypher::cypher_order_compare(key, key)?;
        }
    }
    let failure = RefCell::new(None);
    projected.sort_by(
        |left, right| match compare_order_keys(&left.1, &right.1, order) {
            Ok(ordering) => ordering,
            Err(error) => {
                let mut first = failure.borrow_mut();
                if first.is_none() {
                    *first = Some(error);
                }
                Ordering::Equal
            }
        },
    );
    match failure.into_inner() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn compare_order_keys(
    left: &[Value],
    right: &[Value],
    order: &[OrderItem],
) -> QueryResult<Ordering> {
    for ((left, right), item) in left.iter().zip(right).zip(order) {
        let ordering = crate::cypher::cypher_order_compare(left, right)?;
        if ordering != Ordering::Equal {
            return Ok(if item.descending {
                ordering.reverse()
            } else {
                ordering
            });
        }
    }
    Ok(Ordering::Equal)
}
