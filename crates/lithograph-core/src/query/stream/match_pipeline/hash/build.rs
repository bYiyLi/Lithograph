use super::*;

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
        let plan = step
            .join
            .as_ref()
            .ok_or_else(|| QueryError::internal("Hash lacks its Join plan"))?;
        let crate::query::plan::join::StageAccess::Hash {
            equality,
            inner_side,
        } = plan.stages[depth].access
        else {
            return Err(QueryError::internal(
                "Hash build requested for another access",
            ));
        };
        let (ordinal, part) = crate::query::plan::join::execution_part(step, depth);
        let mut source = PartCursor::new(&part, &BindingRow::default(), control)?;
        source.profile = metrics.match_profile_slots(plan.clause_start, ordinal);
        let bindings = MatchReadBindings::default();
        self.storage = BuildStorage::Memory(HashTable::new(control)?);
        // Keep row before its claim in one tuple through NULL/error exits.
        while let Some(input) =
            pull_build_row(&mut source, snapshot, view, metrics, &bindings, control)?
        {
            self.stats.build_rows = self.stats.build_rows.saturating_add(1);
            let slot = hash_slot(metrics, plan, depth);
            let previous = metrics.activate_profile_slot(slot);
            let extracted = extract_hash_key(
                plan.hashes[equality].keys(inner_side).0,
                plan.hashes[equality].family,
                &input.0,
                snapshot,
                view,
                control,
                metrics,
            );
            metrics.restore_access_operator(previous);
            if let Some(key) = extracted? {
                self.keep_build_row(input, key, control)?;
            }
            self.publish(metrics, slot)?;
        }
        if let BuildStorage::Spill(store) = &mut self.storage {
            store.temp_logical_bytes(control)?;
        }
        self.built = true;
        self.publish(metrics, hash_slot(metrics, plan, depth))
    }

    fn keep_build_row(
        &mut self,
        input: (BindingRow, MemoryReservation),
        key: EvaluatedHashKey,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let incoming = checked_add(input.1.bytes(), key.memory.bytes())?;
        if let BuildStorage::Memory(table) = &self.storage {
            let target = hash_resident_target(
                control.resources(),
                table.owned_bytes(),
                incoming,
                row_headroom(input.1.bytes())?,
            );
            if table.projected_bytes(input.1.bytes(), key.memory.bytes())? > target {
                self.spill_build(control)?;
            }
        }
        match &mut self.storage {
            BuildStorage::Memory(table) => {
                table.insert_reserved(input.0, input.1, key, control)?;
                self.stats.memory_peak_bytes = self.stats.memory_peak_bytes.max(table.peak_bytes());
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

    fn spill_build(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let BuildStorage::Memory(table) = std::mem::replace(&mut self.storage, BuildStorage::Empty)
        else {
            return Err(QueryError::internal(
                "Hash build cannot transition to spill twice",
            ));
        };
        let mut store = GracePartitions::create(control.resources().clone(), control)?;
        self.stats.memory_peak_bytes = self
            .stats
            .memory_peak_bytes
            .max(checked_add(table.owned_bytes(), store.resident_bytes()?)?);
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
