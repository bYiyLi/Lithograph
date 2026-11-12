use super::*;

#[cfg(test)]
mod tests;

impl HashBuildState {
    pub(in crate::query::stream::match_pipeline) fn ensure_build(
        &mut self,
        step: &MatchStep,
        depth: usize,
        snapshot: &Snapshot<'_>,
        view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self.built {
            return Ok(());
        }
        let (plan, equality, inner_side, dynamic_filter) = hash_build_stage(step, depth)?;
        let (ordinal, part) = crate::query::plan::join::execution_part(step, depth);
        let mut source = PartCursor::new(&part, &BindingRow::default(), control)?;
        source.profile = metrics.match_profile_slots(plan.clause_start, ordinal);
        let bindings = MatchReadBindings::default();
        self.storage = BuildStorage::Memory(HashTable::new(control)?);
        if dynamic_filter {
            self.filter = dynamic_filter::KeyCollector::new(control)?;
        }
        // Keep row before its claim in one tuple through NULL/error exits.
        while let Some(input) =
            pull_build_row(&mut source, snapshot, view, metrics, &bindings, control)?
        {
            let slot = hash_slot(metrics, plan, depth);
            self.consume_build_input(
                input,
                (
                    plan.hashes[equality].keys(inner_side).0,
                    plan.hashes[equality].family,
                ),
                (snapshot, view),
                slot,
                metrics,
                control,
            )?;
        }
        if let BuildStorage::Spill(store) = &mut self.storage {
            store.temp_logical_bytes(control)?;
        }
        self.built = true;
        self.publish(metrics, hash_slot(metrics, plan, depth))
    }

    /// The complete ASP ID filter is produced before this call. Every page is
    /// consumed by the real finite Node reader; only the final source EOF can
    /// make the Hash build available for probing.
    pub(in crate::query::stream) fn ensure_build_from_id_pages(
        &mut self,
        step: &MatchStep,
        depth: usize,
        next_page: &mut dyn FnMut() -> QueryResult<Option<(Vec<i64>, MemoryReservation)>>,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self.built {
            return Ok(());
        }
        let (plan, equality, _, dynamic_filter) = hash_build_stage(step, depth)?;
        if plan.hashes[equality].family != HashFamily::NodeIdentity || dynamic_filter {
            return Err(QueryError::internal(
                "finite Hash build requires a plain Node identity stage",
            ));
        }
        self.storage = BuildStorage::Memory(HashTable::new(control)?);
        self.filter = None;
        loop {
            control.work(1)?;
            control.check()?;
            let Some(ids) = next_page()? else { break };
            self.consume_id_page(step, depth, ids, scope, metrics, control)?;
        }
        if let BuildStorage::Spill(store) = &mut self.storage {
            store.temp_logical_bytes(control)?;
        }
        self.built = true;
        self.publish(metrics, hash_slot(metrics, plan, depth))
    }

    fn consume_id_page(
        &mut self,
        step: &MatchStep,
        depth: usize,
        ids: (Vec<i64>, MemoryReservation),
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let (plan, equality, inner_side, _) = hash_build_stage(step, depth)?;
        let (ordinal, part) = crate::query::plan::join::execution_part(step, depth);
        let source_step = MatchStep {
            clause_start: plan.clause_start,
            optional: false,
            parts: vec![part],
            predicate: None,
            early_predicates: Vec::new(),
            join: None,
        };
        let mut source =
            StepCursor::with_pattern_node_ids(&source_step, BindingRow::default(), ids, control)?;
        source.set_profile_parts(&[ordinal], control)?;
        let mut bindings = MatchReadBindings::default();
        let params = std::collections::BTreeMap::new();
        let node_slot = metrics
            .match_profile_slots(plan.clause_start, ordinal)
            .and_then(|slots| slots.slot(ProfileRole::NodeAccess));
        loop {
            let previous = metrics.activate_profile_slot(node_slot);
            let result =
                source.next_row(scope.0, scope.1, &params, metrics, &mut bindings, control);
            metrics.restore_access_operator(previous);
            let Some(row) = result? else { break };
            let memory = source.take_output_memory(&row, control)?;
            self.consume_build_input(
                (row, memory),
                (
                    plan.hashes[equality].keys(inner_side).0,
                    plan.hashes[equality].family,
                ),
                scope,
                hash_slot(metrics, plan, depth),
                metrics,
                control,
            )?;
        }
        Ok(())
    }

    fn consume_build_input(
        &mut self,
        input: (BindingRow, MemoryReservation),
        descriptor: (&crate::query::plan::join::HashInputKey, HashFamily),
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        slot: Option<usize>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        self.stats.build_rows = self.stats.build_rows.saturating_add(1);
        let previous = metrics.activate_profile_slot(slot);
        let extracted = extract_hash_key_admitted(
            descriptor.0,
            descriptor.1,
            &input.0,
            scope,
            control,
            metrics,
            &mut |bytes| self.admit_key_allocation(bytes, control),
        );
        metrics.restore_access_operator(previous);
        if let Some(key) = extracted? {
            self.collect_filter_key(&key, control)?;
            self.keep_build_row(input, key, control)?;
        }
        self.publish(metrics, slot)
    }

