use super::*;
use crate::cypher::{AstKind, AstNode, ClauseKind, ExpressionKind, QueryConnector, SubqueryKind};
use crate::query::LogicalOperator;
use crate::query::expression::Expr;
use crate::query::ingestion::{
    CompiledLoadCsv, CsvStream, compile_load_csv, csv_binding_row, evaluate_csv_source,
};
use crate::query::plan::{OrderItem, Projection, ProjectionPlan};
use crate::query::spill::{BINDING_SPILL_BATCH_ROWS, BindingSpill};

#[derive(Debug)]
pub(crate) struct ProgramStream {
    source: Box<dyn BindingSource>,
    columns: Vec<String>,
}

struct ProgramRuntime<'a, 'connection> {
    connection: &'connection Connection,
    snapshot: &'a Snapshot<'connection>,
    graph_view: &'a ResolvedGraphView,
    params: &'a std::collections::BTreeMap<String, Value>,
    metrics: &'a mut QueryMetrics,
    is_interrupted: &'a dyn Fn() -> bool,
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
            let mut spill = DistinctSpill::create(&connection)?;
            while let Some(binding) = self.upstream.next(runtime)? {
                check_interrupted(runtime.is_interrupted)?;
                let row = self
                    .columns
                    .iter()
                    .map(|column| {
                        expression::binding_value(
                            runtime.snapshot,
                            binding.values.get(column).unwrap_or(&BindingValue::Null),
                        )
                    })
                    .collect::<QueryResult<Vec<_>>>()?;
                spill.push(&connection, &row)?;
            }
            self.state = DistinctBindingState::Ready {
                connection,
                output: spill.output(),
            };
        }
        let DistinctBindingState::Ready { connection, output } = &mut self.state else {
            return Ok(None);
        };
        let Some((sequence, row)) = read_output_row(connection, &output.table, output.after)?
        else {
            drop_temp_table(connection, &output.table)?;
            self.state = DistinctBindingState::Done;
            return Ok(None);
        };
        output.after = sequence;
        projected_binding(runtime.snapshot, &self.columns, row).map(Some)
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
                },
                runtime.metrics,
                runtime.is_interrupted,
            )?;
            if result.columns != self.output_columns {
                return Err(QueryError::internal(format!(
                    "streamed clause columns {:?} differ from prepared columns {:?}",
                    result.columns, self.output_columns
                )));
            }
            self.buffered.extend(result.rows);
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
}

impl BindingSource for RowsSource {
    fn next(&mut self, _runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        Ok(self.rows.pop_front())
    }
}

#[derive(Debug)]
struct SpilledSource {
    connection: Connection,
    spill: BindingSpill,
    after: i64,
    buffered: std::collections::VecDeque<BindingRow>,
    done: bool,
}

impl BindingSource for SpilledSource {
    fn next(&mut self, _runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        if let Some(row) = self.buffered.pop_front() {
            return Ok(Some(row));
        }
        if self.done {
            return Ok(None);
        }
        let batch =
            self.spill
                .read_batch(&self.connection, self.after, BINDING_SPILL_BATCH_ROWS)?;
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
                )? {
                    return Ok(Some(row));
                }
                self.current = None;
            }
            let Some(input) = self.upstream.next(runtime)? else {
                return Ok(None);
            };
            self.current = Some(StepCursor::new(self.step.clone(), input));
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
            check_interrupted(runtime.is_interrupted)?;
            let Some(row) = self.upstream.next(runtime)? else {
                return Ok(None);
            };
            let value =
                expression::evaluate(&self.predicate, runtime.snapshot, &row, runtime.params)?;
            if expression::predicate(value)? {
                return Ok(Some(row));
            }
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
        let mut output = source.clone();
        for (name, expression) in &self.bindings {
            let value =
                expression::evaluate(expression, runtime.snapshot, &source, runtime.params)?;
            output.insert(
                name.clone(),
                expression::binding_from_value(runtime.snapshot, value)?,
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
}

impl BindingSource for UnwindSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        loop {
            check_interrupted(runtime.is_interrupted)?;
            if let Some(value) = self.current_values.next() {
                let mut row = self
                    .current_input
                    .as_ref()
                    .ok_or_else(|| QueryError::internal("UNWIND stream lost its input row"))?
                    .clone();
                row.insert(
                    self.name.clone(),
                    expression::binding_from_value(runtime.snapshot, value)?,
                );
                return Ok(Some(row));
            }
            let Some(input) = self.upstream.next(runtime)? else {
                self.current_input = None;
                return Ok(None);
            };
            let value =
                expression::evaluate(&self.expression, runtime.snapshot, &input, runtime.params)?;
            let values = match value {
                Value::Null => Vec::new(),
                Value::List(values) => values,
                value => vec![value],
            };
            self.current_input = Some(input);
            self.current_values = values.into_iter();
        }
    }
}

