use super::*;

mod cursor;
mod hash;
mod join;
mod output;
use super::profile::MatchProfileSlots;
use crate::query::plan::ProfileRole;
use cursor::MatchFrame;
#[cfg(test)]
pub(crate) use hash::hash_resident_target;
pub(super) use hash::{HashBuildState, HashProbe};
pub(crate) use hash::{HashLayoutFacts, hash_layout, hash_table_peak_bytes};
pub(crate) use join::MatchReadBindings;

#[derive(Debug)]
pub(super) struct MatchPipeline {
    steps: Vec<MatchStep>,
    stack: Vec<StepCursor>,
    initialized: bool,
    empty_seed_emitted: bool,
    read_bindings: Vec<MatchReadBindings>,
    read_memory: Option<MemoryReservation>,
    fallback_read_bindings: MatchReadBindings,
    ordered_numeric: Option<crate::query::schema::OrderedNumericIndexRead>,
}

impl MatchPipeline {
    pub(super) fn clear(&mut self) {
        self.stack.clear();
        self.read_bindings = Vec::new();
        self.read_memory = None;
    }

    fn reserve_bindings(
        &mut self,
        depth: usize,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self.read_bindings.len() < depth {
            let additional = depth - self.read_bindings.len();
            let memory = self.read_memory.get_or_insert(control.reserve(0)?);
            memory.grow(crate::query::resources::allocation_bytes::<
                MatchReadBindings,
            >(additional)?)?;
            self.read_bindings.reserve_exact(additional);
            self.read_bindings
                .resize_with(depth, MatchReadBindings::default);
        }
        Ok(())
    }

    fn finish_bindings(&mut self, metrics: &mut QueryMetrics) -> QueryResult<()> {
        for (step, bindings) in self.steps.iter().zip(&mut self.read_bindings) {
            bindings.finish(step, metrics)?;
        }
        Ok(())
    }

    pub(super) fn new(steps: Vec<MatchStep>) -> Self {
        Self {
            steps,
            stack: Vec::new(),
            initialized: false,
            empty_seed_emitted: false,
            read_bindings: Vec::new(),
            read_memory: None,
            fallback_read_bindings: MatchReadBindings::default(),
            ordered_numeric: None,
        }
    }

    pub(super) fn require_ordered_numeric(
        &mut self,
        proof: crate::query::schema::OrderedNumericIndexRead,
    ) {
        self.ordered_numeric = Some(proof);
    }

    pub(super) fn next_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        if self.steps.is_empty() {
            if self.empty_seed_emitted {
                return Ok(None);
            }
            self.empty_seed_emitted = true;
            return Ok(Some(BindingRow::default()));
        }
        if !self.initialized {
            let mut cursor = StepCursor::new(&self.steps[0], BindingRow::default(), control)?;
            cursor.ordered_numeric = self.ordered_numeric;
            self.stack.push(cursor);
            self.initialized = true;
        }
        loop {
            control.work(1)?;
            let depth = self.stack.len();
            let joined = self
                .stack
                .last()
                .is_some_and(|frame| frame.step.join.is_some());
            if joined {
                self.reserve_bindings(depth, control)?;
            }
            let Some(frame) = self.stack.last_mut() else {
                self.finish_bindings(metrics)?;
                return Ok(None);
            };
            let bindings = if joined {
                &mut self.read_bindings[depth - 1]
            } else {
                &mut self.fallback_read_bindings
            };
            if let Some(row) =
                frame.next_row(snapshot, graph_view, params, metrics, bindings, control)?
            {
                if depth == self.steps.len() {
                    return Ok(Some(row));
                }
                self.stack
                    .push(StepCursor::new(&self.steps[depth], row, control)?);
                continue;
            }
            self.stack.pop();
        }
    }

    pub(super) fn next_packet(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<super::factor::BindingPacket>> {
        if self.steps.len() != 1
            || !self.steps[0]
                .join
                .as_ref()
                .is_some_and(|plan| plan.factorized.is_some())
        {
            return self
                .next_row(snapshot, graph_view, params, metrics, control)
                .map(|row| row.map(super::factor::BindingPacket::Flat));
        }
        if !self.initialized {
            self.stack.push(StepCursor::new(
                &self.steps[0],
                BindingRow::default(),
                control,
            )?);
            self.initialized = true;
        }
        if self.stack.is_empty() {
            return Ok(None);
        }
        self.reserve_bindings(1, control)?;
        let next = self.stack[0].next_packet(
            snapshot,
            graph_view,
            params,
            metrics,
            &mut self.read_bindings[0],
            control,
        )?;
        if next.is_none() {
            self.stack.clear();
            self.finish_bindings(metrics)?;
        }
        Ok(next)
    }
}

