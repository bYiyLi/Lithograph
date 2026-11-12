//! One current-base Probe run over the clause-owned Grace store. A partition
//! produces nothing until its complete build fits, or bounded chunk mode wins.

use super::*;
use crate::query::spill::{
    GRACE_MAX_REPARTITION_DEPTH, PartitionId, RepartitionOutcome, SpilledBinding,
};
use probe::{MemoryProbe, next_match};

mod load;
mod reader;

use load::{BuildInput, LoadResult};
use reader::PartitionReader;

#[derive(Debug)]
pub(super) struct GraceProbe {
    after_leaf: Option<PartitionId>,
    leaf: Option<LeafRun>,
    done: bool,
}

#[derive(Debug)]
struct LeafRun {
    partition: PartitionId,
    table: HashTable,
    build: PartitionReader,
    probe: PartitionReader,
    pending: Option<BuildInput>,
    active: Option<MemoryProbe>,
    chunked: bool,
    loaded: bool,
    build_done: bool,
}

impl GraceProbe {
    pub(super) fn new() -> Self {
        Self {
            after_leaf: None,
            leaf: None,
            done: false,
        }
    }

    pub(super) fn next_row(
        &mut self,
        state: &mut HashBuildState,
        step: &MatchStep,
        depth: usize,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<(BindingRow, MemoryReservation)>> {
        if self.done {
            return Ok(None);
        }
        let plan = step
            .join
            .as_ref()
            .ok_or_else(|| QueryError::internal("Grace probe lacks its Join plan"))?;
        let BuildStorage::Spill(store) = &mut state.storage else {
            return Err(QueryError::internal(
                "Grace probe lacks its partition store",
            ));
        };
        self.next_in_store(
            (store, &mut state.stats),
            step,
            (plan, depth),
            scope,
            metrics,
            control,
        )
    }

    fn next_in_store(
        &mut self,
        state: (&mut GracePartitions, &mut HashJoinRuntimeMetrics),
        step: &MatchStep,
        stage: (&JoinPlan, usize),
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<(BindingRow, MemoryReservation)>> {
        let (store, stats) = state;
        let (plan, depth) = stage;
        loop {
            control.check()?;
            if !self.select_leaf(store, control)? {
                return self.finish_run(store, control);
            }
            if !self.prepare_leaf(store, stats, stage, scope, metrics, control)? {
                continue;
            }
            let leaf = self.leaf.as_mut().ok_or_else(missing_leaf)?;
            let part = &step.parts[plan.stages[depth].part];
            if let Some(row) = leaf.next_active(part, stats, control)? {
                leaf.record_resident(stats, store, row.1.bytes())?;
                return Ok(Some(row));
            }
            if leaf.advance_probe(store, plan, depth, scope, metrics, control)? {
                leaf.record_resident(stats, store, 0)?;
                continue;
            }
            self.finish_chunk(control)?;
        }
    }

    fn select_leaf(
        &mut self,
        store: &GracePartitions,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        if self.leaf.is_some() {
            return Ok(true);
        }
        while let Some(partition) = store.next_leaf(self.after_leaf) {
            control.work(1)?;
            let partition_stats = store.stats(partition)?;
            let counts = partition_stats.rows;
            if counts[HashSide::Build as usize] > 0 && counts[HashSide::Probe as usize] > 0 {
                let mut leaf = LeafRun::new(partition, control)?;
                leaf.chunked = partition_stats.depth >= GRACE_MAX_REPARTITION_DEPTH;
                self.leaf = Some(leaf);
                return Ok(true);
            }
            self.after_leaf = Some(partition);
        }
        Ok(false)
    }

    fn prepare_leaf(
        &mut self,
        store: &mut GracePartitions,
        stats: &mut HashJoinRuntimeMetrics,
        stage: (&JoinPlan, usize),
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        let leaf = self.leaf.as_mut().ok_or_else(missing_leaf)?;
        if leaf.loaded {
            return Ok(true);
        }
        let result = leaf.load(store, stats, stage, scope, metrics, control)?;
        if result == LoadResult::Complete || leaf.chunked {
            // Retaining a page's unread decoded rows while probing would spend
            // RAM on rows that can be re-read from TEMP using the yielded seq.
            leaf.build.release_batch();
            leaf.loaded = true;
            return Ok(true);
        }
        self.repartition_leaf(store, control)?;
        Ok(false)
    }

    fn repartition_leaf(
        &mut self,
        store: &mut GracePartitions,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let leaf = self.leaf.take().ok_or_else(missing_leaf)?;
        let partition = leaf.partition;
        // The canonical partition still contains all rows. Free decoded input
        // and partial tables before the bounded repartition key pages allocate.
        drop(leaf);
        let outcome = store.repartition(partition, control)?;
        // Repartition can allocate TEMP pages before the first output or LIMIT.
        // Publish their true high-water mark at this bounded phase boundary.
        store.temp_logical_bytes(control)?;
        match outcome {
            RepartitionOutcome::Children(_) => self.after_leaf = Some(partition),
            RepartitionOutcome::Skew(partition) => {
                let mut leaf = LeafRun::new(partition, control)?;
                leaf.chunked = true;
                self.leaf = Some(leaf);
            }
        }
        Ok(())
    }

    fn finish_chunk(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let leaf = self.leaf.as_mut().ok_or_else(missing_leaf)?;
        if leaf.build_done {
            self.after_leaf = Some(leaf.partition);
            self.leaf = None;
        } else {
            leaf.table = HashTable::new(control)?;
            leaf.probe.reset();
            leaf.loaded = false;
        }
        Ok(())
    }

    fn finish_run(
        &mut self,
        store: &mut GracePartitions,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<(BindingRow, MemoryReservation)>> {
        store.retire_probe_run(control)?;
        store.temp_logical_bytes(control)?;
        self.done = true;
        Ok(None)
    }
}

impl LeafRun {
    fn new(partition: PartitionId, control: &ExecutionControl<'_>) -> QueryResult<Self> {
        Ok(Self {
            partition,
            table: HashTable::new(control)?,
            build: PartitionReader::new(HashSide::Build),
            probe: PartitionReader::new(HashSide::Probe),
            pending: None,
            active: None,
            chunked: false,
            loaded: false,
            build_done: false,
        })
    }

    fn next_active(
        &mut self,
        part: &PatternPart,
        stats: &mut HashJoinRuntimeMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<(BindingRow, MemoryReservation)>> {
        let Some(probe) = &mut self.active else {
            return Ok(None);
        };
        if let Some(row) = next_match(probe, &self.table, part, stats, control)? {
            return Ok(Some(row));
        }
        self.active = None;
        Ok(None)
    }

    fn advance_probe(
        &mut self,
        store: &mut GracePartitions,
        plan: &JoinPlan,
        depth: usize,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        let Some(record) = self.probe.next(store, self.partition, control)? else {
            return Ok(false);
        };
        let key = load::stored_key(
            &record,
            HashSide::Probe,
            plan,
            depth,
            scope,
            metrics,
            control,
        )?;
        self.active = Some(MemoryProbe {
            next: self.table.head(&key.digest),
            row: record.row,
            memory: record.memory,
            key,
        });
        Ok(true)
    }

    fn resident_bytes(&self) -> QueryResult<u64> {
        let readers = checked_add(self.build.resident_bytes()?, self.probe.resident_bytes()?)?;
        let pending = self
            .pending
            .as_ref()
            .map_or(Ok(0), BuildInput::resident_bytes)?;
        let active = self.active.as_ref().map_or(Ok(0), |probe| {
            checked_add(probe.memory.bytes(), probe.key.memory.bytes())
        })?;
        checked_add(
            checked_add(self.table.owned_bytes(), readers)?,
            checked_add(pending, active)?,
        )
    }

    fn record_resident(
        &self,
        stats: &mut HashJoinRuntimeMetrics,
        store: &GracePartitions,
        additional: u64,
    ) -> QueryResult<()> {
        let bytes = checked_add(
            checked_add(self.resident_bytes()?, store.resident_bytes()?)?,
            additional,
        )?;
        stats.memory_peak_bytes = stats.memory_peak_bytes.max(bytes);
        Ok(())
    }
}

fn missing_leaf() -> QueryError {
    QueryError::internal("Grace probe lost its active leaf")
}