struct LoadCsvSource {
    upstream: Box<dyn BindingSource>,
    spec: CompiledLoadCsv,
    current_input: Option<BindingRow>,
    current_stream: Option<CsvStream>,
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
                if let Some((value, line)) = stream.next_value()? {
                    let input = self.current_input.as_ref().ok_or_else(|| {
                        QueryError::internal("LOAD CSV stream lost its input row")
                    })?;
                    return Ok(Some(csv_binding_row(
                        input,
                        &self.spec.binding,
                        value,
                        stream.file_path(),
                        line,
                    )));
                }
                self.current_stream = None;
                self.current_input = None;
            }

            let Some(input) = self.upstream.next(runtime)? else {
                return Ok(None);
            };
            let (source, delimiter) = evaluate_csv_source(&self.spec, |expr| {
                expression::evaluate(expr, runtime.snapshot, &input, runtime.params)
            })?;
            self.current_stream =
                Some(CsvStream::open(&source, delimiter, self.spec.with_headers)?);
            self.current_input = Some(input);
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
            check_interrupted(runtime.is_interrupted)?;
            let Some(input) = self.upstream.next(runtime)? else {
                return Ok(None);
            };
            let mut output = BindingRow::default();
            output.load_csv_context.clone_from(&input.load_csv_context);
            for projection in &self.projections {
                let value = expression::evaluate(
                    &projection.expression,
                    runtime.snapshot,
                    &input,
                    runtime.params,
                )?;
                output.insert(
                    projection.column.clone(),
                    expression::binding_from_value(runtime.snapshot, value)?,
                );
            }
            if self.seen < self.skip {
                self.seen = self.seen.saturating_add(1);
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
            let Some((sequence, row)) = read_output_row(connection, &output.table, output.after)?
            else {
                drop_temp_table(connection, &output.table)?;
                self.state = BarrierProjectionState::Done;
                return Ok(None);
            };
            output.after = sequence;
            if *skipped < self.skip {
                *skipped = skipped.saturating_add(1);
                continue;
            }
            *emitted = emitted.saturating_add(1);
            return projected_binding(runtime.snapshot, &self.columns, row).map(Some);
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
        let mut spill = DistinctSpill::create(connection)?;
        while let Some(input) = self.upstream.next(runtime)? {
            check_interrupted(runtime.is_interrupted)?;
            let (row, _) = project_binding_values(
                runtime.snapshot,
                runtime.params,
                &input,
                &self.projections,
                &self.order,
            )?;
            spill.push(connection, &row)?;
        }
        Ok(spill.output())
    }

    fn build_sorted(
        &mut self,
        connection: &Connection,
        runtime: &mut ProgramRuntime<'_, '_>,
    ) -> QueryResult<SpillOutput> {
        let directions = self.order.iter().map(|item| item.descending).collect();
        let mut spill = SortSpill::create(connection, self.distinct, directions)?;
        while let Some(input) = self.upstream.next(runtime)? {
            check_interrupted(runtime.is_interrupted)?;
            let (row, keys) = project_binding_values(
                runtime.snapshot,
                runtime.params,
                &input,
                &self.projections,
                &self.order,
            )?;
            spill.push(connection, row, keys)?;
        }
        let total = spill.finish(connection, runtime.is_interrupted)?;
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
) -> QueryResult<(Vec<Value>, Vec<Value>)> {
    let row = projections
        .iter()
        .map(|projection| expression::evaluate(&projection.expression, snapshot, input, params))
        .collect::<QueryResult<Vec<_>>>()?;
    let aliases = projections
        .iter()
        .map(|projection| projection.column.clone())
        .zip(row.iter().cloned())
        .collect::<std::collections::BTreeMap<_, _>>();
    let keys = order
        .iter()
        .map(|item| {
            expression::evaluate_with_aliases(&item.expression, snapshot, input, params, &aliases)
        })
        .collect::<QueryResult<Vec<_>>>()?;
    Ok((row, keys))
}

