use super::*;
use crate::cypher::{AstKind, AstNode, ClauseKind, ExpressionKind, QueryConnector, SubqueryKind};
use crate::query::LogicalOperator;
use crate::query::expression::Expr;
use crate::query::ingestion::{
    CompiledLoadCsv, CsvStream, compile_load_csv, csv_binding_row, evaluate_csv_source,
};
use crate::query::plan::{OrderItem, Projection, ProjectionPlan};
use crate::query::resources::{ExecutionControl, MemoryReservation};
use crate::query::spill::{BINDING_SPILL_BATCH_ROWS, BindingSpill};

#[derive(Debug)]
pub(crate) struct ProgramStream {
    source: Box<dyn BindingSource>,
    columns: Vec<String>,
    output_memory: Option<MemoryReservation>,
}

struct ProgramRuntime<'a, 'connection> {
    connection: &'connection Connection,
    snapshot: &'a Snapshot<'connection>,
    graph_view: &'a ResolvedGraphView,
    params: &'a std::collections::BTreeMap<String, Value>,
    metrics: &'a mut QueryMetrics,
    is_interrupted: &'a dyn Fn() -> bool,
    control: &'a ExecutionControl<'a>,
    expression_memory: MemoryReservation,
}

impl ProgramRuntime<'_, '_> {
    fn copy_binding(&mut self, row: &BindingRow) -> QueryResult<BindingRow> {
        let mut copied = crate::query::completeness::execute::RowSet::try_clone_rows(
            &row.order,
            std::slice::from_ref(row),
            self.control,
        )?;
        if let Some(memory) = copied.memory.take() {
            self.expression_memory.merge(memory)?;
        }
        copied
            .rows
            .pop()
            .ok_or_else(|| QueryError::internal("streamed copy lost its row"))
    }

    fn evaluate(&mut self, expr: &Expr, row: &BindingRow) -> QueryResult<Value> {
        let evaluated = expression::evaluate_with_access_control(
            expr,
            self.snapshot,
            row,
            self.params,
            &self.graph_view.query_access,
            self.control,
        )?;
        self.expression_memory.merge(evaluated.reservation)?;
        Ok(evaluated.value)
    }
}

trait BindingSource: std::fmt::Debug {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>>;
}

#[derive(Debug)]
struct ConcatSource {
    left: Box<dyn BindingSource>,
    right: Box<dyn BindingSource>,
    left_done: bool,
}

impl BindingSource for ConcatSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        if !self.left_done {
            if let Some(row) = self.left.next(runtime)? {
                return Ok(Some(row));
            }
            self.left_done = true;
        }
        self.right.next(runtime)
    }
}

#[derive(Debug)]
struct UnitMapSource {
    upstream: Box<dyn BindingSource>,
}

impl BindingSource for UnitMapSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        self.upstream
            .next(runtime)
            .map(|row| row.map(|_| BindingRow::default()))
    }
}

#[derive(Debug)]
struct DistinctBindingSource {
    upstream: Box<dyn BindingSource>,
    columns: Vec<String>,
    state: DistinctBindingState,
}

#[derive(Debug)]
enum DistinctBindingState {
    Pending,
    Ready {
        connection: Connection,
        output: SpillOutput,
    },
    Done,
}

impl BindingSource for DistinctBindingSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        if matches!(self.state, DistinctBindingState::Pending) {
            let connection = open_spill_connection()?;
            let mut spill = DistinctSpill::create_with_resources(
                &connection,
                runtime.control.resources().clone(),
            )?;
            while let Some(binding) = self.upstream.next(runtime)? {
                check_interrupted(runtime.is_interrupted)?;
                let (row, _row_memory) = materialized_binding_values(
                    runtime.snapshot,
                    &binding,
                    &self.columns,
                    &runtime.graph_view.query_access,
                    runtime.control,
                )?;
                spill.push(&connection, &row, runtime.control)?;
                drop(binding);
                runtime.expression_memory.resize(0)?;
            }
            self.state = DistinctBindingState::Ready {
                connection,
                output: spill.output(),
            };
        }
        let DistinctBindingState::Ready { connection, output } = &mut self.state else {
            return Ok(None);
        };
        let Some((row, _row_memory)) = next_program_spill_row(connection, output, runtime.control)?
        else {
            self.state = DistinctBindingState::Done;
            return Ok(None);
        };
        projected_binding(runtime.snapshot, &self.columns, row, runtime.control).map(Some)
    }
}

