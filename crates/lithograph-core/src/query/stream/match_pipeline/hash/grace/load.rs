use super::*;
use crate::query::QueryErrorKind;

#[derive(Debug)]
pub(super) struct BuildInput {
    record: SpilledBinding,
    key: Option<EvaluatedHashKey>,
}

impl BuildInput {
    pub(super) fn resident_bytes(&self) -> QueryResult<u64> {
        checked_add(
            self.record.memory.bytes(),
            self.key.as_ref().map_or(0, |key| key.memory.bytes()),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LoadResult {
    Complete,
    Overflow,
}

impl LeafRun {
    pub(super) fn load(
        &mut self,
        store: &mut GracePartitions,
        stats: &mut HashJoinRuntimeMetrics,
        stage: (&JoinPlan, usize),
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<LoadResult> {
        let (plan, depth) = stage;
        loop {
            control.work(1)?;
            if !self.fill_pending(store, control)? {
                self.build_done = true;
                return Ok(LoadResult::Complete);
            }
            self.record_resident(stats, store, 0)?;
            if !self.extract_pending(plan, depth, scope, metrics, control)? {
                return Ok(LoadResult::Overflow);
            }
            self.record_resident(stats, store, 0)?;
            if !self.keep_pending(stats, store, control)? {
                return Ok(LoadResult::Overflow);
            }
        }
    }

    fn fill_pending(
        &mut self,
        store: &mut GracePartitions,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        if self.pending.is_some() {
            return Ok(true);
        }
        match self.build.next(store, self.partition, control) {
            Ok(Some(record)) => {
                self.pending = Some(BuildInput { record, key: None });
                Ok(true)
            }
            Ok(None) => Ok(false),
            // Retrying the same unread page is safe after a chunk is released.
            Err(error)
                if error.kind == QueryErrorKind::Resource
                    && self.table.len() > 0
                    && store.stats(self.partition).is_ok() =>
            {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }

    fn extract_pending(
        &mut self,
        plan: &JoinPlan,
        depth: usize,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        let Some(input) = &mut self.pending else {
            return Ok(false);
        };
        if input.key.is_some() {
            return Ok(true);
        }
        match stored_key(
            &input.record,
            HashSide::Build,
            plan,
            depth,
            scope,
            metrics,
            control,
        ) {
            Ok(key) => {
                input.key = Some(key);
                Ok(true)
            }
            Err(error) if error.kind == QueryErrorKind::Resource && self.table.len() > 0 => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn keep_pending(
        &mut self,
        stats: &mut HashJoinRuntimeMetrics,
        store: &GracePartitions,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        let input = self.pending.as_ref().ok_or_else(missing_input)?;
        let key = input.key.as_ref().ok_or_else(missing_key)?;
        let incoming = checked_add(input.record.memory.bytes(), key.memory.bytes())?;
        let target = hash_resident_target(
            control.resources(),
            self.table.owned_bytes(),
            incoming,
            row_headroom(input.record.memory.bytes())?,
        );
        let projected = self
            .table
            .projected_bytes(input.record.memory.bytes(), key.memory.bytes())?;
        // A soft build target never rejects a legal singleton. The real leases
        // still enforce the query's hard budget before any allocation occurs.
        if self.table.len() > 0 && projected > target {
            return Ok(false);
        }
        let peak = self.growth_peak(projected, incoming, store)?;
        let input = self.pending.take().ok_or_else(missing_input)?;
        let key = input.key.ok_or_else(missing_key)?;
        self.table
            .insert_reserved(input.record.row, input.record.memory, key, control)?;
        stats.memory_peak_bytes = stats.memory_peak_bytes.max(peak);
        Ok(true)
    }

    fn growth_peak(
        &self,
        projected: u64,
        incoming: u64,
        store: &GracePartitions,
    ) -> QueryResult<u64> {
        let table_and_input = checked_add(self.table.owned_bytes(), incoming)?;
        let outside = self
            .resident_bytes()?
            .checked_sub(table_and_input)
            .ok_or_else(|| QueryError::internal("Grace resident claims omit its pending input"))?;
        checked_add(projected, checked_add(outside, store.resident_bytes()?)?)
    }
}

pub(super) fn stored_key(
    record: &SpilledBinding,
    side: HashSide,
    plan: &JoinPlan,
    depth: usize,
    scope: (&Snapshot<'_>, &ResolvedGraphView),
    metrics: &mut QueryMetrics,
    control: &ExecutionControl<'_>,
) -> QueryResult<EvaluatedHashKey> {
    let crate::query::plan::join::StageAccess::Hash {
        equality,
        inner_side,
        ..
    } = plan.stages[depth].access
    else {
        return Err(QueryError::internal("Grace leaf has another access kind"));
    };
    let equality = &plan.hashes[equality];
    let (build, probe) = equality.keys(inner_side);
    let expression = if side == HashSide::Build {
        build
    } else {
        probe
    };
    let previous = metrics.activate_profile_slot(hash_slot(metrics, plan, depth));
    let extracted = extract_hash_key(
        expression,
        equality.family,
        &record.row,
        scope.0,
        scope.1,
        control,
        metrics,
    );
    metrics.restore_access_operator(previous);
    let key = extracted?.ok_or_else(|| QueryError::internal("Grace stored key became NULL"))?;
    if key.digest != record.digest {
        return Err(QueryError::internal(
            "Grace stored digest contradicts its typed key",
        ));
    }
    Ok(key)
}

fn missing_input() -> QueryError {
    QueryError::internal("Grace build lost its pending input")
}

fn missing_key() -> QueryError {
    QueryError::internal("Grace build input has no evaluated key")
}
