use super::*;
use crate::query::spill::{RowSpill, SpillOutput, drop_temp_table};
use crate::query::stream::program;

struct StreamExecution<'a, 'control> {
    connection: &'a Connection,
    program: &'a PreparedProgram,
    params: &'a BTreeMap<String, Value>,
    metrics: &'a mut QueryMetrics,
    is_interrupted: &'a dyn Fn() -> bool,
    control: &'a ExecutionControl<'control>,
}

impl<'a> StreamExecution<'a, '_> {
    fn runtime<'scope>(
        &'scope mut self,
        owner: &TransactionProgramStream,
    ) -> QueryResult<TransactionRuntime<'scope>> {
        Ok(TransactionRuntime {
            connection: self.connection,
            program: self.program,
            options: transaction_options(self.program)?,
            params: self.params,
            metrics: self.metrics,
            resources: self.control.resources(),
            is_interrupted: self.is_interrupted,
            commit: owner.commit,
            counters: owner.counters.clone(),
            next_batch_id: owner.next_batch_id,
        })
    }

    fn graph(&self, commit: HashId) -> QueryResult<(Snapshot<'a>, ResolvedGraphView)> {
        let snapshot = crate::query::graph::resolve_snapshot_with_control(
            self.connection,
            commit,
            self.control,
        )?;
        let options = transaction_options(self.program)?;
        let graph_view = ResolvedGraphView::resolve(self.connection, &options.graph_view)?
            .with_query_access(options.access.read.query.clone());
        Ok((snapshot, graph_view))
    }
}

#[derive(Debug)]
pub(crate) struct TransactionProgramStream {
    prefix: program::ProgramStream,
    prefix_commit: HashId,
    prefix_graph_view: ResolvedGraphView,
    subquery: AstNode,
    spec: TransactionSpec,
    suffix: Vec<AstNode>,
    input_columns: Vec<String>,
    call_columns: Vec<String>,
    batch_size: Option<usize>,
    concurrency_validated: bool,
    pending_input: Vec<BindingRow>,
    pending_input_memory: Option<crate::query::resources::MemoryReservation>,
    output_memory: Option<crate::query::resources::MemoryReservation>,
    current_output: Option<(Connection, SpillOutput)>,
    deferred_bindings: Option<(Connection, BindingSpill)>,
    final_stream: Option<program::ProgramStream>,
    commit: HashId,
    counters: QueryCounters,
    next_batch_id: u64,
    upstream_done: bool,
    broken: bool,
    finished: bool,
    output_policy: TransactionOutputPolicy,
}

#[derive(Debug)]
pub(crate) struct TransactionStreamBatch {
    pub(crate) rows: Vec<Vec<Value>>,
    pub(crate) memory: Option<crate::query::resources::MemoryReservation>,
    pub(crate) done: bool,
    pub(crate) commit: HashId,
    pub(crate) counters: QueryCounters,
}