fn projected_binding(
    snapshot: &Snapshot<'_>,
    columns: &[String],
    values: Vec<Value>,
) -> QueryResult<BindingRow> {
    let mut row = BindingRow::default();
    for (column, value) in columns.iter().cloned().zip(values) {
        row.insert(column, expression::binding_from_value(snapshot, value)?);
    }
    Ok(row)
}

impl ProgramStream {
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
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<Option<BindingRow>> {
        let mut runtime = ProgramRuntime {
            connection,
            snapshot,
            graph_view,
            params,
            metrics,
            is_interrupted,
        };
        self.source.next(&mut runtime)
    }

    pub(crate) fn next(
        &mut self,
        connection: &Connection,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<Option<Vec<Value>>> {
        let Some(row) = self.next_binding(
            connection,
            snapshot,
            graph_view,
            params,
            metrics,
            is_interrupted,
        )?
        else {
            return Ok(None);
        };
        self.columns
            .iter()
            .map(|column| {
                expression::binding_value(
                    snapshot,
                    row.values.get(column).unwrap_or(&BindingValue::Null),
                )
            })
            .collect::<QueryResult<Vec<_>>>()
            .map(Some)
    }
}

pub(super) fn build_program_stream(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    mode: ExecutionMode,
    params: &std::collections::BTreeMap<String, Value>,
) -> QueryResult<Option<ProgramStream>> {
    if mode == ExecutionMode::Profile
        || !program.public_result
        || program.writes
        || program.version_operation
        || program.transaction_options.is_some()
        || has_nonstreamable_operator(program)
    {
        return Ok(None);
    }

    let query = super::super::completeness::executable_query(&program.root, true)?;
    let stream = build_query_stream(connection, program, snapshot, params, query)?;
    let Some(stream) = stream else {
        return Ok(None);
    };
    if stream.columns != program.columns {
        return Ok(None);
    }
    Ok(Some(stream))
}

fn build_query_stream(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    query: &AstNode,
) -> QueryResult<Option<ProgramStream>> {
    match query.kind {
        AstKind::SingleQuery => build_clause_stream(
            connection,
            program,
            snapshot,
            params,
            Box::new(SeedSource { emitted: false }),
            Vec::new(),
            &query.children,
        ),
        AstKind::ComposedQuery => {
            build_composed_union_stream(connection, program, snapshot, params, query)
        }
        AstKind::Subquery(SubqueryKind::Braced) => {
            let body = super::super::completeness::required_query_body(
                query,
                "braced query is missing its body",
            )?;
            let nested = super::super::completeness::executable_query(body, true)?;
            build_query_stream(connection, program, snapshot, params, nested)
        }
        _ => Ok(None),
    }
}

fn build_query_stream_from_input(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    query: &AstNode,
    source: Box<dyn BindingSource>,
    columns: Vec<String>,
) -> QueryResult<Option<ProgramStream>> {
    match query.kind {
        AstKind::SingleQuery => build_clause_stream(
            connection,
            program,
            snapshot,
            params,
            source,
            columns,
            &query.children,
        ),
        AstKind::Subquery(SubqueryKind::Braced) => {
            let body = super::super::completeness::required_query_body(
                query,
                "braced query is missing its body",
            )?;
            let nested = super::super::completeness::executable_query(body, true)?;
            build_query_stream_from_input(
                connection, program, snapshot, params, nested, source, columns,
            )
        }
        _ => Ok(None),
    }
}

fn build_composed_union_stream(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    query: &AstNode,
) -> QueryResult<Option<ProgramStream>> {
    let parts = super::super::completeness::composed_query_parts(query)?;
    let has_next = parts
        .rest
        .iter()
        .any(|(connector, _)| *connector == QueryConnector::Next);
    if has_next {
        if parts
            .rest
            .iter()
            .any(|(connector, _)| *connector != QueryConnector::Next)
        {
            return Ok(None);
        }
        let Some(mut current) =
            build_query_stream(connection, program, snapshot, params, parts.first)?
        else {
            return Ok(None);
        };
        let mut previous = parts.first;
        for (_, operand) in parts.rest {
            let ProgramStream { source, columns } = current;
            let (source, columns) = if crate::cypher::query_body_returns_columns(previous) {
                (source, columns)
            } else if crate::cypher::query_body_ends_with_call(previous) {
                (
                    Box::new(UnitMapSource { upstream: source }) as Box<dyn BindingSource>,
                    Vec::new(),
                )
            } else {
                return Ok(None);
            };
            let Some(next) = build_query_stream_from_input(
                connection, program, snapshot, params, operand, source, columns,
            )?
            else {
                return Ok(None);
            };
            current = next;
            previous = operand;
        }
        return Ok(Some(current));
    }
    let Some(first) = build_query_stream(connection, program, snapshot, params, parts.first)?
    else {
        return Ok(None);
    };
    let ProgramStream {
        mut source,
        columns,
    } = first;
    for (connector, operand) in parts.rest {
        let Some(right) = build_query_stream(connection, program, snapshot, params, operand)?
        else {
            return Ok(None);
        };
        if right.columns != columns {
            return Err(QueryError::internal(format!(
                "UNION stream columns {:?} differ from {:?}",
                right.columns, columns
            )));
        }
        source = Box::new(ConcatSource {
            left: source,
            right: right.source,
            left_done: false,
        });
        if connector != QueryConnector::UnionAll {
            source = Box::new(DistinctBindingSource {
                upstream: source,
                columns: columns.clone(),
                state: DistinctBindingState::Pending,
            });
        }
    }
    Ok(Some(ProgramStream { source, columns }))
}

pub(crate) fn build_program_prefix_stream(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    clauses: &[AstNode],
) -> QueryResult<Option<ProgramStream>> {
    build_clause_stream(
        connection,
        program,
        snapshot,
        params,
        Box::new(SeedSource { emitted: false }),
        Vec::new(),
        clauses,
    )
}

pub(crate) fn build_program_suffix_stream(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    input_columns: Vec<String>,
    input_rows: Vec<BindingRow>,
    clauses: &[AstNode],
) -> QueryResult<Option<ProgramStream>> {
    build_clause_stream(
        connection,
        program,
        snapshot,
        params,
        Box::new(RowsSource {
            rows: input_rows.into(),
        }),
        input_columns,
        clauses,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the spilled suffix builder keeps execution state explicit at one internal boundary"
)]
pub(crate) fn build_program_spilled_suffix_stream(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    input_columns: Vec<String>,
    spill_connection: Connection,
    spill: BindingSpill,
    clauses: &[AstNode],
) -> QueryResult<Option<ProgramStream>> {
    build_clause_stream(
        connection,
        program,
        snapshot,
        params,
        Box::new(SpilledSource {
            connection: spill_connection,
            spill,
            after: -1,
            buffered: std::collections::VecDeque::new(),
            done: false,
        }),
        input_columns,
        clauses,
    )
}

fn build_clause_stream(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    mut source: Box<dyn BindingSource>,
    mut columns: Vec<String>,
    clauses: &[AstNode],
) -> QueryResult<Option<ProgramStream>> {
    let builder = ClauseStreamBuilder {
        connection,
        program,
        snapshot,
        params,
    };
    for clause in clauses {
        let AstKind::Clause(kind) = clause.kind else {
            continue;
        };
        let next_columns =
            super::super::completeness::infer_clause_columns(clause, &columns, &program.source)?;
        source = match builder.apply(source, &columns, &next_columns, clause, kind)? {
            Some(source) => source,
            None => return Ok(None),
        };
        columns = next_columns;
    }
    Ok(Some(ProgramStream { source, columns }))
}

struct ClauseStreamBuilder<'a, 'snapshot> {
    connection: &'a Connection,
    program: &'a PreparedProgram,
    snapshot: &'a Snapshot<'snapshot>,
    params: &'a std::collections::BTreeMap<String, Value>,
}

impl ClauseStreamBuilder<'_, '_> {
    fn apply(
        &self,
        source: Box<dyn BindingSource>,
        columns: &[String],
        next_columns: &[String],
        clause: &AstNode,
        kind: ClauseKind,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        if matches!(
            kind,
            ClauseKind::Filter | ClauseKind::Let | ClauseKind::Unwind | ClauseKind::For
        ) && clause_requires_graph_executor(clause)
        {
            return Ok(Some(self.batch_source(
                source,
                clause,
                columns,
                next_columns,
            )));
        }
        match kind {
            ClauseKind::Match | ClauseKind::OptionalMatch => {
                self.match_source(source, columns, next_columns, clause, kind)
            }
            ClauseKind::Filter => self.filter_source(source, clause),
            ClauseKind::Let => self.let_source(source, clause),
            ClauseKind::Unwind | ClauseKind::For => self.unwind_source(source, clause),
            ClauseKind::LoadCsv => Ok(Some(Box::new(LoadCsvSource {
                upstream: source,
                spec: compile_load_csv(clause)?,
                current_input: None,
                current_stream: None,
            }))),
            ClauseKind::Call if call_clause_can_batch_stream(clause) => Ok(Some(
                self.batch_source(source, clause, columns, next_columns),
            )),
            ClauseKind::Show => Ok(Some(self.batch_source(
                source,
                clause,
                columns,
                next_columns,
            ))),
            ClauseKind::With | ClauseKind::Return => {
                self.projection_source(source, columns, next_columns, clause, kind)
            }
            _ => Ok(None),
        }
    }