#[derive(Debug)]
pub(crate) struct StepCursor {
    step: MatchStep,
    base: BindingRow,
    stack: Vec<MatchFrame>,
    initialized: bool,
    matched: bool,
    optional_emitted: bool,
    _memory: MemoryReservation,
    output_memory: Option<MemoryReservation>,
    _seek_memory: MemoryReservation,
    node_ids: Option<(Vec<i64>, MemoryReservation)>,
    finite_advisory: bool,
    ordered_numeric: Option<crate::query::schema::OrderedNumericIndexRead>,
    factor: Option<Box<super::factor::producer::FactorProducer>>,
    flatten: Option<super::factor::FlattenContinuation>,
    profile_parts: Vec<usize>,
}

impl StepCursor {
    pub(crate) fn new(
        step: &MatchStep,
        mut base: BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        // Relationship uniqueness is scoped to one MATCH graph pattern. A
        // later MATCH clause may legally traverse a Relationship used by an
        // earlier MATCH while still inheriting its variable bindings.
        base.used_relationships.clear();
        let seek_memory = crate::query::plan::reserve_match_seeks(step, control)?;
        let factorized = step
            .join
            .as_ref()
            .is_some_and(|plan| plan.factorized.is_some());
        let base_bytes = if factorized {
            0
        } else {
            expression::binding_row_bytes_controlled(&base, Some(control))?
        };
        let memory = control.reserve(crate::query::resources::checked_add(
            base_bytes,
            crate::query::resources::allocation_bytes::<MatchFrame>(step.parts.len())?,
        )?)?;
        let factor = if factorized {
            Some(Box::new(super::factor::producer::FactorProducer::new(
                step,
                std::mem::take(&mut base),
                control,
            )?))
        } else {
            None
        };
        Ok(Self {
            step: step.clone(),
            base,
            stack: Vec::with_capacity(step.parts.len()),
            initialized: false,
            matched: false,
            optional_emitted: false,
            _memory: memory,
            output_memory: None,
            _seek_memory: seek_memory,
            node_ids: None,
            finite_advisory: false,
            ordered_numeric: None,
            factor,
            flatten: None,
            profile_parts: Vec::new(),
        })
    }

    pub(crate) fn set_profile_parts(
        &mut self,
        parts: &[usize],
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        self._memory
            .grow(crate::query::resources::allocation_bytes::<usize>(
                parts.len(),
            )?)?;
        control.check()?;
        let mut copied = Vec::new();
        copied.try_reserve_exact(parts.len()).map_err(|_| {
            crate::query::resources::memory_error("unable to allocate MATCH profile ordinals")
        })?;
        copied.extend_from_slice(parts);
        self.profile_parts = copied;
        Ok(())
    }

    pub(crate) fn take_output_memory(
        &mut self,
        row: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<MemoryReservation> {
        if let Some(flatten) = self.flatten.as_mut() {
            return flatten.split_output_memory(row, control);
        }
        if let Some(memory) = self.output_memory.as_mut() {
            return memory.split(expression::binding_row_bytes_controlled(
                row,
                Some(control),
            )?);
        }
        self.stack
            .last_mut()
            .ok_or_else(|| QueryError::internal("MATCH output lost its producer"))?
            .split_output_memory(row, control)
    }

    pub(crate) fn with_node_ids(
        step: &MatchStep,
        base: BindingRow,
        candidates: (Vec<i64>, MemoryReservation),
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let mut cursor = Self::with_pattern_node_ids(step, base, candidates, control)?;
        cursor.finite_advisory = true;
        Ok(cursor)
    }

    pub(crate) fn with_pattern_node_ids(
        step: &MatchStep,
        base: BindingRow,
        candidates: (Vec<i64>, MemoryReservation),
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        if step.optional
            || step.join.is_some()
            || step.parts.len() != 1
            || step.parts[0].relationship.is_some()
            || step.parts[0].end.is_some()
            || step.parts[0].path_variable.is_some()
        {
            return Err(QueryError::internal(
                "finite node access requires one plain MATCH part",
            ));
        }
        let mut cursor = Self::new(step, base, control)?;
        cursor.node_ids = Some(candidates);
        Ok(cursor)
    }

    fn initialize(
        &mut self,
        snapshot: &Snapshot<'_>,
        view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        bindings: &mut MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if !self.initialized {
            if !self.step.parts.is_empty() {
                let mut frame =
                    self.frame(0, &self.base, (snapshot, view), metrics, bindings, control)?;
                frame.ordered_numeric = self.ordered_numeric;
                if let Some((ids, memory)) = self.node_ids.take() {
                    frame.start_buffer = ids;
                    frame.start_done = true;
                    frame.finite_starts = true;
                    frame.finite_advisory = self.finite_advisory;
                    frame._finite_memory = Some(memory);
                }
                self.stack.push(MatchFrame::part(frame)?);
            }
            self.initialized = true;
        }
        Ok(())
    }

    pub(crate) fn next_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        bindings: &mut MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        if self.factor.is_none() {
            return self.next_flat_row(snapshot, graph_view, params, metrics, bindings, control);
        }
        loop {
            if let Some(flatten) = self.flatten.as_mut()
                && let Some(mut row) = flatten.next_row(control)?
            {
                if let Some((clause, delta)) = flatten.take_storage_metrics()? {
                    metrics.record_factorized_storage(clause, "Flatten", delta);
                }
                metrics.record_factorized_flatten(self.step.clause_start, 1);
                self.restore_declared_order(&mut row);
                return Ok(Some(row));
            }
            let Some(packet) =
                self.next_packet(snapshot, graph_view, params, metrics, bindings, control)?
            else {
                return Ok(None);
            };
            if self.flatten.is_none() {
                self.flatten = Some(super::factor::FlattenContinuation::new(control)?);
            }
            self.flatten
                .as_mut()
                .expect("installed factor continuation")
                .install(packet, control)?;
        }
    }