#[derive(Debug)]
struct BatchClauseSource {
    upstream: Box<dyn BindingSource>,
    clause: AstNode,
    source: String,
    input_columns: Vec<String>,
    output_columns: Vec<String>,
    buffered: std::collections::VecDeque<BindingRow>,
    buffered_memory: Option<MemoryReservation>,
    done: bool,
    input_batch_rows: usize,
}

impl BindingSource for BatchClauseSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        loop {
            if let Some(row) = self.buffered.pop_front() {
                return Ok(Some(row));
            }
            if self.done {
                return Ok(None);
            }
            self.buffered_memory = None;
            let baseline = runtime.expression_memory.bytes();
            let mut input = Vec::with_capacity(self.input_batch_rows);
            while input.len() < self.input_batch_rows {
                check_interrupted(runtime.is_interrupted)?;
                let Some(row) = self.upstream.next(runtime)? else {
                    self.done = true;
                    break;
                };
                input.push(row);
            }
            if input.is_empty() {
                return Ok(None);
            }
            let result = super::super::completeness::execute::execute_read_clause(
                runtime.connection,
                runtime.snapshot.clone(),
                runtime.graph_view,
                runtime.params,
                &self.source,
                &self.clause,
                super::super::completeness::execute::RowSet {
                    columns: self.input_columns.clone(),
                    rows: input,
                    memory: None,
                },
                runtime.metrics,
                runtime.is_interrupted,
                runtime.control,
            )?;
            if result.columns != self.output_columns {
                return Err(QueryError::internal(format!(
                    "streamed clause columns {:?} differ from prepared columns {:?}",
                    result.columns, self.output_columns
                )));
            }
            self.buffered_memory = result.memory;
            self.buffered.extend(result.rows);
            runtime.expression_memory.resize(baseline)?;
        }
    }
}

#[derive(Debug)]
struct SeedSource {
    emitted: bool,
}

impl BindingSource for SeedSource {
    fn next(&mut self, _runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        if self.emitted {
            Ok(None)
        } else {
            self.emitted = true;
            Ok(Some(BindingRow::default()))
        }
    }
}

#[derive(Debug)]
struct RowsSource {
    rows: std::collections::VecDeque<BindingRow>,
    memory: MemoryReservation,
}

impl BindingSource for RowsSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        let Some(row) = self.rows.pop_front() else {
            return Ok(None);
        };
        let bytes = expression::binding_row_bytes_controlled(&row, Some(runtime.control))?;
        runtime.expression_memory.merge(self.memory.split(bytes)?)?;
        Ok(Some(row))
    }
}

#[derive(Debug)]
struct SpilledSource {
    connection: Connection,
    spill: BindingSpill,
    after: i64,
    buffered: std::collections::VecDeque<BindingRow>,
    memory: Option<MemoryReservation>,
    done: bool,
}

impl BindingSource for SpilledSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        if let Some(row) = self.buffered.pop_front() {
            return Ok(Some(row));
        }
        if self.done {
            return Ok(None);
        }
        self.memory = None;
        let (batch, memory) = self.spill.read_batch_reserved(
            &self.connection,
            self.after,
            BINDING_SPILL_BATCH_ROWS,
            runtime.control,
        )?;
        self.memory = Some(memory);
        if batch.is_empty() {
            self.done = true;
            return Ok(None);
        }
        self.after = batch.last().map_or(self.after, |(sequence, _)| *sequence);
        self.buffered.extend(batch.into_iter().map(|(_, row)| row));
        Ok(self.buffered.pop_front())
    }
}

