use super::*;

#[derive(Debug)]
pub(super) struct MemoryProbe {
    pub(super) row: BindingRow,
    pub(super) memory: MemoryReservation,
    pub(super) key: EvaluatedHashKey,
    pub(super) next: Option<usize>,
}

#[derive(Debug)]
pub(super) enum ProbeMode {
    Empty,
    Memory(Box<MemoryProbe>),
    Grace(Box<grace::GraceProbe>),
}

#[derive(Debug)]
pub(in crate::query::stream::match_pipeline) struct HashProbe {
    pub(super) mode: ProbeMode,
    output_memory: Option<MemoryReservation>,
    mode_memory: Option<MemoryReservation>,
}

impl HashProbe {
    pub(in crate::query::stream::match_pipeline) fn empty() -> Self {
        Self {
            mode: ProbeMode::Empty,
            output_memory: None,
            mode_memory: None,
        }
    }

    pub(in crate::query::stream::match_pipeline) fn grace(
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let memory = control.reserve(std::mem::size_of::<grace::GraceProbe>() as u64)?;
        Ok(Self {
            mode: ProbeMode::Grace(Box::new(grace::GraceProbe::new())),
            output_memory: None,
            mode_memory: Some(memory),
        })
    }

    pub(in crate::query::stream::match_pipeline) fn metadata_bytes(&self) -> u64 {
        self.mode_memory
            .as_ref()
            .map_or(0, MemoryReservation::bytes)
    }

    pub(in crate::query::stream::match_pipeline) fn memory(
        input: (BindingRow, MemoryReservation),
        key: EvaluatedHashKey,
        state: &HashBuildState,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let BuildStorage::Memory(table) = &state.storage else {
            return Err(QueryError::internal(
                "in-memory probe lacks its build table",
            ));
        };
        let next = table.head(&key.digest);
        let mode_memory = control.reserve(std::mem::size_of::<MemoryProbe>() as u64)?;
        Ok(Self {
            mode: ProbeMode::Memory(Box::new(MemoryProbe {
                row: input.0,
                memory: input.1,
                key,
                next,
            })),
            output_memory: None,
            mode_memory: Some(mode_memory),
        })
    }

