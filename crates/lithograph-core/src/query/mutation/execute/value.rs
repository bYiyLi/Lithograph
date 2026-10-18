use super::*;
use crate::query::access::PropertyRules;
use crate::query::spill::{
    BINDING_SPILL_BATCH_ROWS, BindingSpill, DistinctSpill, RowSpill, SortSpill, SpillOutput,
    drop_temp_table, open_spill_connection,
};

pub(super) fn resolve_property_key(
    key: &WritePropertyKey,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(String, MemoryReservation)> {
    match key {
        WritePropertyKey::Static(key) => {
            let memory = control.reserve(key.len() as u64)?;
            Ok((
                expression::clone_string_controlled(key, Some(control))?,
                memory,
            ))
        }
        WritePropertyKey::Dynamic(expression) => {
            let evaluated = expression::evaluate_with_access_control(
                expression,
                snapshot,
                row,
                params,
                query_access,
                control,
            )?;
            match evaluated.value {
                Value::String(key) if !key.is_empty() => Ok((key, evaluated.reservation)),
                Value::String(_) => {
                    Err(QueryError::semantic("dynamic property key cannot be empty"))
                }
                _ => Err(QueryError::new(
                    QueryErrorKind::Type,
                    "dynamic property key must evaluate to a non-null String",
                )),
            }
        }
    }
}

pub(super) fn visible_owner(
    staged: &Snapshot<'_>,
    clause_input: &Snapshot<'_>,
    row: &BindingRow,
    variable: &str,
    graph_view: &ResolvedGraphView,
    control: &ExecutionControl<'_>,
) -> QueryResult<(OwnerKind, i64)> {
    let owner = binding_owner(row, variable)?;
    require_visible_owner(graph_view, clause_input, staged, row, variable, control)?;
    Ok(owner)
}

pub(super) fn evaluate_map(
    expression: &Expr,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BTreeMap<String, Value>, MemoryReservation)> {
    let evaluated = expression::evaluate_with_access_control(
        expression,
        snapshot,
        row,
        params,
        query_access,
        control,
    )?;
    match evaluated.value {
        Value::Map(map) => Ok((map, evaluated.reservation)),
        Value::Null => Ok((BTreeMap::new(), evaluated.reservation)),
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
    context.access.check_write_property(key)?;
    let _conversion_memory = context
        .control
        .reserve(crate::query::resources::checked_mul(
            expression::value_bytes_with_control(value, Some(&context.control))?,
            2,
        )?)?;
    let next = property_from_value_controlled(
        expression::clone_value_unreserved_controlled(value, Some(&context.control))?,
        &context.control,
    )?;
    let key_id = match &next {
        Some(_) => storage::intern_property_key(context.connection, key)?,
        None => match storage::find_property_key(context.connection, key)? {
            Some(key_id) => key_id,
            None => return Ok(()),
        },
    };
    let equal = staged.with_property_checked(
        owner_kind,
        owner_id,
        key_id,
        &|| {
            context
                .control
                .check()
                .map_err(QueryError::into_storage_error)
        },
        |previous| property_states_equal(previous, next.as_ref(), &context.control),
    )?;
    if equal {
        return Ok(());
    }
    context.set_property(owner_kind, owner_id, key_id, next)?;
    Ok(())
}

/// The caller retains the input lease and conversion workspace until the
/// returned property has been consumed or transferred to its next owner.
pub(crate) fn property_from_value_controlled(
    value: Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<PropertyValue>> {
    property_from_value_inner(value, Some(control))
}

fn property_from_value_inner(
    value: Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<PropertyValue>> {
    if let Some(control) = control {
        control.work(1)?;
    }
    let value = match value {
        Value::Null => return Ok(None),
        Value::Boolean(value) => PropertyValue::Boolean(value),
        Value::Integer(value) => PropertyValue::Integer(value),
        Value::Float(value) => PropertyValue::Float(value),
        Value::String(value) => PropertyValue::String(value),
        Value::List(values) => PropertyValue::List(property_list(values, control)?),
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
                zone_id: expression::clone_string_controlled(zone_id, control)?,
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
        Value::Vector(value) => PropertyValue::Vector(storage_vector(&value, control)?),
        Value::Uuid(value) => PropertyValue::Uuid(*value.as_bytes()),
        Value::Map(_) | Value::Node(_) | Value::Relationship(_) | Value::Path(_) => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "graph properties cannot store Map, Node, Relationship, or Path values",
            ));
        }
    };
    let check = || {
        control
            .map_or(Ok(()), |control| control.work(1))
            .map_err(QueryError::into_storage_error)
    };
    if !value.is_property_value_checked(&check)? {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "value is not a valid Cypher persistent property value",
        ));
    }
    Ok(Some(value))
}