#[derive(Debug)]
struct MatchSource {
    upstream: Box<dyn BindingSource>,
    step: MatchStep,
    current: Option<StepCursor>,
    _index_memory: MemoryReservation,
}

impl BindingSource for MatchSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        loop {
            check_interrupted(runtime.is_interrupted)?;
            if let Some(cursor) = self.current.as_mut() {
                if let Some(row) = cursor.next_row(
                    runtime.snapshot,
                    runtime.graph_view,
                    runtime.params,
                    runtime.metrics,
                    runtime.control,
                )? {
                    return Ok(Some(row));
                }
                self.current = None;
            }
            let Some(input) = self.upstream.next(runtime)? else {
                return Ok(None);
            };
            self.current = Some(StepCursor::new(self.step.clone(), input, runtime.control)?);
        }
    }
}

#[derive(Debug)]
struct FilterSource {
    upstream: Box<dyn BindingSource>,
    predicate: Expr,
}

impl BindingSource for FilterSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        loop {
            let baseline = runtime.expression_memory.bytes();
            check_interrupted(runtime.is_interrupted)?;
            let Some(row) = self.upstream.next(runtime)? else {
                return Ok(None);
            };
            let value = runtime.evaluate(&self.predicate, &row)?;
            if expression::predicate(value)? {
                return Ok(Some(row));
            }
            drop(row);
            runtime.expression_memory.resize(baseline)?;
        }
    }
}

#[derive(Debug)]
struct LetSource {
    upstream: Box<dyn BindingSource>,
    bindings: Vec<(String, Expr)>,
}

impl BindingSource for LetSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        check_interrupted(runtime.is_interrupted)?;
        let Some(source) = self.upstream.next(runtime)? else {
            return Ok(None);
        };
        let mut output = runtime.copy_binding(&source)?;
        for (name, expression) in &self.bindings {
            let value = runtime.evaluate(expression, &source)?;
            output.insert(
                name.clone(),
                expression::binding_from_value_controlled(
                    runtime.snapshot,
                    value,
                    runtime.control,
                )?,
            );
        }
        Ok(Some(output))
    }
}

#[derive(Debug)]
struct UnwindSource {
    upstream: Box<dyn BindingSource>,
    name: String,
    expression: Expr,
    current_input: Option<BindingRow>,
    current_values: std::vec::IntoIter<Value>,
    values_memory: Option<MemoryReservation>,
    input_memory: Option<MemoryReservation>,
}

impl BindingSource for UnwindSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        loop {
            check_interrupted(runtime.is_interrupted)?;
            if let Some(value) = self.current_values.next() {
                let input = self
                    .current_input
                    .as_ref()
                    .ok_or_else(|| QueryError::internal("UNWIND stream lost its input row"))?;
                let mut row = runtime.copy_binding(input)?;
                row.insert(
                    self.name.clone(),
                    expression::binding_from_value_controlled(
                        runtime.snapshot,
                        value,
                        runtime.control,
                    )?,
                );
                return Ok(Some(row));
            }
            let Some(input) = self.upstream.next(runtime)? else {
                self.current_input = None;
                self.values_memory = None;
                self.input_memory = None;
                return Ok(None);
            };
            self.values_memory = None;
            self.input_memory = Some(runtime.control.reserve(
                expression::binding_row_bytes_controlled(&input, Some(runtime.control))?,
            )?);
            let value = expression::evaluate_with_access_control(
                &self.expression,
                runtime.snapshot,
                &input,
                runtime.params,
                &runtime.graph_view.query_access,
                runtime.control,
            )?;
            let values = match value.value {
                Value::Null => Vec::new(),
                Value::List(values) => values,
                value => vec![value],
            };
            self.current_input = Some(input);
            self.current_values = values.into_iter();
            self.values_memory = Some(value.reservation);
        }
    }
}