    pub(in crate::query::stream::match_pipeline) fn next_row(
        &mut self,
        state: &mut HashBuildState,
        step: &MatchStep,
        depth: usize,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        self.output_memory = None;
        let plan = step
            .join
            .as_ref()
            .ok_or_else(|| QueryError::internal("Hash probe lacks a plan"))?;
        let part = &step.parts[plan.stages[depth].part];
        let next = match &mut self.mode {
            ProbeMode::Empty => None,
            ProbeMode::Memory(probe) => {
                let BuildStorage::Memory(table) = &state.storage else {
                    return Err(QueryError::internal(
                        "Hash build changed during a memory probe",
                    ));
                };
                next_match(probe, table, part, &mut state.stats, control)?
            }
            ProbeMode::Grace(probe) => {
                probe.next_row(state, step, depth, scope, metrics, control)?
            }
        };
        if let ProbeMode::Memory(probe) = &self.mode {
            let BuildStorage::Memory(table) = &state.storage else {
                return Err(QueryError::internal(
                    "Hash memory owner changed during probe",
                ));
            };
            let output = next.as_ref().map_or(0, |(_, memory)| memory.bytes());
            let probe_bytes = checked_add(probe.memory.bytes(), probe.key.memory.bytes())?;
            let retained = checked_add(table.owned_bytes(), checked_add(probe_bytes, output)?)?;
            state.stats.memory_peak_bytes = state.stats.memory_peak_bytes.max(retained);
        }
        state.publish(metrics, hash_slot(metrics, plan, depth))?;
        match next {
            Some((row, memory)) => {
                self.output_memory = Some(memory);
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }

    pub(in crate::query::stream::match_pipeline) fn split_output_memory(
        &mut self,
        row: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<MemoryReservation> {
        let bytes = expression::binding_row_bytes_controlled(row, Some(control))?;
        self.output_memory
            .as_mut()
            .ok_or_else(|| QueryError::internal("Hash row has no reservation"))?
            .split(bytes)
    }
}

pub(super) fn next_match(
    probe: &mut MemoryProbe,
    table: &HashTable,
    part: &PatternPart,
    stats: &mut HashJoinRuntimeMetrics,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(BindingRow, MemoryReservation)>> {
    while let Some(index) = probe.next {
        control.work(1)?;
        let entry = table
            .entry(index)
            .ok_or_else(|| QueryError::internal("Hash bucket has an invalid entry"))?;
        probe.next = entry.next;
        if probe.key.digest != entry.key.digest {
            continue;
        }
        match hash_keys_equal(&probe.key.value, &entry.key.value, control)? {
            Some(true) => {
                if let Some(row) = merge_rows(&probe.row, &entry.row, part, control)? {
                    return Ok(Some(row));
                }
            }
            Some(false) => stats.hash_collisions = stats.hash_collisions.saturating_add(1),
            None => {}
        }
    }
    Ok(None)
}

fn merge_rows(
    prefix: &BindingRow,
    inner: &BindingRow,
    part: &PatternPart,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(BindingRow, MemoryReservation)>> {
    if !compatible_relationships(prefix, inner, part, control)? {
        return Ok(None);
    }
    let bytes = merged_row_bytes(prefix, inner, control)?;
    let memory = control.reserve(bytes)?;
    let mut row = expression::clone_binding_row_unreserved_controlled(prefix, Some(control))?;
    for name in &inner.order {
        control.work(1)?;
        match inner.values.get(name) {
            Some(BindingValue::Node(id)) => {
                let Some(bound) = bind_node(row, Some(name), *id)? else {
                    return Ok(None);
                };
                row = bound;
            }
            Some(BindingValue::Relationship(rel)) => {
                if !row.bind_relationship(Some(name), *rel)? {
                    return Ok(None);
                }
            }
            _ => {
                return Err(QueryError::internal(
                    "independent Hash build contains a non-pattern binding",
                ));
            }
        }
    }
    row.used_relationships
        .extend(inner.used_relationships.iter().copied());
    Ok(Some((row, memory)))
}

impl HashBuildState {
    pub(in crate::query::stream::match_pipeline) fn probe_key(
        &mut self,
        step: &MatchStep,
        depth: usize,
        row: &BindingRow,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<EvaluatedHashKey>> {
        let plan = step
            .join
            .as_ref()
            .ok_or_else(|| QueryError::internal("Hash probe lacks a plan"))?;
        let crate::query::plan::join::StageAccess::Hash {
            equality,
            inner_side,
        } = plan.stages[depth].access
        else {
            return Err(QueryError::internal("Hash probe received another stage"));
        };
        self.stats.probe_rows = self.stats.probe_rows.saturating_add(1);
        let previous = metrics.activate_profile_slot(hash_slot(metrics, plan, depth));
        let result = extract_hash_key(
            plan.hashes[equality].keys(inner_side).1,
            plan.hashes[equality].family,
            row,
            scope.0,
            scope.1,
            control,
            metrics,
        );
        metrics.restore_access_operator(previous);
        result
    }

    pub(in crate::query::stream::match_pipeline) fn spill_probe(
        &mut self,
        input: (BindingRow, MemoryReservation),
        key: EvaluatedHashKey,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let digest = key.digest;
        drop(key);
        let BuildStorage::Spill(store) = &mut self.storage else {
            return Err(QueryError::internal("Grace probe lacks partition storage"));
        };
        let partition = store.route_digest(&digest)?;
        store.append_reserved(
            partition,
            HashSide::Probe,
            digest,
            input.0,
            input.1,
            control,
        )?;
        Ok(())
    }
}

fn merged_row_bytes(
    prefix: &BindingRow,
    inner: &BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let bytes = checked_add(
        expression::binding_row_bytes_controlled(prefix, Some(control))?,
        expression::binding_row_bytes_controlled(inner, Some(control))?,
    )?;
    let names = prefix
        .order
        .len()
        .checked_add(inner.order.len())
        .and_then(usize::checked_next_power_of_two)
        .ok_or_else(|| {
            crate::query::resources::memory_error("Hash output order capacity overflow")
        })?;
    let bytes = checked_add(bytes, allocation_bytes::<String>(names.max(4))?)?;
    Ok(bytes)
}

fn compatible_relationships(
    prefix: &BindingRow,
    inner: &BindingRow,
    part: &PatternPart,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    for id in &inner.used_relationships {
        control.work(1)?;
        if prefix.used_relationships.contains(id)
            && !part
                .relationship
                .as_ref()
                .is_some_and(|rel| relationship_already_bound(prefix, rel, *id))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