    fn match_source(
        &self,
        source: Box<dyn BindingSource>,
        columns: &[String],
        next_columns: &[String],
        clause: &AstNode,
        kind: ClauseKind,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        if match_requires_complex_executor(clause) {
            return Ok(Some(Box::new(BatchClauseSource {
                upstream: source,
                clause: clause.clone(),
                source: self.program.source.clone(),
                input_columns: columns.to_vec(),
                output_columns: next_columns.to_vec(),
                buffered: std::collections::VecDeque::new(),
                done: false,
                input_batch_rows: 1,
            })));
        }
        let Ok(mut step) = super::super::plan::lower_match(
            self.connection,
            clause,
            kind == ClauseKind::OptionalMatch,
        ) else {
            return Ok(None);
        };
        if let Ok(schema) = self.snapshot.schema_state() {
            super::super::schema::select_standard_index_seeks(
                &schema,
                std::slice::from_mut(&mut step),
                self.params,
            );
        }
        Ok(Some(Box::new(MatchSource {
            upstream: source,
            step,
            current: None,
        })))
    }

    fn filter_source(
        &self,
        source: Box<dyn BindingSource>,
        clause: &AstNode,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        Ok(Some(Box::new(FilterSource {
            upstream: source,
            predicate: first_expression(clause, "FILTER is missing its predicate")?,
        })))
    }