struct LoadCsvSource {
    upstream: Box<dyn BindingSource>,
    spec: CompiledLoadCsv,
    current_input: Option<BindingRow>,
    current_stream: Option<CsvStream>,
    input_memory: Option<MemoryReservation>,
}

impl std::fmt::Debug for LoadCsvSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoadCsvSource")
            .field("binding", &self.spec.binding)
            .field("active", &self.current_stream.is_some())
            .finish_non_exhaustive()
    }
}

impl BindingSource for LoadCsvSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        loop {
            check_interrupted(runtime.is_interrupted)?;
            if let Some(stream) = self.current_stream.as_mut() {
                if let Some((value, line, memory)) = stream.next_value(runtime.control)? {
                    let input = self.current_input.as_ref().ok_or_else(|| {
                        QueryError::internal("LOAD CSV stream lost its input row")
                    })?;
                    let (row, memory) = csv_binding_row(
                        input,
                        &self.spec.binding,
                        value,
                        stream.file_path(),
                        line,
                        memory,
                        runtime.control,
                    )?;
                    runtime.expression_memory.merge(memory)?;
                    return Ok(Some(row));
                }
                self.current_stream = None;
                self.current_input = None;
                self.input_memory = None;
            }

            let baseline = runtime.expression_memory.bytes();
            let Some(input) = self.upstream.next(runtime)? else {
                return Ok(None);
            };
            self.input_memory = Some(runtime.control.reserve(
                expression::binding_row_bytes_controlled(&input, Some(runtime.control))?,
            )?);
            let (source, delimiter) =
                evaluate_csv_source(&self.spec, |expr| runtime.evaluate(expr, &input))?;
            self.current_stream = Some(CsvStream::open(
                &source,
                delimiter,
                self.spec.with_headers,
                runtime.control,
            )?);
            self.current_input = Some(input);
            drop(source);
            runtime.expression_memory.resize(baseline)?;
        }
    }
}

#[derive(Debug)]
struct ProjectionSource {
    upstream: Box<dyn BindingSource>,
    projections: Vec<Projection>,
    skip: usize,
    limit: Option<usize>,
    seen: usize,
    emitted: usize,
}

impl BindingSource for ProjectionSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        if self.limit.is_some_and(|limit| self.emitted >= limit) {
            return Ok(None);
        }
        loop {
            let baseline = runtime.expression_memory.bytes();
            check_interrupted(runtime.is_interrupted)?;
            let Some(input) = self.upstream.next(runtime)? else {
                return Ok(None);
            };
            let mut output = BindingRow::default();
            output.load_csv_context.clone_from(&input.load_csv_context);
            for projection in &self.projections {
                let value = runtime.evaluate(&projection.expression, &input)?;
                output.insert(
                    projection.column.clone(),
                    expression::binding_from_value_controlled(
                        runtime.snapshot,
                        value,
                        runtime.control,
                    )?,
                );
            }
            if self.seen < self.skip {
                self.seen = self.seen.saturating_add(1);
                drop(input);
                drop(output);
                runtime.expression_memory.resize(baseline)?;
                continue;
            }
            self.seen = self.seen.saturating_add(1);
            self.emitted = self.emitted.saturating_add(1);
            return Ok(Some(output));
        }
    }
}

#[derive(Debug)]
struct BarrierProjectionSource {
    upstream: Box<dyn BindingSource>,
    projections: Vec<Projection>,
    order: Vec<OrderItem>,
    distinct: bool,
    skip: usize,
    limit: Option<usize>,
    columns: Vec<String>,
    state: BarrierProjectionState,
}

#[derive(Debug)]
enum BarrierProjectionState {
    Pending,
    Ready {
        connection: Connection,
        output: SpillOutput,
        skipped: usize,
        emitted: usize,
    },
    Done,
}