    fn collect_filter_key(
        &mut self,
        key: &EvaluatedHashKey,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let Some(filter) = &mut self.filter else {
            return Ok(());
        };
        let complete = filter.observe(key, control)?;
        let peak = filter.last_peak_bytes();
        self.stats.memory_peak_bytes = self
            .stats
            .memory_peak_bytes
            .max(checked_add(self.storage_bytes()?, peak)?);
        if !complete {
            // An incomplete advisory set can never exclude a Probe.
            self.filter = None;
        }
        Ok(())
    }

    fn keep_build_row(
        &mut self,
        input: (BindingRow, MemoryReservation),
        key: EvaluatedHashKey,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let incoming = checked_add(input.1.bytes(), key.memory.bytes())?;
        if let BuildStorage::Memory(table) = &self.storage {
            let headroom = row_headroom(input.1.bytes())?;
            let projected = table.projected_bytes(input.1.bytes(), key.memory.bytes())?;
            let target =
                hash_resident_target(control.resources(), table.owned_bytes(), incoming, headroom);
            if projected > target
                && !self.discard_filter_to_keep_memory(projected, incoming, headroom, control)?
            {
                self.spill_build(control)?;
            }
        }
        match &mut self.storage {
            BuildStorage::Memory(table) => {
                table.insert_reserved(input.0, input.1, key, control)?;
                let keys = self
                    .filter
                    .as_ref()
                    .map_or(0, |filter| filter.owned_bytes());
                self.stats.memory_peak_bytes = self
                    .stats
                    .memory_peak_bytes
                    .max(checked_add(table.peak_bytes(), keys)?);
                Ok(())
            }
            BuildStorage::Spill(store) => {
                let digest = key.digest;
                drop(key);
                let partition = store.route_digest(&digest)?;
                store.append_reserved(
                    partition,
                    HashSide::Build,
                    digest,
                    input.0,
                    input.1,
                    control,
                )?;
                Ok(())
            }
            BuildStorage::Empty => Err(QueryError::internal("Hash build storage is missing")),
        }
    }

    fn discard_filter_to_keep_memory(
        &mut self,
        projected: u64,
        incoming: u64,
        headroom: u64,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        let (BuildStorage::Memory(table), Some(filter)) = (&self.storage, &self.filter) else {
            return Ok(false);
        };
        // The optional allocation may not be the sole reason a formerly
        // resident Hash requires TEMP. Subtract precisely this existing claim
        // from outside memory, then drop it before using the recovered grant.
        let without_filter = hash_resident_target(
            control.resources(),
            checked_add(table.owned_bytes(), filter.owned_bytes())?,
            incoming,
            headroom,
        );
        if projected <= without_filter {
            self.filter = None;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(super) fn spill_build(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let BuildStorage::Memory(table) = std::mem::replace(&mut self.storage, BuildStorage::Empty)
        else {
            return Err(QueryError::internal(
                "Hash build cannot transition to spill twice",
            ));
        };
        let mut store = GracePartitions::create(control.resources().clone(), control)?;
        self.stats.memory_peak_bytes = self.stats.memory_peak_bytes.max(checked_add(
            checked_add(table.owned_bytes(), store.resident_bytes()?)?,
            self.filter
                .as_ref()
                .map_or(0, |filter| filter.owned_bytes()),
        )?);
        for mut entry in table.flush() {
            control.work(1)?;
            let digest = entry.key.digest;
            drop(entry.key);
            let partition = store.route_digest(&digest)?;
            entry
                .row_memory
                .grow(std::mem::size_of::<BindingRow>() as u64)?;
            store.append_reserved(
                partition,
                HashSide::Build,
                digest,
                entry.row,
                entry.row_memory,
                control,
            )?;
        }
        self.storage = BuildStorage::Spill(Box::new(store));
        Ok(())
    }
}

fn hash_build_stage(
    step: &MatchStep,
    depth: usize,
) -> QueryResult<(&JoinPlan, usize, HashOperandSide, bool)> {
    let plan = step
        .join
        .as_ref()
        .ok_or_else(|| QueryError::internal("Hash lacks its Join plan"))?;
    let crate::query::plan::join::StageAccess::Hash {
        equality,
        inner_side,
        dynamic_filter,
    } = plan.stages[depth].access
    else {
        return Err(QueryError::internal(
            "Hash build requested for another access",
        ));
    };
    Ok((plan, equality, inner_side, dynamic_filter.is_some()))
}

fn pull_build_row(
    source: &mut PartCursor,
    snapshot: &Snapshot<'_>,
    view: &ResolvedGraphView,
    metrics: &mut QueryMetrics,
    bindings: &MatchReadBindings,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(BindingRow, MemoryReservation)>> {
    control.check()?;
    let previous = source.activate_frame(metrics);
    let result = source.next_row(snapshot, view, metrics, bindings, control);
    if matches!(result, Ok(Some(_))) && source.part.relationship.is_some() {
        metrics.record_profile_rows(
            source
                .profile
                .and_then(|slots| slots.slot(ProfileRole::RelationshipAccess)),
            1,
        );
    }
    metrics.restore_access_operator(previous);
    let Some(row) = result? else { return Ok(None) };
    let memory = source.split_output_memory(&row, control)?;
    Ok(Some((row, memory)))
}