    fn let_source(
        &self,
        source: Box<dyn BindingSource>,
        clause: &AstNode,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        Ok(Some(Box::new(LetSource {
            upstream: source,
            bindings: compile_let_bindings(clause)?,
        })))
    }

    fn unwind_source(
        &self,
        source: Box<dyn BindingSource>,
        clause: &AstNode,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        let name = clause
            .descendants()
            .find(|node| node.kind == AstKind::BindingVariable)
            .and_then(|node| node.text.clone())
            .ok_or_else(|| QueryError::semantic("UNWIND/FOR is missing its variable"))?;
        let expression = first_expression(clause, "UNWIND/FOR is missing its list expression")?;
        Ok(Some(Box::new(UnwindSource {
            upstream: source,
            name,
            expression,
            current_input: None,
            current_values: Vec::new().into_iter(),
        })))
    }

    fn projection_source(
        &self,
        source: Box<dyn BindingSource>,
        columns: &[String],
        next_columns: &[String],
        clause: &AstNode,
        kind: ClauseKind,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        if projection_requires_global_executor(clause) {
            return Ok(None);
        }
        if kind == ClauseKind::With && clause.descendants().any(|node| node.kind == AstKind::Where)
        {
            if projection_has_global_modifier(clause) {
                return Ok(None);
            }
            return Ok(Some(self.batch_source(
                source,
                clause,
                columns,
                next_columns,
            )));
        }
        if projection_requires_batch_executor(clause) {
            return Ok(Some(self.batch_source(
                source,
                clause,
                columns,
                next_columns,
            )));
        }
        let Ok(plan) =
            super::super::plan::lower_projection(clause, &self.program.source, self.params)
        else {
            return Ok(None);
        };
        let mut source = self.lower_projection_source(source, next_columns, plan);
        if kind == ClauseKind::With
            && let Some(where_node) = clause
                .descendants()
                .find(|node| node.kind == AstKind::Where)
        {
            source = Box::new(FilterSource {
                upstream: source,
                predicate: first_expression(where_node, "WITH WHERE is missing its predicate")?,
            });
        }
        Ok(Some(source))
    }