    pub(crate) fn next_packet(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        bindings: &mut MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<super::factor::BindingPacket>> {
        if let Some(factor) = self.factor.as_mut() {
            return factor
                .next_packet((snapshot, graph_view), params, metrics, control)
                .map(|packet| packet.map(super::factor::BindingPacket::Group));
        }
        self.next_flat_row(snapshot, graph_view, params, metrics, bindings, control)
            .map(|row| row.map(super::factor::BindingPacket::Flat))
    }

    fn next_flat_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        bindings: &mut MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        self.initialize(snapshot, graph_view, metrics, bindings, control)?;
        while let Some(mut row) = self.advance_to(
            self.step.parts.len(),
            (snapshot, graph_view),
            params,
            metrics,
            bindings,
            control,
        )? {
            if self.full_match_passes(&row, snapshot, graph_view, params, control)? {
                if self.step.predicate.is_some() {
                    metrics.record_profile_rows(
                        metrics.match_residual_slot(self.step.clause_start),
                        1,
                    );
                }
                self.matched = true;
                self.restore_declared_order(&mut row);
                return Ok(Some(row));
            }
        }
        self.optional_or_done(control)
    }

    fn optional_or_done(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        if self.step.optional && !self.matched && !self.optional_emitted {
            self.optional_emitted = true;
            let mut bytes = expression::binding_row_bytes_controlled(&self.base, Some(control))?;
            for part in &self.step.parts {
                for name in [
                    part.path_variable.as_ref(),
                    part.start.variable.as_ref(),
                    part.relationship
                        .as_ref()
                        .and_then(|spec| spec.variable.as_ref()),
                    part.end.as_ref().and_then(|spec| spec.variable.as_ref()),
                ]
                .into_iter()
                .flatten()
                {
                    bytes = crate::query::resources::checked_add(
                        bytes,
                        crate::query::resources::checked_add(
                            crate::query::resources::map_entry_bytes::<BindingValue>()?,
                            name.capacity() as u64,
                        )?,
                    )?;
                }
            }
            self.output_memory = Some(control.reserve(bytes)?);
            let mut row = self.base.clone();
            for part in &self.step.parts {
                set_optional_null(&mut row, part.path_variable.as_deref());
                set_optional_null(&mut row, part.start.variable.as_deref());
                if let Some(rel) = &part.relationship {
                    set_optional_null(&mut row, rel.variable.as_deref());
                }
                if let Some(end) = &part.end {
                    set_optional_null(&mut row, end.variable.as_deref());
                }
            }
            return Ok(Some(row));
        }
        Ok(None)
    }
}

