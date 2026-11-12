use super::*;

struct Page {
    rows: Vec<SpilledBinding>,
    memory: MemoryReservation,
    bytes: u64,
    after: i64,
    target: u64,
}

pub(super) fn read_rows(
    mut rows: rusqlite::Rows<'_>,
    after: i64,
    target: u64,
    resources: &QueryResources,
    control: &ExecutionControl<'_>,
    budget_refused: &mut bool,
) -> QueryResult<ReservedSpillBatch> {
    let mut page = Page {
        rows: Vec::new(),
        memory: resources.reserve(0)?,
        bytes: 0,
        after,
        target,
    };
    let mut local_error = |error: &QueryError| {
        *budget_refused = error.kind == QueryErrorKind::Resource
            && error.sqlite_code == Some(rusqlite::ffi::SQLITE_TOOBIG);
    };
    while let Some(row) = rows.next()? {
        let Some(item) = page
            .decode(row, resources, control)
            .inspect_err(&mut local_error)?
        else {
            break;
        };
        if !page.push(item).inspect_err(&mut local_error)?
            || checked_add(page.bytes, page.memory.bytes()).inspect_err(&mut local_error)? >= target
        {
            break;
        }
    }
    control.check()?;
    Ok(ReservedSpillBatch {
        rows: page.rows.into_iter(),
        after: page.after,
        _memory: page.memory,
    })
}

impl Page {
    fn decode(
        &self,
        row: &rusqlite::Row<'_>,
        resources: &QueryResources,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<SpilledBinding>> {
        control.work(1)?;
        let seq = row.get::<_, i64>(0)?;
        if seq <= self.after {
            return Err(QueryError::internal(
                "Grace page sequence is not increasing",
            ));
        }
        let digest = io::read_digest(row, 1)?;
        let allowance = decoding_bytes(sqlite_text(row, 2)?, control)?;
        if !self.rows.is_empty() && allowance > self.target.saturating_sub(self.bytes) {
            return Ok(None);
        }
        let decoded = decode_binding_record(row, 2, resources, control);
        let (binding, memory) = match decoded {
            Err(error) if error.kind == QueryErrorKind::Resource && !self.rows.is_empty() => {
                return Ok(None);
            }
            result => result?,
        };
        Ok(Some(SpilledBinding {
            seq,
            digest,
            row: binding,
            memory,
        }))
    }

    fn push(&mut self, item: SpilledBinding) -> QueryResult<bool> {
        let bytes = checked_add(self.bytes, item.memory.bytes())?;
        let resident = checked_add(bytes, self.capacity_bytes()?)?;
        if !self.rows.is_empty() && resident > self.target {
            return Ok(false);
        }
        if let Err(error) = reserve_vec_slot(&mut self.rows, &mut self.memory) {
            if error.kind == QueryErrorKind::Resource && !self.rows.is_empty() {
                return Ok(false);
            }
            return Err(error);
        }
        self.bytes = bytes;
        self.after = item.seq;
        self.rows.push(item);
        Ok(true)
    }

    fn capacity_bytes(&self) -> QueryResult<u64> {
        if self.rows.len() < self.rows.capacity() {
            return Ok(self.memory.bytes());
        }
        let capacity = self.rows.capacity().max(2).checked_mul(2).ok_or_else(|| {
            super::super::super::resources::memory_error("Grace page capacity overflow")
        })?;
        checked_add(
            self.memory.bytes(),
            allocation_bytes::<SpilledBinding>(capacity - self.rows.capacity())?,
        )
    }
}
