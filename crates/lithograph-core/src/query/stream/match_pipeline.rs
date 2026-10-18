use super::*;

#[derive(Debug)]
pub(super) struct MatchPipeline {
    steps: Vec<MatchStep>,
    stack: Vec<StepCursor>,
    initialized: bool,
    empty_seed_emitted: bool,
}

impl MatchPipeline {
    pub(super) fn clear(&mut self) {
        self.stack.clear();
    }

    pub(super) fn new(steps: Vec<MatchStep>) -> Self {
        Self {
            steps,
            stack: Vec::new(),
            initialized: false,
            empty_seed_emitted: false,
        }
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
            self.stack.push(StepCursor::new(
                self.steps[0].clone(),
                BindingRow::default(),
                control,
            )?);
            self.initialized = true;
        }
        loop {
            control.work(1)?;
            let depth = self.stack.len();
            let Some(frame) = self.stack.last_mut() else {
                return Ok(None);
            };
            if let Some(row) = frame.next_row(snapshot, graph_view, params, metrics, control)? {
                if depth == self.steps.len() {
                    return Ok(Some(row));
                }
                self.stack
                    .push(StepCursor::new(self.steps[depth].clone(), row, control)?);
                continue;
            }
            self.stack.pop();
        }
    }
}

#[derive(Debug)]
pub(super) struct StepCursor {
    step: MatchStep,
    base: BindingRow,
    stack: Vec<PartCursor>,
    initialized: bool,
    matched: bool,
    optional_emitted: bool,
    _memory: MemoryReservation,
    output_memory: Option<MemoryReservation>,
}

impl StepCursor {
    pub(super) fn new(
        step: MatchStep,
        mut base: BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        // Relationship uniqueness is scoped to one MATCH graph pattern. A
        // later MATCH clause may legally traverse a Relationship used by an
        // earlier MATCH while still inheriting its variable bindings.
        base.used_relationships.clear();
        let memory = control.reserve(expression::binding_row_bytes_controlled(
            &base,
            Some(control),
        )?)?;
        Ok(Self {
            step,
            base,
            stack: Vec::new(),
            initialized: false,
            matched: false,
            optional_emitted: false,
            _memory: memory,
            output_memory: None,
        })
    }