struct TransactionStreamShape {
    transaction_clause: AstNode,
    subquery: AstNode,
    spec: TransactionSpec,
    prefix_clauses: Vec<AstNode>,
    suffix: Vec<AstNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionOutputPolicy {
    Immediate,
    DeferredRead,
    OuterWrite { write_index: usize },
    Discard,
}

fn transaction_stream_shape(
    program: &PreparedProgram,
) -> QueryResult<Option<TransactionStreamShape>> {
    let query = executable_query(&program.root, true)?;
    let single = match query.kind {
        AstKind::SingleQuery => query,
        AstKind::ComposedQuery => {
            let parts = composed_query_parts(query)?;
            if !parts.rest.is_empty() || parts.first.kind != AstKind::SingleQuery {
                return Ok(None);
            }
            parts.first
        }
        _ => return Ok(None),
    };
    let transaction_indices = single
        .children
        .iter()
        .enumerate()
        .filter_map(|(index, clause)| {
            (clause.kind == AstKind::Clause(ClauseKind::Call)
                && transaction_subquery(clause).is_some())
            .then_some(index)
        })
        .collect::<Vec<_>>();
    if transaction_indices.len() != 1 {
        return Ok(None);
    }
    let transaction_index = transaction_indices[0];
    let transaction_clause = single.children[transaction_index].clone();
    let subquery = transaction_subquery(&transaction_clause)
        .ok_or_else(|| QueryError::internal("transaction CALL is missing its subquery"))?
        .clone();
    let spec = transaction_spec(&subquery)?;
    Ok(Some(TransactionStreamShape {
        transaction_clause,
        subquery,
        spec,
        prefix_clauses: single.children[..transaction_index].to_vec(),
        suffix: single.children[transaction_index + 1..].to_vec(),
    }))
}

fn transaction_stream_output_policy(
    connection: &Connection,
    program: &PreparedProgram,
    initial_commit: HashId,
    params: &BTreeMap<String, Value>,
    call_columns: &[String],
    shape: &TransactionStreamShape,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<TransactionOutputPolicy>> {
    let suffix = &shape.suffix;
    if let Some(write_index) = suffix.iter().position(is_basic_outer_write_clause) {
        if program.public_result
            || !outer_write_suffix_supported(&suffix[write_index..])
            || !suffix_prefix_streamable(
                connection,
                program,
                initial_commit,
                params,
                call_columns,
                &suffix[..write_index],
                control,
            )?
        {
            return Ok(None);
        }
        return Ok(Some(TransactionOutputPolicy::OuterWrite { write_index }));
    }
    if !program.public_result {
        return Ok(discard_output_policy(suffix));
    }
    if suffix.is_empty() && call_columns != program.columns {
        return Ok(None);
    }
    if !suffix.is_empty()
        && !suffix_stream_matches_program(
            connection,
            program,
            initial_commit,
            params,
            call_columns,
            suffix,
            control,
        )?
    {
        return Ok(None);
    }
    let defer_output = !incremental_suffix_is_safe(suffix)
        || transaction_result_requires_final_snapshot(&shape.subquery, suffix);
    Ok(Some(if defer_output {
        TransactionOutputPolicy::DeferredRead
    } else {
        TransactionOutputPolicy::Immediate
    }))
}

fn discard_output_policy(suffix: &[AstNode]) -> Option<TransactionOutputPolicy> {
    (suffix.is_empty()
        || suffix
            .iter()
            .all(|clause| clause.kind == AstKind::Clause(ClauseKind::Finish)))
    .then_some(TransactionOutputPolicy::Discard)
}

fn is_basic_outer_write_clause(clause: &AstNode) -> bool {
    matches!(
        clause.kind,
        AstKind::Clause(
            ClauseKind::Create
                | ClauseKind::Insert
                | ClauseKind::Set
                | ClauseKind::Remove
                | ClauseKind::Merge
        )
    )
}

fn outer_write_suffix_supported(suffix: &[AstNode]) -> bool {
    suffix.iter().all(|clause| {
        matches!(
            clause.kind,
            AstKind::Clause(
                ClauseKind::Create
                    | ClauseKind::Insert
                    | ClauseKind::Set
                    | ClauseKind::Remove
                    | ClauseKind::Merge
                    | ClauseKind::Finish
            )
        )
    })
}

fn suffix_prefix_streamable(
    connection: &Connection,
    program: &PreparedProgram,
    initial_commit: HashId,
    params: &BTreeMap<String, Value>,
    call_columns: &[String],
    prefix: &[AstNode],
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    if prefix.is_empty() {
        return Ok(true);
    }
    let snapshot =
        crate::query::graph::resolve_snapshot_with_control(connection, initial_commit, control)?;
    Ok(program::build_program_suffix_stream(
        connection,
        program,
        &snapshot,
        params,
        RowSet {
            columns: call_columns.to_vec(),
            rows: Vec::new(),
            memory: None,
        },
        prefix,
        control,
    )?
    .is_some())
}

fn suffix_stream_matches_program(
    connection: &Connection,
    program: &PreparedProgram,
    initial_commit: HashId,
    params: &BTreeMap<String, Value>,
    call_columns: &[String],
    suffix: &[AstNode],
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let snapshot =
        crate::query::graph::resolve_snapshot_with_control(connection, initial_commit, control)?;
    let Some(stream) = program::build_program_suffix_stream(
        connection,
        program,
        &snapshot,
        params,
        RowSet {
            columns: call_columns.to_vec(),
            rows: Vec::new(),
            memory: None,
        },
        suffix,
        control,
    )?
    else {
        return Ok(false);
    };
    Ok(stream.columns() == program.columns)
}

impl TransactionProgramStream {
    pub(crate) fn try_new(
        connection: &Connection,
        program: &PreparedProgram,
        initial_commit: HashId,
        params: &BTreeMap<String, Value>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<Self>> {
        if !connection.is_autocommit() {
            return Err(QueryError::transaction_boundary_required(
                "CALL subqueries IN TRANSACTIONS require SQLite autocommit execution",
            ));
        }
        let options = program.transaction_options.as_ref().ok_or_else(|| {
            QueryError::internal("transaction program is missing transaction options")
        })?;
        let Some(shape) = transaction_stream_shape(program)? else {
            return Ok(None);
        };

        let prefix_snapshot = crate::query::graph::resolve_snapshot_with_control(
            connection,
            initial_commit,
            control,
        )?;
        let prefix_graph_view = ResolvedGraphView::resolve(connection, &options.graph_view)?
            .with_query_access(options.access.read.query.clone());
        let Some(prefix) = program::build_program_prefix_stream(
            connection,
            program,
            &prefix_snapshot,
            params,
            &shape.prefix_clauses,
            control,
        )?
        else {
            return Ok(None);
        };
        let input_columns = prefix.columns().to_vec();
        let call_columns =
            call_output_columns(&shape.transaction_clause, &input_columns, &program.source)?;
        let Some(output_policy) = transaction_stream_output_policy(
            connection,
            program,
            initial_commit,
            params,
            &call_columns,
            &shape,
            control,
        )?
        else {
            return Ok(None);
        };

        Ok(Some(Self {
            prefix,
            prefix_commit: initial_commit,
            prefix_graph_view,
            subquery: shape.subquery,
            spec: shape.spec,
            suffix: shape.suffix,
            input_columns,
            call_columns,
            batch_size: None,
            concurrency_validated: false,
            pending_input: Vec::new(),
            pending_input_memory: None,
            output_memory: None,
            current_output: None,
            deferred_bindings: None,
            final_stream: None,
            commit: initial_commit,
            counters: QueryCounters::default(),
            next_batch_id: 1,
            upstream_done: false,
            broken: false,
            finished: false,
            output_policy,
        }))
    }

    pub(crate) fn next_batch(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        max_rows: usize,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<TransactionStreamBatch> {
        let is_interrupted = || control.check().is_err();
        let mut execution = StreamExecution {
            connection,
            program,
            params,
            metrics,
            is_interrupted: &is_interrupted,
            control,
        };
        self.next_batch_in(max_rows, &mut execution)
    }

    fn next_batch_in(
        &mut self,
        max_rows: usize,
        execution: &mut StreamExecution<'_, '_>,
    ) -> QueryResult<TransactionStreamBatch> {
        let max_rows = max_rows.clamp(1, 4_096);
        loop {
            let rows = self.drain_output(max_rows, execution.control)?;
            if !rows.is_empty() {
                return Ok(self.batch(rows, false));
            }
            let (rows, final_done) = self.drain_final_stream(max_rows, execution)?;
            if !rows.is_empty() || final_done {
                return Ok(self.batch(rows, final_done));
            }
            if self.finished {
                return Ok(self.batch(Vec::new(), true));
            }
            check_interrupted(execution.is_interrupted)?;
            if self.broken {
                self.produce_unstarted_rows(execution)?;
            } else {
                self.execute_next_batch(execution)?;
            }
        }
    }

    pub(crate) fn cancel(&mut self) -> QueryResult<()> {
        if let Some((connection, output)) = self.current_output.take() {
            drop_temp_table(&connection, &output.table)?;
        }
        if let Some((connection, spill)) = self.deferred_bindings.take() {
            spill.abort(&connection)?;
        }
        self.final_stream = None;
        self.pending_input.clear();
        self.pending_input_memory = None;
        self.output_memory = None;
        self.finished = true;
        Ok(())
    }

    fn batch(&mut self, rows: Vec<Vec<Value>>, done: bool) -> TransactionStreamBatch {
        TransactionStreamBatch {
            rows,
            memory: self.output_memory.take(),
            done,
            commit: self.commit,
            counters: self.counters.clone(),
        }
    }

    fn pull_prefix_row(
        &mut self,
        execution: &mut StreamExecution<'_, '_>,
    ) -> QueryResult<Option<BindingRow>> {
        if self.upstream_done {
            return Ok(None);
        }
        let snapshot = crate::query::graph::resolve_snapshot_with_control(
            execution.connection,
            self.prefix_commit,
            execution.control,
        )?;
        let next = self.prefix.next_binding(
            execution.connection,
            &snapshot,
            &self.prefix_graph_view,
            execution.params,
            execution.metrics,
            execution.control,
        )?;
        if next.is_none() {
            self.upstream_done = true;
        }
        let _ = execution.program;
        Ok(next)
    }

    fn retain_prefix_memory(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let memory = self.pending_input_memory.get_or_insert(control.reserve(0)?);
        if let Some(row_memory) = self.prefix.take_output_memory() {
            memory.merge(row_memory)?;
        }
        if self.pending_input.len() == self.pending_input.capacity() {
            let capacity = self.pending_input.capacity().saturating_mul(2).max(4);
            memory.grow(crate::query::resources::allocation_bytes::<BindingRow>(
                capacity - self.pending_input.capacity(),
            )?)?;
            self.pending_input
                .reserve_exact(capacity - self.pending_input.len());
        }
        Ok(())
    }

    fn execute_next_batch(&mut self, execution: &mut StreamExecution<'_, '_>) -> QueryResult<()> {
        if !self.ensure_batch_started(execution)? {
            return Ok(());
        }
        self.fill_pending_batch(execution)?;
        if self.pending_input.is_empty() {
            return self.finish_input(execution);
        }
        self.execute_pending_batch(execution)
    }

    fn ensure_batch_started(
        &mut self,
        execution: &mut StreamExecution<'_, '_>,
    ) -> QueryResult<bool> {
        if !self.pending_input.is_empty() {
            return Ok(true);
        }
        let Some(first) = self.pull_prefix_row(execution)? else {
            self.finish_input(execution)?;
            return Ok(false);
        };
        let runtime = execution.runtime(self)?;
        if !self.concurrency_validated {
            validate_concurrency(&runtime, &self.spec, &first)?;
            self.concurrency_validated = true;
        }
        if self.batch_size.is_none() {
            self.batch_size = Some(resolve_batch_size(&runtime, &self.spec, &first)?);
        }
        self.commit = runtime.commit;
        self.counters = runtime.counters;
        self.next_batch_id = runtime.next_batch_id;
        self.retain_prefix_memory(execution.control)?;
        self.pending_input.push(first);
        Ok(true)
    }

    fn fill_pending_batch(&mut self, execution: &mut StreamExecution<'_, '_>) -> QueryResult<()> {
        let batch_size = self
            .batch_size
            .ok_or_else(|| QueryError::internal("transaction stream lost its batch size"))?;
        while self.pending_input.len() < batch_size && !self.upstream_done {
            let Some(row) = self.pull_prefix_row(execution)? else {
                break;
            };
            self.retain_prefix_memory(execution.control)?;
            self.pending_input.push(row);
        }
        Ok(())
    }

    fn execute_pending_batch(
        &mut self,
        execution: &mut StreamExecution<'_, '_>,
    ) -> QueryResult<()> {
        let batch_rows = std::mem::take(&mut self.pending_input);
        let batch = RowSet {
            columns: self.input_columns.clone(),
            rows: batch_rows,
            memory: self.pending_input_memory.take(),
        };
        let control = execution.control;
        let mut runtime = execution.runtime(self)?;
        validate_disjoint_expressions(&runtime, &self.spec, &batch.rows)?;
        let transaction_id = next_transaction_id(&mut runtime);
        let result = execute_batch_with_retry(
            &mut runtime,
            &self.subquery,
            batch.try_clone(control)?,
            &self.spec,
            &transaction_id,
        )?;
        let mut output = RowSet {
            columns: self.call_columns.clone(),
            rows: Vec::new(),
            memory: Some(control.reserve(0)?),
        };
        match result {
            Ok(successful) => {
                append_successful_batch(
                    &mut runtime,
                    &self.spec,
                    &transaction_id,
                    successful,
                    &mut output,
                )?;
            }
            Err(failure) => {
                self.broken = append_failed_batch(
                    &self.spec,
                    failure,
                    &batch.rows,
                    &[],
                    &self.call_columns,
                    &mut output,
                    control,
                )?;
            }
        }
        self.commit = runtime.commit;
        self.counters = runtime.counters;
        self.next_batch_id = runtime.next_batch_id;
        self.install_output(output, execution)
    }

    fn produce_unstarted_rows(
        &mut self,
        execution: &mut StreamExecution<'_, '_>,
    ) -> QueryResult<()> {
        let mut input_memory =
            execution
                .control
                .reserve(crate::query::resources::allocation_bytes::<BindingRow>(
                    256,
                )?)?;
        let mut inputs = Vec::with_capacity(256);
        while inputs.len() < 256 {
            let Some(row) = self.pull_prefix_row(execution)? else {
                break;
            };
            if let Some(row_memory) = self.prefix.take_output_memory() {
                input_memory.merge(row_memory)?;
            }
            inputs.push(row);
        }
        if inputs.is_empty() {
            return self.finish_input(execution);
        }
        let output = failed_rows(
            &inputs,
            &self.call_columns,
            self.spec.status_alias.as_deref(),
            transaction_status(false, false, None, None),
            execution.control,
        )?;
        self.install_output(output, execution)
    }

    fn install_output(
        &mut self,
        mut input: RowSet,
        execution: &mut StreamExecution<'_, '_>,
    ) -> QueryResult<()> {
        if input.rows.is_empty() {
            return Ok(());
        }
        input.track(execution.control)?;
        match self.output_policy {
            TransactionOutputPolicy::Discard => Ok(()),
            TransactionOutputPolicy::DeferredRead | TransactionOutputPolicy::OuterWrite { .. } => {
                self.defer_rows(&input.rows, execution.is_interrupted, execution.control)
            }
            TransactionOutputPolicy::Immediate => self.install_immediate_output(input, execution),
        }
    }

    fn defer_rows(
        &mut self,
        rows: &[BindingRow],
        is_interrupted: &dyn Fn() -> bool,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self.deferred_bindings.is_none() {
            let spill_connection = open_spill_connection()?;
            let spill = BindingSpill::create_with_resources(
                &spill_connection,
                control.resources().clone(),
            )?;
            self.deferred_bindings = Some((spill_connection, spill));
        }
        let (spill_connection, spill) = self
            .deferred_bindings
            .as_mut()
            .ok_or_else(|| QueryError::internal("transaction deferred output spill disappeared"))?;
        for row in rows {
            check_interrupted(is_interrupted)?;
            spill.push(spill_connection, row, control)?;
        }
        Ok(())
    }

    fn install_immediate_output(
        &mut self,
        input: RowSet,
        execution: &mut StreamExecution<'_, '_>,
    ) -> QueryResult<()> {
        let (snapshot, graph_view) = execution.graph(self.commit)?;
        let spill_connection = open_spill_connection()?;
        let mut spill = RowSpill::create_with_resources(
            &spill_connection,
            execution.control.resources().clone(),
        )?;
        if self.suffix.is_empty() {
            let _input_memory = input.memory;
            self.materialize_rows_without_suffix(
                &snapshot,
                input.rows,
                &spill_connection,
                &mut spill,
                &graph_view.query_access,
                execution.control,
            )?;
        } else {
            let Some(mut suffix) = program::build_program_suffix_stream(
                execution.connection,
                execution.program,
                &snapshot,
                execution.params,
                input,
                &self.suffix,
                execution.control,
            )?
            else {
                return Err(QueryError::internal(
                    "incremental transaction suffix became non-streamable after preparation",
                ));
            };
            while let Some(row) = suffix.next(
                execution.connection,
                &snapshot,
                &graph_view,
                execution.params,
                execution.metrics,
                execution.control,
            )? {
                spill.push(&spill_connection, &row, execution.control)?;
            }
        }
        self.current_output = Some((spill_connection, spill.output()));
        Ok(())
    }

    fn materialize_rows_without_suffix(
        &self,
        snapshot: &Snapshot<'_>,
        rows: Vec<BindingRow>,
        spill_connection: &Connection,
        spill: &mut RowSpill,
        query_access: &crate::query::access::PropertyRules,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for row in rows {
            control.check()?;
            let (values, _memory) =
                materialize_binding_row(snapshot, &self.call_columns, &row, query_access, control)?;
            spill.push(spill_connection, &values, control)?;
        }
        Ok(())
    }

    fn finish_input(&mut self, execution: &mut StreamExecution<'_, '_>) -> QueryResult<()> {
        match self.output_policy {
            TransactionOutputPolicy::Immediate | TransactionOutputPolicy::Discard => {
                self.finished = true;
                Ok(())
            }
            TransactionOutputPolicy::DeferredRead => self.finish_deferred_read(execution),
            TransactionOutputPolicy::OuterWrite { write_index } => {
                self.finish_outer_write(write_index, execution)
            }
        }
    }

    fn finish_deferred_read(&mut self, execution: &mut StreamExecution<'_, '_>) -> QueryResult<()> {
        let (spill_connection, spill) = self.take_deferred_bindings(execution.control)?;
        let snapshot = crate::query::graph::resolve_snapshot_with_control(
            execution.connection,
            self.commit,
            execution.control,
        )?;
        let Some(stream) = program::build_program_spilled_suffix_stream(
            execution.connection,
            execution.program,
            &snapshot,
            execution.params,
            self.call_columns.clone(),
            spill_connection,
            spill,
            &self.suffix,
            execution.control,
        )?
        else {
            return Err(QueryError::internal(
                "deferred transaction suffix became non-streamable after preparation",
            ));
        };
        self.final_stream = Some(stream);
        Ok(())
    }

    fn finish_outer_write(
        &mut self,
        write_index: usize,
        execution: &mut StreamExecution<'_, '_>,
    ) -> QueryResult<()> {
        let (spill_connection, spill) = self.take_deferred_bindings(execution.control)?;
        let (write_spill_connection, write_spill) = if write_index == 0 {
            (spill_connection, spill)
        } else {
            self.materialize_outer_write_prefix(write_index, spill_connection, spill, execution)?
        };
        let options = transaction_options(execution.program)?;
        let base_commit = storage::branch_head(execution.connection, &options.branch)?;
        let mut transaction = TransactionMutationContext {
            connection: execution.connection,
            program: execution.program,
            base_commit,
            branch: &options.branch,
            graph_view: &options.graph_view,
            access: &options.access,
            author: options.author.as_deref(),
            message: options.message.as_deref(),
            params: execution.params,
            metrics: execution.metrics,
            resources: execution.control.resources(),
            is_interrupted: execution.is_interrupted,
        };
        let outcome = execute_spilled_outer_write_transaction(
            &mut transaction,
            &self.suffix[write_index..],
            write_spill_connection,
            write_spill,
        )?;
        self.commit = outcome.commit;
        add_mutation_counters(&mut self.counters, &outcome.counters);
        self.finished = true;
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "outer-write prefix materialization keeps the final snapshot and spill state explicit"
    )]
    fn materialize_outer_write_prefix(
        &self,
        write_index: usize,
        spill_connection: Connection,
        spill: BindingSpill,
        execution: &mut StreamExecution<'_, '_>,
    ) -> QueryResult<(Connection, BindingSpill)> {
        let (snapshot, graph_view) = execution.graph(self.commit)?;
        let Some(mut stream) = program::build_program_spilled_suffix_stream(
            execution.connection,
            execution.program,
            &snapshot,
            execution.params,
            self.call_columns.clone(),
            spill_connection,
            spill,
            &self.suffix[..write_index],
            execution.control,
        )?
        else {
            return Err(QueryError::internal(
                "outer-write transaction prefix became non-streamable after preparation",
            ));
        };
        let output_connection = open_spill_connection()?;
        let mut output = BindingSpill::create_with_resources(
            &output_connection,
            execution.control.resources().clone(),
        )?;
        while let Some(row) = stream.next_binding(
            execution.connection,
            &snapshot,
            &graph_view,
            execution.params,
            execution.metrics,
            execution.control,
        )? {
            output.push(&output_connection, &row, execution.control)?;
        }
        Ok((output_connection, output))
    }

    fn take_deferred_bindings(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<(Connection, BindingSpill)> {
        if let Some(deferred) = self.deferred_bindings.take() {
            return Ok(deferred);
        }
        let connection = open_spill_connection()?;
        let spill = BindingSpill::create_with_resources(&connection, control.resources().clone())?;
        Ok((connection, spill))
    }

    fn drain_final_stream(
        &mut self,
        max_rows: usize,
        execution: &mut StreamExecution<'_, '_>,
    ) -> QueryResult<(Vec<Vec<Value>>, bool)> {
        if self.final_stream.is_none() {
            return Ok((Vec::new(), false));
        }
        let (snapshot, graph_view) = execution.graph(self.commit)?;
        let mut memory = execution
            .control
            .reserve(crate::query::resources::allocation_bytes::<Vec<Value>>(
                max_rows,
            )?)?;
        let mut rows = Vec::with_capacity(max_rows);
        let mut ended = false;
        while rows.len() < max_rows {
            let next = self
                .final_stream
                .as_mut()
                .ok_or_else(|| QueryError::internal("transaction final stream disappeared"))?
                .next(
                    execution.connection,
                    &snapshot,
                    &graph_view,
                    execution.params,
                    execution.metrics,
                    execution.control,
                )?;
            let Some(row) = next else {
                ended = true;
                break;
            };
            if let Some(row_memory) = self
                .final_stream
                .as_mut()
                .and_then(program::ProgramStream::take_output_memory)
            {
                memory.merge(row_memory)?;
            }
            rows.push(row);
        }
        self.output_memory = Some(memory);
        if ended {
            self.final_stream = None;
            self.finished = true;
        }
        Ok((rows, ended))
    }

    fn drain_output(
        &mut self,
        max_rows: usize,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Vec<Vec<Value>>> {
        let Some((connection, mut output)) = self.current_output.take() else {
            return Ok(Vec::new());
        };
        let mut memory = control.reserve(
            crate::query::resources::allocation_bytes::<Vec<Value>>(max_rows)?,
        )?;
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows {
            let Some((sequence, row, row_memory)) =
                crate::query::spill::read_output_row_with_resources(
                    &connection,
                    &output.table,
                    output.after,
                    control,
                )?
            else {
                drop_temp_table(&connection, &output.table)?;
                self.output_memory = Some(memory);
                return Ok(rows);
            };
            memory.merge(row_memory)?;
            output.after = sequence;
            rows.push(row);
        }
        self.output_memory = Some(memory);
        self.current_output = Some((connection, output));
        Ok(rows)
    }
}

fn transaction_options(
    program: &PreparedProgram,
) -> QueryResult<&crate::query::completeness::TransactionProgramOptions> {
    program
        .transaction_options
        .as_ref()
        .ok_or_else(|| QueryError::internal("transaction program is missing transaction options"))
}

fn materialize_binding_row(
    snapshot: &Snapshot<'_>,
    columns: &[String],
    row: &BindingRow,
    query_access: &crate::query::access::PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<Value>, crate::query::resources::MemoryReservation)> {
    let mut memory = control.reserve(0)?;
    let mut values = Vec::with_capacity(columns.len());
    for column in columns {
        let evaluated = expression::binding_value_with_memory(
            snapshot,
            row.values.get(column).unwrap_or(&BindingValue::Null),
            query_access,
            control,
        )?;
        memory.merge(evaluated.reservation)?;
        values.push(evaluated.value);
    }
    Ok((values, memory))
}

fn incremental_suffix_is_safe(clauses: &[AstNode]) -> bool {
    clauses.iter().all(|clause| {
        matches!(
            clause.kind,
            AstKind::Clause(
                ClauseKind::Return
                    | ClauseKind::With
                    | ClauseKind::Filter
                    | ClauseKind::Let
                    | ClauseKind::Unwind
                    | ClauseKind::For
                    | ClauseKind::LoadCsv
                    | ClauseKind::Finish
            )
        ) && !clause.descendants().any(|node| {
            matches!(
                node.kind,
                AstKind::OrderBy
                    | AstKind::GroupBy
                    | AstKind::StarProjection
                    | AstKind::SetQuantifier(crate::cypher::SetQuantifierKind::Distinct)
                    | AstKind::Search
                    | AstKind::Subquery(_)
                    | AstKind::FunctionName
                    | AstKind::PropertyExpression
            )
        })
    })
}

fn transaction_result_requires_final_snapshot(subquery: &AstNode, suffix: &[AstNode]) -> bool {
    if suffix.iter().any(|clause| {
        clause.descendants().any(|node| {
            matches!(
                node.kind,
                AstKind::PropertyExpression | AstKind::FunctionName | AstKind::Search
            )
        })
    }) {
        return true;
    }
    let Some(body) = subquery
        .descendants()
        .find(|node| node.kind == AstKind::QueryBody)
    else {
        return false;
    };
    crate::cypher::query_body_returns_columns(body)
        && body.descendants().any(|node| {
            matches!(
                node.kind,
                AstKind::NodePattern
                    | AstKind::RelationshipPattern
                    | AstKind::RelationshipChain
                    | AstKind::PropertyExpression
                    | AstKind::Search
            )
        })
}