fn property_list(
    values: Vec<Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Vec<PropertyValue>> {
    if let Some(control) = control {
        control.check()?;
    }
    let mut output = Vec::with_capacity(values.len());
    for value in values {
        output.push(property_from_value_inner(value, control)?.ok_or_else(|| {
            QueryError::new(QueryErrorKind::Type, "property List cannot contain null")
        })?);
    }
    Ok(output)
}

fn storage_vector(
    value: &crate::cypher::VectorValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<StorageVectorValue> {
    let (coordinate_type, packed) = match value.values() {
        VectorValues::I8(values) => (
            StorageVectorCoordinateType::I8,
            pack_coordinates(values, |value| [*value as u8], control)?,
        ),
        VectorValues::I16(values) => (
            StorageVectorCoordinateType::I16,
            pack_coordinates(values, |value| value.to_le_bytes(), control)?,
        ),
        VectorValues::I32(values) => (
            StorageVectorCoordinateType::I32,
            pack_coordinates(values, |value| value.to_le_bytes(), control)?,
        ),
        VectorValues::I64(values) => (
            StorageVectorCoordinateType::I64,
            pack_coordinates(values, |value| value.to_le_bytes(), control)?,
        ),
        VectorValues::F32(values) => (
            StorageVectorCoordinateType::F32,
            pack_coordinates(values, |value| value.to_le_bytes(), control)?,
        ),
        VectorValues::F64(values) => (
            StorageVectorCoordinateType::F64,
            pack_coordinates(values, |value| value.to_le_bytes(), control)?,
        ),
    };
    Ok(StorageVectorValue {
        coordinate_type,
        dimension: value.dimension() as u64,
        packed,
    })
}

fn pack_coordinates<T, const WIDTH: usize>(
    values: &[T],
    encode: impl Fn(&T) -> [u8; WIDTH],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Vec<u8>> {
    if let Some(control) = control {
        control.check()?;
    }
    let capacity = values.len().checked_mul(WIDTH).ok_or_else(|| {
        crate::query::resources::memory_error("property Vector conversion size overflow")
    })?;
    let mut packed = Vec::with_capacity(capacity);
    for value in values {
        if let Some(control) = control {
            control.work(1)?;
        }
        packed.extend_from_slice(&encode(value));
    }
    Ok(packed)
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
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let visible = match row.values.get(variable) {
        Some(BindingValue::Node(id)) if clause_input.node_exists(*id)? => {
            graph_view.visible_node_controlled(clause_input, *id, control)?
        }
        Some(BindingValue::Node(id)) => staged.node_exists(*id)?,
        Some(BindingValue::Relationship(record)) => match clause_input.relationship(record.id)? {
            Some(input_record) => {
                graph_view.visible_relationship_controlled(clause_input, input_record, control)?
            }
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
        if !self.track {
            return;
        }
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
    snapshot: &Snapshot<'_>,
    selector: &GraphViewSelector,
    touched: &TouchedElements,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    if selector.is_full_graph() {
        return Ok(());
    }
    let graph_view = ResolvedGraphView::resolve(snapshot.connection_for_query(), selector)?;
    for node_id in &touched.nodes {
        control.work(1)?;
        if snapshot.node_exists(*node_id)?
            && !graph_view.visible_node_controlled(snapshot, *node_id, control)?
        {
            return Err(QueryError::graph_view_violation(format!(
                "mutation leaves Node {node_id} outside the active Graph View"
            )));
        }
    }
    for relationship_id in &touched.relationships {
        control.work(1)?;
        if let Some(record) = snapshot.relationship(*relationship_id)?
            && !graph_view.visible_relationship_controlled(snapshot, record, control)?
        {
            return Err(QueryError::graph_view_violation(format!(
                "mutation leaves Relationship {relationship_id} outside the active Graph View"
            )));
        }
    }
    Ok(())
}

struct ProjectionExecution<'a, 'connection, 'control> {
    snapshot: &'a Snapshot<'connection>,
    params: &'a BTreeMap<String, Value>,
    projection: &'a WriteProjection,
    query_access: &'a PropertyRules,
    is_interrupted: &'a dyn Fn() -> bool,
    control: &'a ExecutionControl<'control>,
}

fn project_rows(
    rows: Vec<BindingRow>,
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<Option<WriteRows>> {
    if is_aggregate_projection(execution.projection) {
        return project_aggregate_write_rows(&rows, execution);
    }
    reject_mixed_aggregate_projection(execution.projection)?;
    if execution.projection.order.is_empty() && !execution.projection.distinct {
        return project_direct_rows(rows, execution);
    }
    project_barrier_rows(rows, execution)
}

fn project_aggregate_write_rows(
    rows: &[BindingRow],
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<Option<WriteRows>> {
    let rows = project_aggregate_rows(rows, execution)?;
    spill_value_rows(rows, execution.control)
}

fn project_barrier_rows(
    rows: Vec<BindingRow>,
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<Option<WriteRows>> {
    let connection = open_spill_connection()?;
    let output = if execution.projection.order.is_empty() {
        distinct_output_from_rows(&connection, rows, execution)?
    } else {
        sorted_output_from_rows(&connection, rows, execution)?
    };
    select_spill_output(
        connection,
        output,
        execution.projection.skip,
        execution.projection.limit,
        execution.control,
    )
}

fn distinct_output_from_rows(
    connection: &Connection,
    rows: Vec<BindingRow>,
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<SpillOutput> {
    let mut spill =
        DistinctSpill::create_with_resources(connection, execution.control.resources().clone())?;
    for binding in rows {
        project_and_push(
            &binding,
            |row, _| spill.push(connection, &row, execution.control),
            execution,
        )?;
    }
    Ok(spill.output())
}

fn sorted_output_from_rows(
    connection: &Connection,
    rows: Vec<BindingRow>,
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<SpillOutput> {
    let directions = execution
        .projection
        .order
        .iter()
        .map(|item| item.descending)
        .collect::<Vec<_>>();
    let mut spill = SortSpill::create_with_resources(
        connection,
        execution.projection.distinct,
        directions,
        execution.control.resources().clone(),
    )?;
    for binding in rows {
        project_and_push(
            &binding,
            |row, keys| spill.push(connection, row, keys, execution.control),
            execution,
        )?;
    }
    finish_sort_output(connection, spill, execution.control)
}

fn finish_sort_output(
    connection: &Connection,
    mut spill: SortSpill,
    control: &ExecutionControl<'_>,
) -> QueryResult<SpillOutput> {
    let total = match spill.finish(connection, control).and_then(|total| {
        spill.cleanup_aux(connection)?;
        Ok(total)
    }) {
        Ok(total) => total,
        Err(error) => {
            let _ = spill.abort(connection);
            return Err(error);
        }
    };
    Ok(spill.into_output(total))
}

#[allow(
    clippy::too_many_arguments,
    reason = "write projection keeps the execution-local property policy explicit"
)]
pub(super) fn project_spilled_rows(
    main_connection: &Connection,
    snapshot_state: crate::storage::ResolvedSnapshotState,
    params: &BTreeMap<String, Value>,
    spill_connection: Connection,
    rows: BindingSpill,
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<WriteRows>> {
    if projection.order.is_empty() && !projection.distinct && !is_aggregate_projection(projection) {
        reject_mixed_aggregate_projection(projection)?;
        return project_lazy_spilled_rows(
            snapshot_state,
            params,
            spill_connection,
            rows,
            projection,
            query_access,
            control,
        );
    }

    let snapshot = Snapshot::from_resolved_state(main_connection, &snapshot_state);
    let execution = ProjectionExecution {
        snapshot: &snapshot,
        params,
        projection,
        query_access,
        is_interrupted,
        control,
    };
    if is_aggregate_projection(projection) {
        let (collected, _collected_memory) = rows.collect_reserved(&spill_connection, control)?;
        rows.abort(&spill_connection)?;
        return project_rows(collected, &execution);
    }
    reject_mixed_aggregate_projection(projection)?;
    project_spilled_barrier_rows(spill_connection, rows, &execution)
}

fn project_lazy_spilled_rows(
    snapshot_state: crate::storage::ResolvedSnapshotState,
    params: &BTreeMap<String, Value>,
    spill_connection: Connection,
    rows: BindingSpill,
    projection: &WriteProjection,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<WriteRows>> {
    let available = rows.len().saturating_sub(projection.skip);
    let total = projection
        .limit
        .map_or(available, |limit| available.min(limit));
    if total == 0 {
        rows.abort(&spill_connection)?;
        return Ok(None);
    }
    Ok(Some(WriteRows {
        batch_memory: None,
        kind: WriteRowsKind::Projected(ProjectedWriteRows {
            connection: spill_connection,
            bindings: rows,
            after: -1,
            total,
            snapshot_state: Box::new(snapshot_state),
            _params_memory: reserve_params(params, control)?,
            params: expression::clone_map_controlled(params, Some(control))?,
            projection: projection.clone(),
            query_access: Box::new(query_access.clone()),
            seen: 0,
            emitted: 0,
        }),
    }))
}

#[allow(
    clippy::too_many_arguments,
    reason = "write projection keeps the execution-local property policy explicit"
)]
fn project_spilled_barrier_rows(
    spill_connection: Connection,
    rows: BindingSpill,
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<Option<WriteRows>> {
    let output_connection = open_spill_connection()?;
    let output = if execution.projection.order.is_empty() {
        distinct_output_from_spill(&output_connection, &spill_connection, &rows, execution)?
    } else {
        sorted_output_from_spill(&output_connection, &spill_connection, &rows, execution)?
    };
    rows.abort(&spill_connection)?;
    select_spill_output(
        output_connection,
        output,
        execution.projection.skip,
        execution.projection.limit,
        execution.control,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "write projection keeps the execution-local property policy explicit"
)]
fn distinct_output_from_spill(
    output_connection: &Connection,
    spill_connection: &Connection,
    rows: &BindingSpill,
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<SpillOutput> {
    // #lizard forgives(parameter_count)
    let mut output = DistinctSpill::create_with_resources(
        output_connection,
        execution.control.resources().clone(),
    )?;
    rows.for_each_batch(
        spill_connection,
        BINDING_SPILL_BATCH_ROWS,
        execution.control,
        |bindings| {
            for binding in bindings {
                project_and_push(
                    &binding,
                    |row, _| output.push(output_connection, &row, execution.control),
                    execution,
                )?;
            }
            Ok(())
        },
    )?;
    Ok(output.output())
}

#[allow(
    clippy::too_many_arguments,
    reason = "write projection keeps the execution-local property policy explicit"
)]
fn sorted_output_from_spill(
    output_connection: &Connection,
    spill_connection: &Connection,
    rows: &BindingSpill,
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<SpillOutput> {
    // #lizard forgives(parameter_count)
    let directions = execution
        .projection
        .order
        .iter()
        .map(|item| item.descending)
        .collect::<Vec<_>>();
    let mut output = SortSpill::create_with_resources(
        output_connection,
        execution.projection.distinct,
        directions,
        execution.control.resources().clone(),
    )?;
    rows.for_each_batch(
        spill_connection,
        BINDING_SPILL_BATCH_ROWS,
        execution.control,
        |bindings| {
            for binding in bindings {
                project_and_push(
                    &binding,
                    |row, keys| output.push(output_connection, row, keys, execution.control),
                    execution,
                )?;
            }
            Ok(())
        },
    )?;
    finish_sort_output(output_connection, output, execution.control)
}

fn project_and_push(
    binding: &BindingRow,
    push: impl FnOnce(Vec<Value>, Vec<Value>) -> QueryResult<()>,
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<()> {
    check_interrupted(execution.is_interrupted)?;
    let (row, keys, _memory) = project_binding(binding, execution)?;
    push(row, keys)
}

fn project_direct_rows(
    rows: Vec<BindingRow>,
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<Option<WriteRows>> {
    let connection = open_spill_connection()?;
    let mut spill =
        RowSpill::create_with_resources(&connection, execution.control.resources().clone())?;
    let mut seen = 0usize;
    let mut emitted = 0usize;
    let limit = execution.projection.limit.unwrap_or(usize::MAX);
    for binding in rows {
        check_interrupted(execution.is_interrupted)?;
        if emitted >= limit {
            break;
        }
        let (row, _, _memory) = project_binding(&binding, execution)?;
        if seen < execution.projection.skip {
            seen = seen.saturating_add(1);
            continue;
        }
        seen = seen.saturating_add(1);
        spill.push(&connection, &row, execution.control)?;
        emitted = emitted.saturating_add(1);
    }
    write_rows_from_output(connection, spill.output())
}

pub(crate) fn spill_value_rows(
    rows: Vec<Vec<Value>>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<WriteRows>> {
    if rows.is_empty() {
        return Ok(None);
    }
    let connection = open_spill_connection()?;
    let mut spill = RowSpill::create_with_resources(&connection, control.resources().clone())?;
    spill.push_all(&connection, &rows, control)?;
    write_rows_from_output(connection, spill.output())
}

fn select_spill_output(
    connection: Connection,
    mut source: SpillOutput,
    skip: usize,
    limit: Option<usize>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<WriteRows>> {
    let mut selected = RowSpill::create_with_resources(&connection, control.resources().clone())?;
    let mut seen = 0usize;
    let mut emitted = 0usize;
    let limit = limit.unwrap_or(usize::MAX);
    while emitted < limit {
        let Some((sequence, row, _row_memory)) =
            crate::query::spill::read_output_row_with_resources(
                &connection,
                &source.table,
                source.after,
                control,
            )?
        else {
            break;
        };
        source.after = sequence;
        if seen < skip {
            seen = seen.saturating_add(1);
            continue;
        }
        seen = seen.saturating_add(1);
        selected.push(&connection, &row, control)?;
        emitted = emitted.saturating_add(1);
    }
    drop_temp_table(&connection, &source.table)?;
    write_rows_from_output(connection, selected.output())
}

fn write_rows_from_output(
    connection: Connection,
    output: SpillOutput,
) -> QueryResult<Option<WriteRows>> {
    if output.total == 0 {
        drop_temp_table(&connection, &output.table)?;
        return Ok(None);
    }
    Ok(Some(WriteRows {
        batch_memory: None,
        kind: WriteRowsKind::Materialized(MaterializedWriteRows { connection, output }),
    }))
}

impl WriteRows {
    pub(crate) fn total(&self) -> usize {
        match &self.kind {
            WriteRowsKind::Materialized(rows) => rows.output.total,
            WriteRowsKind::Projected(rows) => rows.total,
        }
    }

    pub(crate) fn take_batch_memory(&mut self) -> Option<MemoryReservation> {
        self.batch_memory.take()
    }

    pub(crate) fn next_batch(
        &mut self,
        main_connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Vec<Vec<Value>>> {
        self.batch_memory = None;
        let (rows, memory) = match &mut self.kind {
            WriteRowsKind::Materialized(rows) => {
                rows.next_batch(max_rows, is_interrupted, control)?
            }
            WriteRowsKind::Projected(rows) => {
                rows.next_batch(main_connection, max_rows, is_interrupted, control)?
            }
        };
        self.batch_memory = Some(memory);
        Ok(rows)
    }
}

impl MaterializedWriteRows {
    fn next_batch(
        &mut self,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<(Vec<Vec<Value>>, MemoryReservation)> {
        let mut memory = control.reserve(
            crate::query::resources::allocation_bytes::<Vec<Value>>(max_rows)?,
        )?;
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows && self.output.after < self.output.total as i64 - 1 {
            check_interrupted(is_interrupted)?;
            let Some((sequence, row, row_memory)) =
                crate::query::spill::read_output_row_with_resources(
                    &self.connection,
                    &self.output.table,
                    self.output.after,
                    control,
                )?
            else {
                return Err(QueryError::internal(
                    "write result spill ended before its declared row count",
                ));
            };
            memory.merge(row_memory)?;
            self.output.after = sequence;
            rows.push(row);
        }
        Ok((rows, memory))
    }
}

impl ProjectedWriteRows {
    fn next_batch(
        &mut self,
        main_connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<(Vec<Vec<Value>>, MemoryReservation)> {
        let snapshot = Snapshot::from_resolved_state(main_connection, self.snapshot_state.as_ref());
        let execution = ProjectionExecution {
            snapshot: &snapshot,
            params: &self.params,
            projection: &self.projection,
            query_access: &self.query_access,
            is_interrupted,
            control,
        };
        let mut memory = control.reserve(
            crate::query::resources::allocation_bytes::<Vec<Value>>(max_rows)?,
        )?;
        let mut output = Vec::with_capacity(max_rows);
        while output.len() < max_rows && self.emitted < self.total {
            check_interrupted(is_interrupted)?;
            let (batch, _batch_memory) = self.bindings.read_batch_reserved(
                &self.connection,
                self.after,
                BINDING_SPILL_BATCH_ROWS,
                control,
            )?;
            if batch.is_empty() {
                return Err(QueryError::internal(
                    "write binding spill ended before its declared row count",
                ));
            }
            for (sequence, binding) in batch {
                if self.emitted >= self.total || output.len() >= max_rows {
                    break;
                }
                if self.seen < self.projection.skip {
                    self.after = sequence;
                    self.seen = self.seen.saturating_add(1);
                    continue;
                }
                let (row, _, row_memory) = project_binding(&binding, &execution)?;
                memory.merge(row_memory)?;
                self.after = sequence;
                self.seen = self.seen.saturating_add(1);
                self.emitted = self.emitted.saturating_add(1);
                output.push(row);
            }
        }
        Ok((output, memory))
    }
}

pub(crate) fn materialize_binding_rows_to_spill(
    snapshot: &Snapshot<'_>,
    columns: &[String],
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<WriteRows>> {
    let connection = open_spill_connection()?;
    let mut spill = RowSpill::create_with_resources(&connection, control.resources().clone())?;
    for row in rows {
        check_interrupted(is_interrupted)?;
        let mut memory = control.reserve(0)?;
        let values = columns
            .iter()
            .map(|column| {
                let evaluated = expression::binding_value_with_memory(
                    snapshot,
                    row.values.get(column).unwrap_or(&BindingValue::Null),
                    query_access,
                    control,
                )?;
                memory.merge(evaluated.reservation)?;
                Ok(evaluated.value)
            })
            .collect::<QueryResult<Vec<_>>>()?;
        spill.push(&connection, &values, control)?;
    }
    write_rows_from_output(connection, spill.output())
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
    rows: &[BindingRow],
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<Vec<Vec<Value>>> {
    let mut counts = vec![0_i64; execution.projection.projections.len()];
    for row in rows {
        check_interrupted(execution.is_interrupted)?;
        for (index, item) in execution.projection.projections.iter().enumerate() {
            if expression::count_contributes_with_access_control(
                &item.expression,
                execution.snapshot,
                row,
                execution.params,
                execution.query_access,
                Some(execution.control),
            )? {
                counts[index] = counts[index].saturating_add(1);
            }
        }
    }
    let projected = counts.into_iter().map(Value::Integer).collect::<Vec<_>>();
    validate_aggregate_order(&projected, execution)?;
    if execution.projection.skip > 0 || execution.projection.limit == Some(0) {
        Ok(Vec::new())
    } else {
        Ok(vec![projected])
    }
}

fn validate_aggregate_order(
    projected: &[Value],
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<()> {
    let aliases = execution
        .projection
        .projections
        .iter()
        .map(|item| item.column.clone())
        .zip(projected.iter().cloned())
        .collect::<BTreeMap<_, _>>();
    let empty_binding = BindingRow::default();
    for item in &execution.projection.order {
        if execution
            .projection
            .projections
            .iter()
            .any(|projected| projected.expression == item.expression)
        {
            continue;
        }
        if contains_count(&item.expression) || !uses_only_aliases(&item.expression, &aliases) {
            return Err(QueryError::semantic(
                "Phase 05 aggregate ORDER BY supports projected count expressions, their aliases, and constant expressions only",
            ));
        }
        let _ = expression::evaluate_with_aliases_and_access_control(
            &item.expression,
            execution.snapshot,
            &empty_binding,
            execution.params,
            &aliases,
            execution.query_access,
            execution.control,
        )?
        .value;
    }
    Ok(())
}

fn opaque_write_projection_expression(expression: &Expr) -> bool {
    matches!(
        expression,
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
            | Expr::PatternComprehension(_)
    )
}

fn contains_count(expression: &Expr) -> bool {
    if opaque_write_projection_expression(expression) {
        return false;
    }
    match expression {
        Expr::Function {
            name,
            args: arguments,
            ..
        } => name.eq_ignore_ascii_case("count") || arguments.iter().any(contains_count),
        Expr::List(items) => items.iter().any(contains_count),
        Expr::Map(entries) => entries.values().any(contains_count),
        Expr::Property(base, _) | Expr::Unary(_, base) => contains_count(base),
        Expr::Binary(_, left, right) => contains_count(left) || contains_count(right),
        Expr::Literal(_) | Expr::Variable(_) | Expr::Parameter(_) => false,
        _ => false,
    }
}

fn uses_only_aliases(expression: &Expr, aliases: &BTreeMap<String, Value>) -> bool {
    if opaque_write_projection_expression(expression) {
        return false;
    }
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
        Expr::Literal(_) | Expr::Parameter(_) => true,
        _ => false,
    }
}

fn reserve_params(
    params: &BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<MemoryReservation> {
    let bytes = params.iter().try_fold(0_u64, |bytes, (name, value)| {
        crate::query::resources::checked_add(
            bytes,
            crate::query::resources::checked_add(
                name.capacity() as u64 + 192,
                expression::value_bytes_with_control(value, Some(control))?,
            )?,
        )
    })?;
    control.reserve(bytes)
}

fn project_binding(
    binding: &BindingRow,
    execution: &ProjectionExecution<'_, '_, '_>,
) -> QueryResult<(Vec<Value>, Vec<Value>, MemoryReservation)> {
    let mut memory = execution.control.reserve(0)?;
    let mut row = Vec::with_capacity(execution.projection.projections.len());
    for item in &execution.projection.projections {
        let evaluated = expression::evaluate_with_access_control(
            &item.expression,
            execution.snapshot,
            binding,
            execution.params,
            execution.query_access,
            execution.control,
        )?;
        memory.merge(evaluated.reservation)?;
        row.push(evaluated.value);
    }
    let alias_bytes = execution.projection.projections.iter().zip(&row).try_fold(
        0_u64,
        |bytes, (item, value)| {
            crate::query::resources::checked_add(
                bytes,
                crate::query::resources::checked_add(
                    item.column.capacity() as u64 + 192,
                    expression::value_bytes_with_control(value, Some(execution.control))?,
                )?,
            )
        },
    )?;
    let _alias_memory = execution.control.reserve(alias_bytes)?;
    let aliases = execution
        .projection
        .projections
        .iter()
        .map(|item| item.column.clone())
        .zip(row.iter().cloned())
        .collect::<BTreeMap<_, _>>();
    let mut keys = Vec::with_capacity(execution.projection.order.len());
    for item in &execution.projection.order {
        let evaluated = expression::evaluate_with_aliases_and_access_control(
            &item.expression,
            execution.snapshot,
            binding,
            execution.params,
            &aliases,
            execution.query_access,
            execution.control,
        )?;
        memory.merge(evaluated.reservation)?;
        keys.push(evaluated.value);
    }
    Ok((row, keys, memory))
}