fn set_optional_null(row: &mut BindingRow, variable: Option<&str>) {
    if let Some(variable) = variable
        && !row.values.contains_key(variable)
    {
        row.insert(variable.to_owned(), BindingValue::Null);
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "match streaming keeps snapshot, view, params, metrics, cancellation, and spill state explicit"
)]
pub(crate) fn spill_match_step(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    params: &std::collections::BTreeMap<String, Value>,
    input: Vec<BindingRow>,
    step: &MatchStep,
    metrics: &mut QueryMetrics,
    control: &ExecutionControl<'_>,
    spill_connection: &Connection,
    output: &mut BindingSpill,
    bindings: &mut MatchReadBindings,
) -> QueryResult<()> {
    for row in input {
        let mut cursor = StepCursor::new(step, row, control)?;
        loop {
            control.work(1)?;
            control.check()?;
            let Some(row) =
                cursor.next_row(snapshot, graph_view, params, metrics, bindings, control)?
            else {
                break;
            };
            output.push(spill_connection, &row, control)?;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct PartCursor {
    profile: Option<MatchProfileSlots>,
    probe_template: Option<usize>,
    probe_value: Option<Value>,
    probe_memory: Option<MemoryReservation>,
    probe_empty: bool,
    part: PatternPart,
    base: BindingRow,
    start_after: i64,
    initial_page_limit: usize,
    start_index_cursor: Option<crate::query::schema::StandardIndexCursor>,
    start_buffer: Vec<i64>,
    start_known_label: Option<LabelId>,
    start_index: usize,
    start_done: bool,
    finite_starts: bool,
    finite_advisory: bool,
    _finite_memory: Option<MemoryReservation>,
    dynamic_filter: Option<hash::dynamic_filter::ProbeFilter>,
    ordered_numeric: Option<crate::query::schema::OrderedNumericIndexRead>,
    current_start: Option<i64>,
    relationships: Vec<RelationshipRecord>,
    relationship_index: usize,
    relationship_cursor: Option<crate::storage::AdjacencyCursor>,
    relationship_done: bool,
    bound_start_emitted: bool,
    indexed_relationship_cursor: Option<crate::query::schema::StandardIndexCursor>,
    indexed_relationship_ids: Vec<i64>,
    indexed_relationship_index: usize,
    indexed_relationship_done: bool,
    indexed_rows: Vec<BindingRow>,
    _memory: MemoryReservation,
    output_memory: Option<MemoryReservation>,
}

impl PartCursor {
    fn new(
        part: &PatternPart,
        base: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let base_bytes = expression::binding_row_bytes_controlled(base, Some(control))?;
        let page_bytes = crate::query::resources::checked_add(
            crate::query::resources::allocation_bytes::<RelationshipRecord>(PIPELINE_BATCH)?,
            crate::query::resources::allocation_bytes::<i64>(PIPELINE_BATCH * 2)?,
        )?;
        let memory = control.reserve(crate::query::resources::checked_add(
            crate::query::resources::checked_add(base_bytes, page_bytes)?,
            crate::query::plan::part_seek_bytes(part, control)?,
        )?)?;
        Ok(Self {
            profile: None,
            probe_template: None,
            probe_value: None,
            probe_memory: None,
            probe_empty: false,
            part: part.clone(),
            base: base.clone(),
            start_after: 0,
            initial_page_limit: PIPELINE_BATCH,
            start_index_cursor: None,
            start_buffer: Vec::new(),
            start_known_label: None,
            start_index: 0,
            start_done: false,
            finite_starts: false,
            finite_advisory: false,
            _finite_memory: None,
            dynamic_filter: None,
            ordered_numeric: None,
            current_start: None,
            relationships: Vec::new(),
            relationship_index: 0,
            relationship_cursor: None,
            relationship_done: false,
            bound_start_emitted: false,
            indexed_relationship_cursor: None,
            indexed_relationship_ids: Vec::new(),
            indexed_relationship_index: 0,
            indexed_relationship_done: false,
            indexed_rows: Vec::new(),
            _memory: memory,
            output_memory: None,
        })
    }

    fn next_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        bindings: &MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        if self.probe_empty || self.part_is_impossible(graph_view) {
            return Ok(None);
        }
        self.reserve_output(control)?;
        if self.part.relationship.as_ref().is_some_and(|relationship| {
            relationship.index_seek.is_some() || relationship.id_seek.is_some()
        }) {
            return self
                .next_indexed_relationship_row(snapshot, graph_view, metrics, bindings, control);
        }
        if self.part.relationship.is_none() {
            let Some(id) = self.next_start(snapshot, graph_view, metrics, bindings, control)?
            else {
                return Ok(None);
            };
            let Some(row) = bind_node(self.base.clone(), self.part.start.variable.as_deref(), id)?
            else {
                return Ok(None);
            };
            return bind_path(
                row,
                self.part.path_variable.as_deref(),
                vec![id],
                Vec::new(),
            );
        }
        self.next_relationship_row(snapshot, graph_view, metrics, bindings, control)
    }

    fn profile_access_operator(&self) -> Option<(&str, &'static str)> {
        if let Some(relationship) = &self.part.relationship
            && relationship.id_seek.is_some()
        {
            return Some((
                relationship.variable.as_deref().unwrap_or("_anon_rel"),
                "RelationshipIdSeek",
            ));
        }
        if let Some(relationship) = &self.part.relationship
            && relationship.index_seek.is_some()
        {
            return Some((
                relationship.variable.as_deref().unwrap_or("_anon_rel"),
                "IndexSeek",
            ));
        }
        let start = self.part.start.variable.as_deref().unwrap_or("_anon");
        if self
            .part
            .start
            .variable
            .as_deref()
            .is_some_and(|variable| self.base.values.contains_key(variable))
        {
            return Some((
                start,
                if self.part.relationship.is_some() {
                    "AdjacencySeek"
                } else {
                    "NodeIdSeek"
                },
            ));
        }
        if self.part.start.id_seek.is_some() {
            return Some((start, "NodeIdSeek"));
        }
        if self.part.start.index_seek.is_some() {
            return Some((start, "IndexSeek"));
        }
        if self.part.start.scan_label.is_some() {
            return Some((start, "LabelIndexScan"));
        }
        Some((start, "NodeScan"))
    }

    fn next_relationship_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        bindings: &MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        loop {
            control.work(1)?;
            if self.relationship_index < self.relationships.len() {
                let rel = self.relationships[self.relationship_index];
                self.relationship_index += 1;
                let access = if self.profile.is_some() {
                    self.activate_role(metrics, ProfileRole::RelationshipAccess)
                } else {
                    metrics.activate_access_for_operator(
                        Some(self.part.start.variable.as_deref().unwrap_or("_anon")),
                        "AdjacencySeek",
                    )
                };
                let matched = self.match_relationship(snapshot, graph_view, rel, metrics, control);
                self.record_fallback_relationship_rows(metrics, &matched, false);
                metrics.restore_access_operator(access);
                if let Some(row) = matched? {
                    return Ok(Some(row));
                }
                continue;
            }
            if let Some(start) = self.current_start
                && !self.relationship_done
            {
                let access = if self.profile.is_some() {
                    self.activate_role(metrics, ProfileRole::RelationshipAccess)
                } else {
                    metrics.activate_access_for_operator(
                        Some(self.part.start.variable.as_deref().unwrap_or("_anon")),
                        "AdjacencySeek",
                    )
                };
                let page = self.load_relationship_page(
                    snapshot,
                    start,
                    self.relationship_cursor,
                    metrics,
                    control,
                );
                metrics.restore_access_operator(access);
                let page = page?;
                self.relationships = page.items;
                self.relationship_index = 0;
                if let Some(cursor) = page.next_cursor {
                    self.relationship_cursor = Some(cursor);
                } else {
                    self.relationship_done = true;
                }
                if !self.relationships.is_empty() || !self.relationship_done {
                    continue;
                }
            }
            let Some(start) = self.next_start(snapshot, graph_view, metrics, bindings, control)?
            else {
                return Ok(None);
            };
            self.current_start = Some(start);
            self.relationships.clear();
            self.relationship_index = 0;
            self.relationship_cursor = None;
            self.relationship_done = false;
        }
    }

    fn next_indexed_relationship_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        bindings: &MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        loop {
            control.work(1)?;
            if let Some(row) = self.indexed_rows.pop() {
                return Ok(Some(row));
            }
            while self.indexed_relationship_index < self.indexed_relationship_ids.len() {
                control.work(1)?;
                let id = self.indexed_relationship_ids[self.indexed_relationship_index];
                self.indexed_relationship_index += 1;
                let Some(relationship) = snapshot.relationship(id)? else {
                    continue;
                };
                let Some(spec) = self.part.relationship.as_ref() else {
                    return Ok(None);
                };
                if spec
                    .type_id
                    .is_some_and(|type_id| type_id != relationship.type_id)
                {
                    continue;
                }
                let orientations = match spec.direction {
                    Direction::Outgoing => vec![(relationship.source, relationship.target)],
                    Direction::Incoming => vec![(relationship.target, relationship.source)],
                    Direction::Undirected if relationship.source == relationship.target => {
                        vec![(relationship.source, relationship.target)]
                    }
                    Direction::Undirected => vec![
                        (relationship.target, relationship.source),
                        (relationship.source, relationship.target),
                    ],
                };
                for (start, end) in orientations {
                    if let Some(row) = self.match_indexed_relationship_orientation(
                        snapshot,
                        graph_view,
                        relationship,
                        (start, end),
                        metrics,
                        control,
                    )? {
                        self.indexed_rows.push(row);
                    }
                }
                if let Some(row) = self.indexed_rows.pop() {
                    return Ok(Some(row));
                }
            }
            if self.indexed_relationship_done {
                return Ok(None);
            }
            self.load_next_indexed_relationship_ids(snapshot, metrics, bindings, control)?;
        }
    }

    fn load_next_indexed_relationship_ids(
        &mut self,
        snapshot: &Snapshot<'_>,
        metrics: &mut QueryMetrics,
        bindings: &MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let Some(spec) = self.part.relationship.as_ref() else {
            self.indexed_relationship_done = true;
            return Ok(());
        };
        if let Some(id) = spec.id_seek {
            metrics.record_db_hits(1);
            self.indexed_relationship_ids.push(id);
            self.indexed_relationship_index = 0;
            self.indexed_relationship_done = true;
            return Ok(());
        }
        let Some(seek) = spec.index_seek.as_ref() else {
            self.indexed_relationship_done = true;
            return Ok(());
        };
        let page = match self.scan_bound_page(
            snapshot,
            bindings,
            self.indexed_relationship_cursor.as_ref(),
            control,
        )? {
            Some(page) => page,
            None => crate::query::schema::scan_relationship_index_page(
                snapshot,
                seek,
                spec.type_id,
                self.indexed_relationship_cursor.as_ref(),
                PIPELINE_BATCH,
                control,
            )?,
        };
        metrics.record_db_hits(page.items.len() as u64);
        self.indexed_relationship_ids = page.items;
        self.indexed_relationship_index = 0;
        if let Some(cursor) = page.next_cursor {
            self.indexed_relationship_cursor = Some(cursor);
        } else {
            self.indexed_relationship_done = true;
        }
        Ok(())
    }

    fn match_indexed_relationship_orientation(
        &self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        relationship: RelationshipRecord,
        endpoints: (i64, i64),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        let (start, end) = endpoints;
        control.work(1)?;
        let Some(end_spec) = &self.part.end else {
            return Ok(None);
        };
        if !graph_view.visible_relationship_controlled(snapshot, relationship, control)? {
            return Ok(None);
        }
        if !node_matches(
            snapshot,
            graph_view,
            &self.part.start,
            start,
            metrics,
            control,
        )? || !node_matches(snapshot, graph_view, end_spec, end, metrics, control)?
        {
            return Ok(None);
        }
        self.bind_relationship_row(start, end, relationship)
    }

    fn bind_relationship_row(
        &self,
        start: i64,
        end: i64,
        relationship: RelationshipRecord,
    ) -> QueryResult<Option<BindingRow>> {
        let Some(spec) = &self.part.relationship else {
            return Ok(None);
        };
        let Some(end_spec) = &self.part.end else {
            return Ok(None);
        };
        if self.base.used_relationships.contains(&relationship.id)
            && !relationship_already_bound(&self.base, spec, relationship.id)
        {
            return Ok(None);
        }
        let Some(row) = bind_node(
            self.base.clone(),
            self.part.start.variable.as_deref(),
            start,
        )?
        else {
            return Ok(None);
        };
        let Some(row) = bind_relationship(row, spec, relationship)? else {
            return Ok(None);
        };
        let Some(row) = bind_node(row, end_spec.variable.as_deref(), end)? else {
            return Ok(None);
        };
        let Some(mut row) = bind_path(
            row,
            self.part.path_variable.as_deref(),
            vec![start, end],
            vec![relationship],
        )?
        else {
            return Ok(None);
        };
        row.used_relationships.insert(relationship.id);
        Ok(Some(row))
    }

    fn part_is_impossible(&self, graph_view: &ResolvedGraphView) -> bool {
        graph_view.is_empty()
            || self.part.start.impossible
            || self.part.end.as_ref().is_some_and(|node| node.impossible)
            || self
                .part
                .relationship
                .as_ref()
                .is_some_and(|relationship| relationship.impossible)
    }

    fn next_start(
        &mut self,
        snapshot: &Snapshot<'_>,
        view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        bindings: &MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<i64>> {
        let next = self.next_start_value(snapshot, view, metrics, bindings, control)?;
        if next.is_some() {
            if let Some(slots) = self.profile {
                metrics.record_profile_rows(slots.slot(ProfileRole::NodeAccess), 1);
            } else if metrics.has_explicit_profile_sites()
                || (self.part.relationship.is_none()
                    && self
                        .part
                        .start
                        .variable
                        .as_ref()
                        .is_some_and(|variable| self.base.values.contains_key(variable)))
            {
                metrics.record_active_rows(1);
            }
        }
        Ok(next)
    }

    fn next_start_value(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        bindings: &MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<i64>> {
        if let Some(variable) = self.part.start.variable.as_deref()
            && let Some(value) = self.base.values.get(variable)
        {
            if self.bound_start_emitted {
                return Ok(None);
            }
            self.bound_start_emitted = true;
            let BindingValue::Node(id) = value else {
                return Ok(None);
            };
            return if node_matches(
                snapshot,
                graph_view,
                &self.part.start,
                *id,
                metrics,
                control,
            )? {
                Ok(Some(*id))
            } else {
                Ok(None)
            };
        }
        self.next_scanned_start(snapshot, graph_view, metrics, bindings, control)
    }

    fn next_scanned_start(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        bindings: &MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<i64>> {
        if self.finite_starts {
            return self.next_finite_start(snapshot, graph_view, metrics, control);
        }
        if self.part.start.id_seek.is_some() {
            return self.next_id_start(snapshot, graph_view, metrics, control);
        }
        loop {
            control.work(1)?;
            while self.start_index < self.start_buffer.len() {
                control.work(1)?;
                let id = self.start_buffer[self.start_index];
                self.start_index += 1;
                if node_matches_scanned(
                    snapshot,
                    graph_view,
                    &self.part.start,
                    id,
                    self.start_known_label,
                    metrics,
                    control,
                )? {
                    return Ok(Some(id));
                }
            }
            if self.start_done {
                return Ok(None);
            }
            self.load_start_page(snapshot, graph_view, metrics, bindings, control)?;
        }
    }

    fn load_start_page(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        bindings: &MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self.load_dynamic_filter_page(snapshot, metrics, control)? {
            return Ok(());
        }
        if self.ordered_numeric.is_some()
            && !self
                .part
                .start
                .index_seek
                .as_ref()
                .is_some_and(|seek| seek.kind == crate::storage::StandardIndexKind::Range)
        {
            return Err(QueryError::internal(
                "ordered numeric access lost its Range seek",
            ));
        }
        if let Some(seek) = &self.part.start.index_seek
            && seek.kind != crate::storage::StandardIndexKind::Lookup
        {
            self.start_known_label = if self.probe_template.is_some() {
                None
            } else {
                self.part.start.scan_label
            };
            let page = if let Some(proof) = &self.ordered_numeric {
                crate::query::schema::scan_topk_numeric_page(
                    snapshot,
                    proof,
                    seek,
                    self.start_index_cursor.as_ref(),
                    PIPELINE_BATCH,
                    control,
                )?
            } else {
                match self.scan_bound_page(
                    snapshot,
                    bindings,
                    self.start_index_cursor.as_ref(),
                    control,
                )? {
                    Some(page) => page,
                    None => crate::query::schema::scan_node_index_page(
                        snapshot,
                        seek,
                        self.start_index_cursor.as_ref(),
                        PIPELINE_BATCH,
                        control,
                    )?,
                }
            };
            metrics.record_db_hits(page.items.len() as u64);
            self.start_buffer = page.items;
            self.start_index = 0;
            self.start_index_cursor = page.next_cursor;
            self.start_done = self.start_index_cursor.is_none();
        } else {
            let page_limit = std::mem::replace(&mut self.initial_page_limit, PIPELINE_BATCH);
            let (page, known_label) = graph_view.scan_nodes_after(
                snapshot,
                self.part.start.scan_label,
                self.start_after,
                page_limit,
                control,
            )?;
            self.start_known_label = known_label;
            metrics.record_db_hits(page.items.len() as u64);
            self.start_buffer = page.items;
            self.start_index = 0;
            if let Some(after) = page.next_after {
                self.start_after = after;
            } else {
                self.start_done = true;
            }
        }
        Ok(())
    }

    fn next_finite_start(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<i64>> {
        while let Some(&id) = self.start_buffer.get(self.start_index) {
            control.work(1)?;
            self.start_index += 1;
            metrics.record_db_hits(1);
            if self.finite_advisory {
                metrics.record_semi_mask(0, 1);
            }
            if node_matches(snapshot, graph_view, &self.part.start, id, metrics, control)? {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    fn next_id_start(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<i64>> {
        let Some(id) = self.part.start.id_seek else {
            return Ok(None);
        };
        if self.start_done {
            return Ok(None);
        }
        self.start_done = true;
        control.work(1)?;
        metrics.record_db_hits(1);
        if node_matches(snapshot, graph_view, &self.part.start, id, metrics, control)? {
            Ok(Some(id))
        } else {
            Ok(None)
        }
    }

    fn load_relationship_page(
        &self,
        snapshot: &Snapshot<'_>,
        start: i64,
        cursor: Option<crate::storage::AdjacencyCursor>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<crate::storage::AdjacencyScanPage> {
        control.work(1)?;
        let Some(spec) = &self.part.relationship else {
            return Ok(crate::storage::AdjacencyScanPage {
                items: Vec::new(),
                next_cursor: None,
            });
        };
        let page = match spec.direction {
            Direction::Outgoing => {
                snapshot.scan_outgoing_page(start, spec.type_id, cursor, PIPELINE_BATCH)?
            }
            Direction::Incoming => {
                snapshot.scan_incoming_page(start, spec.type_id, cursor, PIPELINE_BATCH)?
            }
            Direction::Undirected => {
                snapshot.scan_incident_page(start, spec.type_id, cursor, PIPELINE_BATCH)?
            }
        };
        metrics.record_db_hits(page.items.len() as u64);
        Ok(page)
    }

    fn match_relationship(
        &self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        relationship: RelationshipRecord,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        control.work(1)?;
        let Some(start) = self.current_start else {
            return Ok(None);
        };
        let Some(spec) = &self.part.relationship else {
            return Ok(None);
        };
        if !graph_view.visible_relationship_controlled(snapshot, relationship, control)? {
            return Ok(None);
        }
        metrics.record_db_hits(2);
        let end_id = match spec.direction {
            Direction::Outgoing => relationship.target,
            Direction::Incoming => relationship.source,
            Direction::Undirected if relationship.source == start => relationship.target,
            Direction::Undirected => relationship.source,
        };
        let Some(end_spec) = &self.part.end else {
            return Ok(None);
        };
        if !node_matches(snapshot, graph_view, end_spec, end_id, metrics, control)? {
            return Ok(None);
        }
        self.bind_relationship_row(start, end_id, relationship)
    }
}

fn relationship_already_bound(row: &BindingRow, spec: &RelationshipSpec, id: i64) -> bool {
    spec.variable
        .as_deref()
        .and_then(|name| row.values.get(name))
        .is_some_and(|value| matches!(value, BindingValue::Relationship(rel) if rel.id == id))
}

fn bind_node(
    mut row: BindingRow,
    variable: Option<&str>,
    node_id: i64,
) -> QueryResult<Option<BindingRow>> {
    let Some(variable) = variable else {
        return Ok(Some(row));
    };
    match row.values.get(variable) {
        Some(BindingValue::Node(existing)) if *existing == node_id => Ok(Some(row)),
        Some(BindingValue::Node(_) | BindingValue::Null) => Ok(None),
        Some(_) => Err(QueryError::semantic(format!(
            "variable {variable} is not a Node binding"
        ))),
        None => {
            row.insert(variable.to_owned(), BindingValue::Node(node_id));
            Ok(Some(row))
        }
    }
}

fn bind_relationship(
    mut row: BindingRow,
    spec: &RelationshipSpec,
    relationship: RelationshipRecord,
) -> QueryResult<Option<BindingRow>> {
    row.bind_relationship(spec.variable.as_deref(), relationship)
        .map(|matches| matches.then_some(row))
}

fn bind_path(
    mut row: BindingRow,
    variable: Option<&str>,
    nodes: Vec<i64>,
    relationships: Vec<RelationshipRecord>,
) -> QueryResult<Option<BindingRow>> {
    let Some(variable) = variable else {
        return Ok(Some(row));
    };
    match row.values.get(variable) {
        Some(BindingValue::Path {
            nodes: existing_nodes,
            relationships: existing_relationships,
        }) if *existing_nodes == nodes && *existing_relationships == relationships => Ok(Some(row)),
        Some(BindingValue::Path { .. } | BindingValue::Null) => Ok(None),
        Some(_) => Err(QueryError::semantic(format!(
            "variable {variable} is not a Path binding"
        ))),
        None => {
            row.insert(
                variable.to_owned(),
                BindingValue::Path {
                    nodes,
                    relationships,
                },
            );
            Ok(Some(row))
        }
    }
}

// Do not expose errors from a partial Pattern Part. An original WHERE evaluates
// only after all Parts have bound and performs lazy AND left-to-right.
// Deferring a failing prefix (including Type errors) preserves that behavior;
// interruption/resource rejection still obey the immediate execution contract.
fn early_prefix_rejects(
    expressions: &[crate::query::expression::Expr],
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    row: &BindingRow,
    params: &std::collections::BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    for expression in expressions {
        let result = expression::evaluate_with_access_control(
            expression,
            snapshot,
            row,
            params,
            &graph_view.query_access,
            control,
        );
        let evaluated = match result {
            Ok(value) => value,
            Err(error)
                if !matches!(
                    error.kind,
                    crate::query::QueryErrorKind::Resource
                        | crate::query::QueryErrorKind::Interrupted
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        match evaluated.value {
            Value::Boolean(false) => return Ok(true),
            Value::Boolean(true) | Value::Null => {}
            _ => return Ok(false),
        }
    }
    Ok(false)
}