    pub(super) fn next_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        if !self.initialized {
            if let Some(part) = self.step.parts.first() {
                self.stack
                    .push(PartCursor::new(part.clone(), &self.base, control)?);
            }
            self.initialized = true;
        }
        loop {
            control.work(1)?;
            let depth = self.stack.len();
            if depth == 0 {
                return self.optional_or_done(control);
            }
            let Some(frame) = self.stack.last_mut() else {
                return self.optional_or_done(control);
            };
            if let Some(row) = frame.next_row(snapshot, graph_view, metrics, control)? {
                if depth < self.step.parts.len() {
                    self.stack.push(PartCursor::new(
                        self.step.parts[depth].clone(),
                        &row,
                        control,
                    )?);
                    continue;
                }
                let passes = match &self.step.predicate {
                    Some(predicate) => expression::predicate(
                        expression::evaluate_with_access_control(
                            predicate,
                            snapshot,
                            &row,
                            params,
                            &graph_view.query_access,
                            control,
                        )?
                        .value,
                    )?,
                    None => true,
                };
                if passes {
                    self.matched = true;
                    return Ok(Some(row));
                }
                continue;
            }
            self.stack.pop();
        }
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
) -> QueryResult<()> {
    for row in input {
        let mut cursor = StepCursor::new(step.clone(), row, control)?;
        loop {
            control.work(1)?;
            control.check()?;
            let Some(row) = cursor.next_row(snapshot, graph_view, params, metrics, control)? else {
                break;
            };
            output.push(spill_connection, &row, control)?;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct PartCursor {
    part: PatternPart,
    base: BindingRow,
    start_after: i64,
    start_index_cursor: Option<crate::query::schema::StandardIndexCursor>,
    start_buffer: Vec<i64>,
    start_known_label: Option<LabelId>,
    start_index: usize,
    start_done: bool,
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
        part: PatternPart,
        base: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let base_bytes = expression::binding_row_bytes_controlled(base, Some(control))?;
        let page_bytes = crate::query::resources::checked_add(
            crate::query::resources::allocation_bytes::<RelationshipRecord>(PIPELINE_BATCH)?,
            crate::query::resources::allocation_bytes::<i64>(PIPELINE_BATCH * 2)?,
        )?;
        let memory = control.reserve(crate::query::resources::checked_add(
            base_bytes, page_bytes,
        )?)?;
        Ok(Self {
            part,
            base: base.clone(),
            start_after: 0,
            start_index_cursor: None,
            start_buffer: Vec::new(),
            start_known_label: None,
            start_index: 0,
            start_done: false,
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

    fn reserve_output(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let mut output_bytes = expression::binding_row_bytes_controlled(&self.base, Some(control))?;
        for name in [
            self.part.path_variable.as_ref(),
            self.part.start.variable.as_ref(),
            self.part
                .relationship
                .as_ref()
                .and_then(|spec| spec.variable.as_ref()),
            self.part
                .end
                .as_ref()
                .and_then(|spec| spec.variable.as_ref()),
        ]
        .into_iter()
        .flatten()
        {
            output_bytes = crate::query::resources::checked_add(
                output_bytes,
                crate::query::resources::checked_add(
                    crate::query::resources::map_entry_bytes::<BindingValue>()?,
                    name.capacity() as u64,
                )?,
            )?;
        }
        output_bytes = crate::query::resources::checked_add(output_bytes, 256)?;
        // An undirected index seek can retain both endpoint orientations.
        let output_copies = if self
            .part
            .relationship
            .as_ref()
            .is_some_and(|relationship| relationship.index_seek.is_some())
        {
            2
        } else {
            1
        };
        let output_bytes = crate::query::resources::checked_mul(output_bytes, output_copies)?;
        match &mut self.output_memory {
            Some(memory) => memory.resize(output_bytes)?,
            None => self.output_memory = Some(control.reserve(output_bytes)?),
        }
        Ok(())
    }

    fn next_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        if self.part_is_impossible(graph_view) {
            return Ok(None);
        }
        self.reserve_output(control)?;
        if self
            .part
            .relationship
            .as_ref()
            .is_some_and(|relationship| relationship.index_seek.is_some())
        {
            return self.next_indexed_relationship_row(snapshot, graph_view, metrics, control);
        }
        if self.part.relationship.is_none() {
            let Some(id) = self.next_start(snapshot, graph_view, metrics, control)? else {
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
        self.next_relationship_row(snapshot, graph_view, metrics, control)
    }

    fn next_relationship_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        loop {
            control.work(1)?;
            if self.relationship_index < self.relationships.len() {
                let rel = self.relationships[self.relationship_index];
                self.relationship_index += 1;
                if let Some(row) =
                    self.match_relationship(snapshot, graph_view, rel, metrics, control)?
                {
                    return Ok(Some(row));
                }
                continue;
            }
            if let Some(start) = self.current_start
                && !self.relationship_done
            {
                let page = self.load_relationship_page(
                    snapshot,
                    start,
                    self.relationship_cursor,
                    metrics,
                    control,
                )?;
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
            let Some(start) = self.next_start(snapshot, graph_view, metrics, control)? else {
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
            let Some(spec) = self.part.relationship.as_ref() else {
                return Ok(None);
            };
            let Some(seek) = spec.index_seek.as_ref() else {
                return Ok(None);
            };
            let page = crate::query::schema::scan_relationship_index_page(
                snapshot,
                seek,
                spec.type_id,
                self.indexed_relationship_cursor.as_ref(),
                PIPELINE_BATCH,
                control,
            )?;
            metrics.record_db_hits(page.items.len() as u64);
            self.indexed_relationship_ids = page.items;
            self.indexed_relationship_index = 0;
            if let Some(cursor) = page.next_cursor {
                self.indexed_relationship_cursor = Some(cursor);
            } else {
                self.indexed_relationship_done = true;
            }
        }
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
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
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
        self.next_scanned_start(snapshot, graph_view, metrics, control)
    }

    fn next_scanned_start(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<i64>> {
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
            if let Some(seek) = &self.part.start.index_seek
                && seek.kind != crate::storage::StandardIndexKind::Lookup
            {
                self.start_known_label = self.part.start.scan_label;
                let page = crate::query::schema::scan_node_index_page(
                    snapshot,
                    seek,
                    self.start_index_cursor.as_ref(),
                    PIPELINE_BATCH,
                    control,
                )?;
                metrics.record_db_hits(page.items.len() as u64);
                self.start_buffer = page.items;
                self.start_index = 0;
                self.start_index_cursor = page.next_cursor;
                self.start_done = self.start_index_cursor.is_none();
            } else {
                let scan_label = self
                    .part
                    .start
                    .scan_label
                    .or_else(|| graph_view.scan_label());
                let page = if let Some(label) = scan_label {
                    self.start_known_label = Some(label);
                    snapshot.scan_label_after(label, self.start_after, PIPELINE_BATCH)?
                } else {
                    self.start_known_label = None;
                    snapshot.scan_nodes_after(self.start_after, PIPELINE_BATCH)?
                };
                metrics.record_db_hits(page.items.len() as u64);
                self.start_buffer = page.items;
                self.start_index = 0;
                if let Some(after) = page.next_after {
                    self.start_after = after;
                } else {
                    self.start_done = true;
                }
            }
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