    fn lower_projection_source(
        &self,
        source: Box<dyn BindingSource>,
        next_columns: &[String],
        plan: ProjectionPlan,
    ) -> Box<dyn BindingSource> {
        let ProjectionPlan {
            projections,
            order,
            skip,
            limit,
            distinct,
        } = plan;
        if order.is_empty() && !distinct {
            return Box::new(ProjectionSource {
                upstream: source,
                projections,
                skip,
                limit,
                seen: 0,
                emitted: 0,
            });
        }
        Box::new(BarrierProjectionSource {
            upstream: source,
            projections,
            order,
            distinct,
            skip,
            limit,
            columns: next_columns.to_vec(),
            state: BarrierProjectionState::Pending,
        })
    }

    fn batch_source(
        &self,
        source: Box<dyn BindingSource>,
        clause: &AstNode,
        columns: &[String],
        next_columns: &[String],
    ) -> Box<dyn BindingSource> {
        batch_clause_source(source, clause, self.program, columns, next_columns)
    }
}

fn has_nonstreamable_operator(program: &PreparedProgram) -> bool {
    program.logical.operators.iter().any(|operator| {
        matches!(
            operator,
            LogicalOperator::Aggregate
                | LogicalOperator::When
                | LogicalOperator::Eager
                | LogicalOperator::Mutation { .. }
                | LogicalOperator::Schema { .. }
                | LogicalOperator::Commit
        )
    })
}

fn match_requires_complex_executor(clause: &AstNode) -> bool {
    clause_requires_graph_executor(clause)
        || clause.descendants().any(|node| {
            matches!(
                node.kind,
                AstKind::Search
                    | AstKind::MatchMode(_)
                    | AstKind::PathMode(_)
                    | AstKind::PathSelector(_)
                    | AstKind::Quantifier(_)
                    | AstKind::VariableLength
            )
        })
}

fn projection_requires_global_executor(clause: &AstNode) -> bool {
    clause.descendants().any(|node| {
        node.kind == AstKind::GroupBy
            || (node.kind == AstKind::FunctionName
                && node
                    .text
                    .as_deref()
                    .is_some_and(super::super::registry::is_aggregating))
    }) || (projection_requires_batch_executor(clause) && projection_has_global_modifier(clause))
}

fn projection_requires_batch_executor(clause: &AstNode) -> bool {
    clause_requires_graph_executor(clause)
        || clause
            .descendants()
            .any(|node| node.kind == AstKind::StarProjection)
}

fn projection_has_global_modifier(clause: &AstNode) -> bool {
    clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::OrderBy
                | AstKind::Skip
                | AstKind::Limit
                | AstKind::SetQuantifier(crate::cypher::SetQuantifierKind::Distinct)
        )
    })
}