impl BindingSource for BarrierProjectionSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        if matches!(self.state, BarrierProjectionState::Pending) {
            self.build(runtime)?;
        }
        loop {
            let BarrierProjectionState::Ready {
                connection,
                output,
                skipped,
                emitted,
            } = &mut self.state
            else {
                return Ok(None);
            };
            if self.limit.is_some_and(|limit| *emitted >= limit) {
                drop_temp_table(connection, &output.table)?;
                self.state = BarrierProjectionState::Done;
                return Ok(None);
            }
            let Some((row, _row_memory)) =
                next_program_spill_row(connection, output, runtime.control)?
            else {
                self.state = BarrierProjectionState::Done;
                return Ok(None);
            };
            if *skipped < self.skip {
                *skipped = skipped.saturating_add(1);
                continue;
            }
            *emitted = emitted.saturating_add(1);
            return projected_binding(runtime.snapshot, &self.columns, row, runtime.control)
                .map(Some);
        }
    }
}

impl BarrierProjectionSource {
    fn build(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<()> {
        let connection = open_spill_connection()?;
        let output = if self.order.is_empty() {
            self.build_distinct(&connection, runtime)?
        } else {
            self.build_sorted(&connection, runtime)?
        };
        self.state = BarrierProjectionState::Ready {
            connection,
            output,
            skipped: 0,
            emitted: 0,
        };
        Ok(())
    }

    fn build_distinct(
        &mut self,
        connection: &Connection,
        runtime: &mut ProgramRuntime<'_, '_>,
    ) -> QueryResult<SpillOutput> {
        let mut spill =
            DistinctSpill::create_with_resources(connection, runtime.control.resources().clone())?;
        while let Some(input) = self.upstream.next(runtime)? {
            check_interrupted(runtime.is_interrupted)?;
            let (row, _, _memory) = project_binding_values(
                runtime.snapshot,
                runtime.params,
                &input,
                &self.projections,
                &self.order,
                &runtime.graph_view.query_access,
                runtime.control,
            )?;
            spill.push(connection, &row, runtime.control)?;
            drop(input);
            runtime.expression_memory.resize(0)?;
        }
        Ok(spill.output())
    }

    fn build_sorted(
        &mut self,
        connection: &Connection,
        runtime: &mut ProgramRuntime<'_, '_>,
    ) -> QueryResult<SpillOutput> {
        let directions = self.order.iter().map(|item| item.descending).collect();
        let mut spill = SortSpill::create_with_resources(
            connection,
            self.distinct,
            directions,
            runtime.control.resources().clone(),
        )?;
        while let Some(input) = self.upstream.next(runtime)? {
            check_interrupted(runtime.is_interrupted)?;
            let (row, keys, _memory) = project_binding_values(
                runtime.snapshot,
                runtime.params,
                &input,
                &self.projections,
                &self.order,
                &runtime.graph_view.query_access,
                runtime.control,
            )?;
            spill.push(connection, row, keys, runtime.control)?;
            drop(input);
            runtime.expression_memory.resize(0)?;
        }
        let total = spill.finish(connection, runtime.control)?;
        spill.cleanup_aux(connection)?;
        Ok(spill.into_output(total))
    }
}

fn project_binding_values(
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    input: &BindingRow,
    projections: &[Projection],
    order: &[OrderItem],
    query_access: &crate::query::access::PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<Value>, Vec<Value>, MemoryReservation)> {
    let (row, mut memory) =
        projection_values(snapshot, params, input, projections, query_access, control)?;
    let alias_bytes = crate::query::resources::checked_add(
        crate::query::resources::row_bytes(&row)?,
        projections
            .iter()
            .map(|p| p.column.capacity() as u64 + 192)
            .sum(),
    )?;
    let _aliases_memory = control.reserve(alias_bytes)?;
    let mut aliases = std::collections::BTreeMap::new();
    for (projection, value) in projections.iter().zip(&row) {
        aliases.insert(
            projection.column.clone(),
            expression::clone_value_unreserved_controlled(value, Some(control))?,
        );
    }
    memory.grow(crate::query::resources::allocation_bytes::<Value>(
        order.len(),
    )?)?;
    let mut keys = Vec::with_capacity(order.len());
    for item in order {
        let evaluated = expression::evaluate_with_aliases_and_access_control(
            &item.expression,
            snapshot,
            input,
            params,
            &aliases,
            query_access,
            control,
        )?;
        memory.merge(evaluated.reservation)?;
        keys.push(evaluated.value);
    }
    Ok((row, keys, memory))
}

fn projection_values(
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    input: &BindingRow,
    projections: &[Projection],
    query_access: &crate::query::access::PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<Value>, MemoryReservation)> {
    let mut memory = control.reserve(crate::query::resources::allocation_bytes::<Value>(
        projections.len(),
    )?)?;
    let mut row = Vec::with_capacity(projections.len());
    for projection in projections {
        let evaluated = expression::evaluate_with_access_control(
            &projection.expression,
            snapshot,
            input,
            params,
            query_access,
            control,
        )?;
        memory.merge(evaluated.reservation)?;
        row.push(evaluated.value);
    }
    Ok((row, memory))
}

fn next_program_spill_row(
    connection: &Connection,
    output: &mut SpillOutput,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(Vec<Value>, MemoryReservation)>> {
    let Some((sequence, row, memory)) =
        read_output_row_with_resources(connection, &output.table, output.after, control)?
    else {
        drop_temp_table(connection, &output.table)?;
        return Ok(None);
    };
    output.after = sequence;
    Ok(Some((row, memory)))
}

fn materialized_binding_values(
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    columns: &[String],
    rules: &crate::query::access::PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<Value>, MemoryReservation)> {
    let mut memory = control.reserve(crate::query::resources::allocation_bytes::<Value>(
        columns.len(),
    )?)?;
    let mut values = Vec::with_capacity(columns.len());
    for column in columns {
        let value = expression::binding_value_with_memory(
            snapshot,
            row.values.get(column).unwrap_or(&BindingValue::Null),
            rules,
            control,
        )?;
        memory.merge(value.reservation)?;
        values.push(value.value);
    }
    Ok((values, memory))
}

fn projected_binding(
    snapshot: &Snapshot<'_>,
    columns: &[String],
    values: Vec<Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<BindingRow> {
    let mut row = BindingRow::default();
    for (column, value) in columns.iter().cloned().zip(values) {
        row.insert(
            column,
            expression::binding_from_value_controlled(snapshot, value, control)?,
        );
    }
    Ok(row)
}

impl ProgramStream {
    pub(crate) fn take_output_memory(&mut self) -> Option<MemoryReservation> {
        self.output_memory.take()
    }

    pub(crate) fn columns(&self) -> &[String] {
        &self.columns
    }

    pub(crate) fn next_binding(
        &mut self,
        connection: &Connection,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        let is_interrupted = || control.check().is_err();
        let mut runtime = ProgramRuntime {
            connection,
            snapshot,
            graph_view,
            params,
            metrics,
            is_interrupted: &is_interrupted,
            control,
            expression_memory: control.reserve(0)?,
        };
        let row = self.source.next(&mut runtime)?;
        let mut memory = runtime.expression_memory;
        memory.resize(
            row.as_ref()
                .map(|row| expression::binding_row_bytes_controlled(row, Some(control)))
                .transpose()?
                .unwrap_or(0),
        )?;
        self.output_memory = Some(memory);
        Ok(row)
    }

    pub(crate) fn next(
        &mut self,
        connection: &Connection,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<Vec<Value>>> {
        let Some(row) =
            self.next_binding(connection, snapshot, graph_view, params, metrics, control)?
        else {
            return Ok(None);
        };
        let (values, memory) = materialized_binding_values(
            snapshot,
            &row,
            &self.columns,
            &graph_view.query_access,
            control,
        )?;
        self.output_memory = Some(memory);
        Ok(Some(values))
    }
}

mod builder;
pub(super) use builder::build_program_stream;
pub(crate) use builder::{
    build_program_prefix_stream, build_program_spilled_suffix_stream, build_program_suffix_stream,
};
