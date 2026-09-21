use super::*;
use crate::query::spill::{RowSpill, SpillOutput, drop_temp_table, read_output_row};
use crate::query::stream::program;

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
    subquery: &AstNode,
    suffix: &[AstNode],
) -> QueryResult<Option<TransactionOutputPolicy>> {
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
        )?
    {
        return Ok(None);
    }
    let defer_output = !incremental_suffix_is_safe(suffix)
        || transaction_result_requires_final_snapshot(subquery, suffix);
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
) -> QueryResult<bool> {
    if prefix.is_empty() {
        return Ok(true);
    }
    let snapshot = Snapshot::resolve(connection, initial_commit)?;
    Ok(program::build_program_suffix_stream(
        connection,
        program,
        &snapshot,
        params,
        call_columns.to_vec(),
        Vec::new(),
        prefix,
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
) -> QueryResult<bool> {
    let snapshot = Snapshot::resolve(connection, initial_commit)?;
    let Some(stream) = program::build_program_suffix_stream(
        connection,
        program,
        &snapshot,
        params,
        call_columns.to_vec(),
        Vec::new(),
        suffix,
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

        let prefix_snapshot = Snapshot::resolve(connection, initial_commit)?;
        let prefix_graph_view = ResolvedGraphView::resolve(connection, &options.graph_view)?;
        let Some(prefix) = program::build_program_prefix_stream(
            connection,
            program,
            &prefix_snapshot,
            params,
            &shape.prefix_clauses,
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
            &shape.subquery,
            &shape.suffix,
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
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<TransactionStreamBatch> {
        let max_rows = max_rows.clamp(1, 4_096);
        loop {
            let rows = self.drain_output(max_rows)?;
            if !rows.is_empty() {
                return Ok(self.batch(rows, false));
            }
            let (rows, final_done) = self.drain_final_stream(
                connection,
                program,
                params,
                metrics,
                max_rows,
                is_interrupted,
            )?;
            if !rows.is_empty() || final_done {
                return Ok(self.batch(rows, final_done));
            }
            if self.finished {
                return Ok(self.batch(Vec::new(), true));
            }
            check_interrupted(is_interrupted)?;
            if self.broken {
                self.produce_unstarted_rows(connection, program, params, metrics, is_interrupted)?;
            } else {
                self.execute_next_batch(connection, program, params, metrics, is_interrupted)?;
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
        self.finished = true;
        Ok(())
    }

    fn batch(&self, rows: Vec<Vec<Value>>, done: bool) -> TransactionStreamBatch {
        TransactionStreamBatch {
            rows,
            done,
            commit: self.commit,
            counters: self.counters.clone(),
        }
    }

    fn pull_prefix_row(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<Option<BindingRow>> {
        if self.upstream_done {
            return Ok(None);
        }
        let snapshot = Snapshot::resolve(connection, self.prefix_commit)?;
        let next = self.prefix.next_binding(
            connection,
            &snapshot,
            &self.prefix_graph_view,
            params,
            metrics,
            is_interrupted,
        )?;
        if next.is_none() {
            self.upstream_done = true;
        }
        let _ = program;
        Ok(next)
    }

    fn execute_next_batch(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        if !self.ensure_batch_started(connection, program, params, metrics, is_interrupted)? {
            return Ok(());
        }
        self.fill_pending_batch(connection, program, params, metrics, is_interrupted)?;
        if self.pending_input.is_empty() {
            return self.finish_input(connection, program, params, metrics, is_interrupted);
        }
        self.execute_pending_batch(connection, program, params, metrics, is_interrupted)
    }

    fn ensure_batch_started(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<bool> {
        if !self.pending_input.is_empty() {
            return Ok(true);
        }
        let Some(first) =
            self.pull_prefix_row(connection, program, params, metrics, is_interrupted)?
        else {
            self.finish_input(connection, program, params, metrics, is_interrupted)?;
            return Ok(false);
        };
        let options = transaction_options(program)?;
        let runtime = TransactionRuntime {
            connection,
            program,
            options,
            params,
            metrics,
            is_interrupted,
            commit: self.commit,
            counters: self.counters.clone(),
            next_batch_id: self.next_batch_id,
        };
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
        self.pending_input.push(first);
        Ok(true)
    }

    fn fill_pending_batch(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        let batch_size = self
            .batch_size
            .ok_or_else(|| QueryError::internal("transaction stream lost its batch size"))?;
        while self.pending_input.len() < batch_size && !self.upstream_done {
            let Some(row) =
                self.pull_prefix_row(connection, program, params, metrics, is_interrupted)?
            else {
                break;
            };
            self.pending_input.push(row);
        }
        Ok(())
    }

    fn execute_pending_batch(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        let batch_rows = std::mem::take(&mut self.pending_input);
        let options = transaction_options(program)?;
        let mut runtime = TransactionRuntime {
            connection,
            program,
            options,
            params,
            metrics,
            is_interrupted,
            commit: self.commit,
            counters: self.counters.clone(),
            next_batch_id: self.next_batch_id,
        };
        validate_disjoint_expressions(&runtime, &self.spec, &batch_rows)?;
        let transaction_id = next_transaction_id(&mut runtime);
        let result = execute_batch_with_retry(
            &mut runtime,
            &self.subquery,
            RowSet {
                columns: self.input_columns.clone(),
                rows: batch_rows.clone(),
            },
            &self.spec,
            &transaction_id,
        )?;
        let mut output = Vec::new();
        match result {
            Ok(successful) => {
                append_successful_batch(
                    &mut runtime,
                    &self.spec,
                    &transaction_id,
                    successful,
                    &mut output,
                );
            }
            Err(failure) => {
                self.broken = append_failed_batch(
                    &self.spec,
                    failure,
                    &batch_rows,
                    &[],
                    &self.call_columns,
                    &mut output,
                )?;
            }
        }
        self.commit = runtime.commit;
        self.counters = runtime.counters;
        self.next_batch_id = runtime.next_batch_id;
        self.install_output(connection, program, params, metrics, output, is_interrupted)
    }

    fn produce_unstarted_rows(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        let mut inputs = Vec::new();
        while inputs.len() < 256 {
            let Some(row) =
                self.pull_prefix_row(connection, program, params, metrics, is_interrupted)?
            else {
                break;
            };
            inputs.push(row);
        }
        if inputs.is_empty() {
            return self.finish_input(connection, program, params, metrics, is_interrupted);
        }
        let output = failed_rows(
            &inputs,
            &self.call_columns,
            self.spec.status_alias.as_deref(),
            transaction_status(false, false, None, None),
        );
        self.install_output(connection, program, params, metrics, output, is_interrupted)
    }

    fn install_output(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        rows: Vec<BindingRow>,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        match self.output_policy {
            TransactionOutputPolicy::Discard => Ok(()),
            TransactionOutputPolicy::DeferredRead | TransactionOutputPolicy::OuterWrite { .. } => {
                self.defer_rows(&rows, is_interrupted)
            }
            TransactionOutputPolicy::Immediate => self.install_immediate_output(
                connection,
                program,
                params,
                metrics,
                rows,
                is_interrupted,
            ),
        }
    }

    fn defer_rows(
        &mut self,
        rows: &[BindingRow],
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        if self.deferred_bindings.is_none() {
            let spill_connection = open_spill_connection()?;
            let spill = BindingSpill::create(&spill_connection)?;
            self.deferred_bindings = Some((spill_connection, spill));
        }
        let (spill_connection, spill) = self
            .deferred_bindings
            .as_mut()
            .ok_or_else(|| QueryError::internal("transaction deferred output spill disappeared"))?;
        for row in rows {
            check_interrupted(is_interrupted)?;
            spill.push(spill_connection, row)?;
        }
        Ok(())
    }

    fn install_immediate_output(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        rows: Vec<BindingRow>,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        let snapshot = Snapshot::resolve(connection, self.commit)?;
        let graph_view =
            ResolvedGraphView::resolve(connection, &transaction_options(program)?.graph_view)?;
        let spill_connection = open_spill_connection()?;
        let mut spill = RowSpill::create(&spill_connection)?;
        if self.suffix.is_empty() {
            self.materialize_rows_without_suffix(
                &snapshot,
                rows,
                &spill_connection,
                &mut spill,
                is_interrupted,
            )?;
        } else {
            let Some(mut suffix) = program::build_program_suffix_stream(
                connection,
                program,
                &snapshot,
                params,
                self.call_columns.clone(),
                rows,
                &self.suffix,
            )?
            else {
                return Err(QueryError::internal(
                    "incremental transaction suffix became non-streamable after preparation",
                ));
            };
            while let Some(row) = suffix.next(
                connection,
                &snapshot,
                &graph_view,
                params,
                metrics,
                is_interrupted,
            )? {
                spill.push(&spill_connection, &row)?;
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
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        for row in rows {
            check_interrupted(is_interrupted)?;
            spill.push(
                spill_connection,
                &materialize_binding_row(snapshot, &self.call_columns, &row)?,
            )?;
        }
        Ok(())
    }

    fn finish_input(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        match self.output_policy {
            TransactionOutputPolicy::Immediate | TransactionOutputPolicy::Discard => {
                self.finished = true;
                Ok(())
            }
            TransactionOutputPolicy::DeferredRead => {
                self.finish_deferred_read(connection, program, params)
            }
            TransactionOutputPolicy::OuterWrite { write_index } => self.finish_outer_write(
                connection,
                program,
                params,
                metrics,
                is_interrupted,
                write_index,
            ),
        }
    }

    fn finish_deferred_read(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
    ) -> QueryResult<()> {
        let (spill_connection, spill) = self.take_deferred_bindings()?;
        let snapshot = Snapshot::resolve(connection, self.commit)?;
        let Some(stream) = program::build_program_spilled_suffix_stream(
            connection,
            program,
            &snapshot,
            params,
            self.call_columns.clone(),
            spill_connection,
            spill,
            &self.suffix,
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
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        is_interrupted: &dyn Fn() -> bool,
        write_index: usize,
    ) -> QueryResult<()> {
        let (spill_connection, spill) = self.take_deferred_bindings()?;
        let (write_spill_connection, write_spill) = if write_index == 0 {
            (spill_connection, spill)
        } else {
            self.materialize_outer_write_prefix(
                connection,
                program,
                params,
                metrics,
                is_interrupted,
                write_index,
                spill_connection,
                spill,
            )?
        };
        let options = transaction_options(program)?;
        let base_commit = storage::branch_head(connection, &options.branch)?;
        let mut transaction = TransactionMutationContext {
            connection,
            program,
            base_commit,
            branch: &options.branch,
            graph_view: &options.graph_view,
            author: options.author.as_deref(),
            message: options.message.as_deref(),
            params,
            metrics,
            is_interrupted,
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
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        is_interrupted: &dyn Fn() -> bool,
        write_index: usize,
        spill_connection: Connection,
        spill: BindingSpill,
    ) -> QueryResult<(Connection, BindingSpill)> {
        let snapshot = Snapshot::resolve(connection, self.commit)?;
        let graph_view =
            ResolvedGraphView::resolve(connection, &transaction_options(program)?.graph_view)?;
        let Some(mut stream) = program::build_program_spilled_suffix_stream(
            connection,
            program,
            &snapshot,
            params,
            self.call_columns.clone(),
            spill_connection,
            spill,
            &self.suffix[..write_index],
        )?
        else {
            return Err(QueryError::internal(
                "outer-write transaction prefix became non-streamable after preparation",
            ));
        };
        let output_connection = open_spill_connection()?;
        let mut output = BindingSpill::create(&output_connection)?;
        while let Some(row) = stream.next_binding(
            connection,
            &snapshot,
            &graph_view,
            params,
            metrics,
            is_interrupted,
        )? {
            output.push(&output_connection, &row)?;
        }
        Ok((output_connection, output))
    }

    fn take_deferred_bindings(&mut self) -> QueryResult<(Connection, BindingSpill)> {
        if let Some(deferred) = self.deferred_bindings.take() {
            return Ok(deferred);
        }
        let connection = open_spill_connection()?;
        let spill = BindingSpill::create(&connection)?;
        Ok((connection, spill))
    }

    fn drain_final_stream(
        &mut self,
        connection: &Connection,
        program: &PreparedProgram,
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<(Vec<Vec<Value>>, bool)> {
        if self.final_stream.is_none() {
            return Ok((Vec::new(), false));
        }
        let snapshot = Snapshot::resolve(connection, self.commit)?;
        let graph_view = ResolvedGraphView::resolve(
            connection,
            &program
                .transaction_options
                .as_ref()
                .ok_or_else(|| {
                    QueryError::internal("transaction program is missing transaction options")
                })?
                .graph_view,
        )?;
        let mut rows = Vec::with_capacity(max_rows);
        let mut ended = false;
        while rows.len() < max_rows {
            let next = self
                .final_stream
                .as_mut()
                .ok_or_else(|| QueryError::internal("transaction final stream disappeared"))?
                .next(
                    connection,
                    &snapshot,
                    &graph_view,
                    params,
                    metrics,
                    is_interrupted,
                )?;
            let Some(row) = next else {
                ended = true;
                break;
            };
            rows.push(row);
        }
        if ended {
            self.final_stream = None;
            self.finished = true;
        }
        Ok((rows, ended))
    }

    fn drain_output(&mut self, max_rows: usize) -> QueryResult<Vec<Vec<Value>>> {
        let Some((connection, mut output)) = self.current_output.take() else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows {
            let Some((sequence, row)) = read_output_row(&connection, &output.table, output.after)?
            else {
                drop_temp_table(&connection, &output.table)?;
                return Ok(rows);
            };
            output.after = sequence;
            rows.push(row);
        }
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
) -> QueryResult<Vec<Value>> {
    columns
        .iter()
        .map(|column| {
            expression::binding_value(
                snapshot,
                row.values.get(column).unwrap_or(&BindingValue::Null),
            )
        })
        .collect()
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