fn clause_contains_subquery(clause: &AstNode) -> bool {
    clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::Subquery(_) | AstKind::Expression(ExpressionKind::Subquery)
        )
    })
}

fn clause_requires_graph_executor(clause: &AstNode) -> bool {
    clause_contains_subquery(clause) || has_graph_expression(clause, false)
}

fn has_graph_expression(node: &AstNode, expression_context: bool) -> bool {
    if expression_context && node.kind == AstKind::Pattern {
        return true;
    }
    let expression_context = expression_context || matches!(node.kind, AstKind::Expression(_));
    node.children
        .iter()
        .any(|child| has_graph_expression(child, expression_context))
}

fn batch_clause_source(
    upstream: Box<dyn BindingSource>,
    clause: &AstNode,
    program: &PreparedProgram,
    input_columns: &[String],
    output_columns: &[String],
) -> Box<dyn BindingSource> {
    Box::new(BatchClauseSource {
        upstream,
        clause: clause.clone(),
        source: program.source.clone(),
        input_columns: input_columns.to_vec(),
        output_columns: output_columns.to_vec(),
        buffered: std::collections::VecDeque::new(),
        done: false,
        input_batch_rows: 1,
    })
}

fn call_clause_can_batch_stream(clause: &AstNode) -> bool {
    if clause_contains_subquery(clause) {
        return true;
    }
    let Some(name) = clause
        .descendants()
        .find(|node| node.kind == AstKind::FunctionName)
        .and_then(|node| node.text.as_deref())
    else {
        return false;
    };
    !super::super::registry::is_version_procedure(name)
        && !super::super::registry::is_semantic_maintenance(name)
}

fn first_expression(node: &AstNode, message: &str) -> QueryResult<Expr> {
    let expression = expression::surface_expressions(node)
        .into_iter()
        .next()
        .ok_or_else(|| QueryError::semantic(message))?;
    expression::compile_expression(expression)
}

fn compile_let_bindings(clause: &AstNode) -> QueryResult<Vec<(String, Expr)>> {
    let mut bindings = clause
        .descendants()
        .filter(|node| node.kind == AstKind::LetBinding)
        .collect::<Vec<_>>();
    bindings.sort_by_key(|node| node.span.start);
    bindings
        .into_iter()
        .map(|binding| {
            let name = binding
                .descendants()
                .find(|node| node.kind == AstKind::BindingVariable)
                .and_then(|node| node.text.clone())
                .ok_or_else(|| QueryError::semantic("LET binding is missing its variable"))?;
            Ok((
                name,
                first_expression(binding, "LET binding is missing its expression")?,
            ))
        })
        .collect()
}
