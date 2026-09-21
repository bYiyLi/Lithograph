use super::*;
use crate::query::spill::{
    BINDING_SPILL_BATCH_ROWS, BindingSpill, DistinctSpill, RowSpill, SortSpill, SpillOutput,
    drop_temp_table, open_spill_connection, read_output_row,
};

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
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<WriteRows>> {
    if is_aggregate_projection(projection) {
        return project_aggregate_write_rows(snapshot, params, &rows, projection, is_interrupted);
    }
    reject_mixed_aggregate_projection(projection)?;
    if projection.order.is_empty() && !projection.distinct {
        return project_direct_rows(snapshot, params, rows, projection, is_interrupted);
    }
    project_barrier_rows(snapshot, params, rows, projection, is_interrupted)
}

fn project_aggregate_write_rows(
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    rows: &[BindingRow],
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<WriteRows>> {
    let rows = project_aggregate_rows(snapshot, params, rows, projection, is_interrupted)?;
    spill_value_rows(rows)
}

fn project_barrier_rows(
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    rows: Vec<BindingRow>,
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<WriteRows>> {
    let connection = open_spill_connection()?;
    let output = if projection.order.is_empty() {
        distinct_output_from_rows(
            &connection,
            snapshot,
            params,
            rows,
            projection,
            is_interrupted,
        )?
    } else {
        sorted_output_from_rows(
            &connection,
            snapshot,
            params,
            rows,
            projection,
            is_interrupted,
        )?
    };
    select_spill_output(connection, output, projection.skip, projection.limit)
}

fn distinct_output_from_rows(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    rows: Vec<BindingRow>,
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpillOutput> {
    let mut spill = DistinctSpill::create(connection)?;
    for binding in rows {
        project_and_push(
            snapshot,
            params,
            &binding,
            projection,
            is_interrupted,
            |row, _| spill.push(connection, &row),
        )?;
    }
    Ok(spill.output())
}

fn sorted_output_from_rows(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    rows: Vec<BindingRow>,
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpillOutput> {
    let directions = projection
        .order
        .iter()
        .map(|item| item.descending)
        .collect::<Vec<_>>();
    let mut spill = SortSpill::create(connection, projection.distinct, directions)?;
    for binding in rows {
        project_and_push(
            snapshot,
            params,
            &binding,
            projection,
            is_interrupted,
            |row, keys| spill.push(connection, row, keys),
        )?;
    }
    finish_sort_output(connection, spill, is_interrupted)
}

fn finish_sort_output(
    connection: &Connection,
    mut spill: SortSpill,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpillOutput> {
    let total = match spill.finish(connection, is_interrupted).and_then(|total| {
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

pub(super) fn project_spilled_rows(
    main_connection: &Connection,
    snapshot_state: crate::storage::ResolvedSnapshotState,
    params: &BTreeMap<String, Value>,
    spill_connection: Connection,
    rows: BindingSpill,
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<WriteRows>> {
    if projection.order.is_empty() && !projection.distinct && !is_aggregate_projection(projection) {
        reject_mixed_aggregate_projection(projection)?;
        return project_lazy_spilled_rows(
            snapshot_state,
            params,
            spill_connection,
            rows,
            projection,
        );
    }

    let snapshot = Snapshot::from_resolved_state(main_connection, &snapshot_state);
    if is_aggregate_projection(projection) {
        let collected = rows.collect(&spill_connection)?;
        rows.abort(&spill_connection)?;
        return project_rows(&snapshot, params, collected, projection, is_interrupted);
    }
    reject_mixed_aggregate_projection(projection)?;
    project_spilled_barrier_rows(
        &snapshot,
        params,
        spill_connection,
        rows,
        projection,
        is_interrupted,
    )
}

fn project_lazy_spilled_rows(
    snapshot_state: crate::storage::ResolvedSnapshotState,
    params: &BTreeMap<String, Value>,
    spill_connection: Connection,
    rows: BindingSpill,
    projection: &WriteProjection,
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
        kind: WriteRowsKind::Projected(ProjectedWriteRows {
            connection: spill_connection,
            bindings: rows,
            after: -1,
            total,
            snapshot_state: Box::new(snapshot_state),
            params: params.clone(),
            projection: projection.clone(),
            seen: 0,
            emitted: 0,
        }),
    }))
}

fn project_spilled_barrier_rows(
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    spill_connection: Connection,
    rows: BindingSpill,
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<WriteRows>> {
    let output_connection = open_spill_connection()?;
    let output = if projection.order.is_empty() {
        distinct_output_from_spill(
            &output_connection,
            snapshot,
            params,
            &spill_connection,
            &rows,
            projection,
            is_interrupted,
        )?
    } else {
        sorted_output_from_spill(
            &output_connection,
            snapshot,
            params,
            &spill_connection,
            &rows,
            projection,
            is_interrupted,
        )?
    };
    rows.abort(&spill_connection)?;
    select_spill_output(output_connection, output, projection.skip, projection.limit)
}

fn distinct_output_from_spill(
    output_connection: &Connection,
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    spill_connection: &Connection,
    rows: &BindingSpill,
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpillOutput> {
    // #lizard forgives(parameter_count)
    let mut output = DistinctSpill::create(output_connection)?;
    rows.for_each_batch(spill_connection, BINDING_SPILL_BATCH_ROWS, |bindings| {
        for binding in bindings {
            project_and_push(
                snapshot,
                params,
                &binding,
                projection,
                is_interrupted,
                |row, _| output.push(output_connection, &row),
            )?;
        }
        Ok(())
    })?;
    Ok(output.output())
}

fn sorted_output_from_spill(
    output_connection: &Connection,
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    spill_connection: &Connection,
    rows: &BindingSpill,
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpillOutput> {
    // #lizard forgives(parameter_count)
    let directions = projection
        .order
        .iter()
        .map(|item| item.descending)
        .collect::<Vec<_>>();
    let mut output = SortSpill::create(output_connection, projection.distinct, directions)?;
    rows.for_each_batch(spill_connection, BINDING_SPILL_BATCH_ROWS, |bindings| {
        for binding in bindings {
            project_and_push(
                snapshot,
                params,
                &binding,
                projection,
                is_interrupted,
                |row, keys| output.push(output_connection, row, keys),
            )?;
        }
        Ok(())
    })?;
    finish_sort_output(output_connection, output, is_interrupted)
}

fn project_and_push(
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    binding: &BindingRow,
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
    push: impl FnOnce(Vec<Value>, Vec<Value>) -> QueryResult<()>,
) -> QueryResult<()> {
    check_interrupted(is_interrupted)?;
    let (row, keys) = project_binding(snapshot, params, binding, projection)?;
    push(row, keys)
}

fn project_direct_rows(
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    rows: Vec<BindingRow>,
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<WriteRows>> {
    let connection = open_spill_connection()?;
    let mut spill = RowSpill::create(&connection)?;
    let mut seen = 0usize;
    let mut emitted = 0usize;
    let limit = projection.limit.unwrap_or(usize::MAX);
    for binding in rows {
        check_interrupted(is_interrupted)?;
        if emitted >= limit {
            break;
        }
        let (row, _) = project_binding(snapshot, params, &binding, projection)?;
        if seen < projection.skip {
            seen = seen.saturating_add(1);
            continue;
        }
        seen = seen.saturating_add(1);
        spill.push(&connection, &row)?;
        emitted = emitted.saturating_add(1);
    }
    write_rows_from_output(connection, spill.output())
}

pub(crate) fn spill_value_rows(rows: Vec<Vec<Value>>) -> QueryResult<Option<WriteRows>> {
    if rows.is_empty() {
        return Ok(None);
    }
    let connection = open_spill_connection()?;
    let mut spill = RowSpill::create(&connection)?;
    spill.push_all(&connection, &rows)?;
    write_rows_from_output(connection, spill.output())
}

fn select_spill_output(
    connection: Connection,
    mut source: SpillOutput,
    skip: usize,
    limit: Option<usize>,
) -> QueryResult<Option<WriteRows>> {
    let mut selected = RowSpill::create(&connection)?;
    let mut seen = 0usize;
    let mut emitted = 0usize;
    let limit = limit.unwrap_or(usize::MAX);
    while emitted < limit {
        let Some((sequence, row)) = read_output_row(&connection, &source.table, source.after)?
        else {
            break;
        };
        source.after = sequence;
        if seen < skip {
            seen = seen.saturating_add(1);
            continue;
        }
        seen = seen.saturating_add(1);
        selected.push(&connection, &row)?;
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

    pub(crate) fn next_batch(
        &mut self,
        main_connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<Vec<Vec<Value>>> {
        match &mut self.kind {
            WriteRowsKind::Materialized(rows) => rows.next_batch(max_rows, is_interrupted),
            WriteRowsKind::Projected(rows) => {
                rows.next_batch(main_connection, max_rows, is_interrupted)
            }
        }
    }
}

impl MaterializedWriteRows {
    fn next_batch(
        &mut self,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<Vec<Vec<Value>>> {
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows && self.output.after < self.output.total as i64 - 1 {
            check_interrupted(is_interrupted)?;
            let Some((sequence, row)) =
                read_output_row(&self.connection, &self.output.table, self.output.after)?
            else {
                return Err(QueryError::internal(
                    "write result spill ended before its declared row count",
                ));
            };
            self.output.after = sequence;
            rows.push(row);
        }
        Ok(rows)
    }
}

impl ProjectedWriteRows {
    fn next_batch(
        &mut self,
        main_connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<Vec<Vec<Value>>> {
        let snapshot = Snapshot::from_resolved_state(main_connection, self.snapshot_state.as_ref());
        let mut output = Vec::with_capacity(max_rows);
        while output.len() < max_rows && self.emitted < self.total {
            check_interrupted(is_interrupted)?;
            let batch =
                self.bindings
                    .read_batch(&self.connection, self.after, BINDING_SPILL_BATCH_ROWS)?;
            if batch.is_empty() {
                return Err(QueryError::internal(
                    "write binding spill ended before its declared row count",
                ));
            }
            self.append_batch(&snapshot, batch, max_rows, &mut output)?;
        }
        Ok(output)
    }

    fn append_batch(
        &mut self,
        snapshot: &Snapshot<'_>,
        batch: Vec<(i64, BindingRow)>,
        max_rows: usize,
        output: &mut Vec<Vec<Value>>,
    ) -> QueryResult<()> {
        for (sequence, binding) in batch {
            if self.emitted >= self.total || output.len() >= max_rows {
                break;
            }
            if self.seen < self.projection.skip {
                self.after = sequence;
                self.seen = self.seen.saturating_add(1);
                continue;
            }
            let (row, _) = project_binding(snapshot, &self.params, &binding, &self.projection)?;
            self.after = sequence;
            self.seen = self.seen.saturating_add(1);
            self.emitted = self.emitted.saturating_add(1);
            output.push(row);
        }
        Ok(())
    }
}

pub(crate) fn materialize_binding_rows_to_spill(
    snapshot: &Snapshot<'_>,
    columns: &[String],
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<WriteRows>> {
    let connection = open_spill_connection()?;
    let mut spill = RowSpill::create(&connection)?;
    for row in rows {
        check_interrupted(is_interrupted)?;
        let values = columns
            .iter()
            .map(|column| {
                expression::binding_value(
                    snapshot,
                    row.values.get(column).unwrap_or(&BindingValue::Null),
                )
            })
            .collect::<QueryResult<Vec<_>>>()?;
        spill.push(&connection, &values)?;
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
    snapshot: &Snapshot<'_>,
    params: &BTreeMap<String, Value>,
    rows: &[BindingRow],
    projection: &WriteProjection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<Vec<Value>>> {
    let mut counts = vec![0_i64; projection.projections.len()];
    for row in rows {
        check_interrupted(is_interrupted)?;
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
