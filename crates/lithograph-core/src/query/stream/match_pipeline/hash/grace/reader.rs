use super::*;
use crate::query::spill::ReservedSpillBatch;

#[derive(Debug)]
pub(super) struct PartitionReader {
    // Keep the owning iterator (and its Vec lease) until its remaining rows are
    // consumed or deliberately discarded for a later TEMP re-read.
    batch: Option<ReservedSpillBatch>,
    side: HashSide,
    after: i64,
    done: bool,
}

impl PartitionReader {
    pub(super) fn new(side: HashSide) -> Self {
        Self {
            batch: None,
            side,
            after: -1,
            done: false,
        }
    }

    pub(super) fn next(
        &mut self,
        store: &mut GracePartitions,
        partition: PartitionId,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<SpilledBinding>> {
        loop {
            control.work(1)?;
            if let Some(row) = self.batch.as_mut().and_then(Iterator::next) {
                if row.seq <= self.after {
                    return Err(QueryError::internal(
                        "Grace reader sequence moved backwards",
                    ));
                }
                self.after = row.seq;
                return Ok(Some(row));
            }
            if let Some(batch) = &self.batch
                && batch.after != self.after
            {
                return Err(QueryError::internal(
                    "Grace exhausted page cursor is inconsistent",
                ));
            }
            self.release_batch();
            if self.done {
                return Ok(None);
            }
            let batch = store.read_batch_reserved(
                partition,
                self.side,
                self.after,
                spill_limit(control),
                control,
            )?;
            self.done = batch.len() == 0;
            self.batch = Some(batch);
        }
    }

    pub(super) fn release_batch(&mut self) {
        // `after` is the last yielded record, not the page's last decoded seq.
        // Discarding unread records therefore never skips them on the next read.
        self.batch = None;
    }

    pub(super) fn reset(&mut self) {
        self.release_batch();
        self.after = -1;
        self.done = false;
    }

    pub(super) fn resident_bytes(&self) -> QueryResult<u64> {
        self.batch
            .as_ref()
            .map_or(Ok(0), ReservedSpillBatch::resident_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryErrorKind;

    fn seed(control: &ExecutionControl<'_>) -> GracePartitions {
        let mut store = GracePartitions::create(control.resources().clone(), control).unwrap();
        for _ in 0..7 {
            let row = BindingRow::default();
            let memory = control
                .reserve(expression::binding_row_bytes(&row).unwrap())
                .unwrap();
            store
                .append_reserved(
                    PartitionId::ROOT,
                    HashSide::Probe,
                    [0; 32],
                    row,
                    memory,
                    control,
                )
                .unwrap();
        }
        store
    }

    #[test]
    fn discarded_page_resumes_after_yielded_seq_and_reset_replays_the_leaf() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let mut store = seed(&control);
        let baseline = resources.used();
        let mut reader = PartitionReader::new(HashSide::Probe);
        let first = reader
            .next(&mut store, PartitionId::ROOT, &control)
            .unwrap()
            .unwrap();
        assert_eq!(first.seq, 0);
        assert_eq!(
            resources.used(),
            baseline + first.memory.bytes() + reader.resident_bytes().unwrap()
        );
        reader.release_batch();
        assert_eq!(resources.used(), baseline + first.memory.bytes());
        drop(first);
        assert_eq!(resources.used(), baseline);

        for expected in 1..7 {
            let row = reader
                .next(&mut store, PartitionId::ROOT, &control)
                .unwrap()
                .unwrap();
            assert_eq!(row.seq, expected);
        }
        assert!(
            reader
                .next(&mut store, PartitionId::ROOT, &control)
                .unwrap()
                .is_none()
        );
        assert_eq!(reader.resident_bytes().unwrap(), 0);
        for _ in 0..2 {
            reader.reset();
            for expected in 0..7 {
                let row = reader
                    .next(&mut store, PartitionId::ROOT, &control)
                    .unwrap()
                    .unwrap();
                assert_eq!(row.seq, expected);
            }
            assert!(
                reader
                    .next(&mut store, PartitionId::ROOT, &control)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(resources.used(), baseline);
        }
        drop(store);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn page_budget_refusal_does_not_skip_the_next_unread_record() {
        let resources = QueryResources::new(Some(64 * 1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut store = seed(&control);
        let mut reader = PartitionReader::new(HashSide::Probe);
        let first = reader
            .next(&mut store, PartitionId::ROOT, &control)
            .unwrap()
            .unwrap();
        assert_eq!(first.seq, 0);
        drop(first);
        reader.release_batch();
        let baseline = resources.used();
        let blocker = control
            .reserve(resources.limit().unwrap() - baseline)
            .unwrap();

        let error = reader
            .next(&mut store, PartitionId::ROOT, &control)
            .unwrap_err();

        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(reader.after, 0);
        assert_eq!(reader.resident_bytes().unwrap(), 0);
        drop(blocker);
        assert_eq!(resources.used(), baseline);
        let next = reader
            .next(&mut store, PartitionId::ROOT, &control)
            .unwrap()
            .unwrap();
        assert_eq!(next.seq, 1);
        drop(next);
        reader.release_batch();
        drop(store);
        assert_eq!(resources.used(), 0);
    }
}
