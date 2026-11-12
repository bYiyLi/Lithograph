//! Query-private Grace partitions over the existing typed Binding codec.
//! The clause owner keeps this store; each probe run has its own reader state.

use super::*;
use rusqlite::{CachedStatement, StatementStatus};

mod directory;
mod io;
mod reader;
#[cfg(test)]
mod tests;

use directory::Directory;

pub(in crate::query) const FANOUT: usize = 8;
pub(in crate::query) const MAX_DEPTH: u8 = 3;
/// Conservative value widths for three INTEGER columns and the digest.
/// This excludes SQLite record headers and is not an on-disk record width.
pub(in crate::query) const RECORD_FIXED_VALUE_BYTES: u64 =
    3 * std::mem::size_of::<i64>() as u64 + std::mem::size_of::<[u8; 32]>() as u64;
const MAX_NODES: usize = 585;
const MAX_PAGE_ROWS: usize = 256;
const SQL_CACHE_SLOTS: usize = 8;
const SQL_TEXT_BYTES: usize = 512;
// Each of the eight fixed templates may retain an Arc<str> while its new key
// is allocated. The 256-byte allowance covers the Rust RawStatement wrapper,
// LRU node, hash buckets/growth, Arc headers and a shared guard/free node.
// SQLite's native statement allocation and pager cache are separate.
const SQL_CACHE_BYTES: u64 = SQL_CACHE_SLOTS as u64 * (2 * SQL_TEXT_BYTES as u64 + 256);
const BASE_MEMORY_BYTES: u64 =
    1024 + std::mem::size_of::<GracePartitions>() as u64 + SQL_CACHE_BYTES;

pub(in crate::query) fn resident_bound() -> u64 {
    BASE_MEMORY_BYTES + Directory::resident_bound()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub(in crate::query) enum HashSide {
    Build = 0,
    Probe = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::query) struct PartitionId(u32);

impl PartitionId {
    pub(in crate::query) const ROOT: Self = Self(0);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::query) struct PartitionStats {
    pub(in crate::query) rows: [u64; 2],
    pub(in crate::query) encoded_bytes: [u64; 2],
    pub(in crate::query) depth: u8,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::query) struct SpillReadLimit {
    pub(in crate::query) rows: usize,
    pub(in crate::query) bytes: u64,
}

#[derive(Debug)]
pub(in crate::query) struct SpilledBinding {
    pub(in crate::query) seq: i64,
    pub(in crate::query) digest: [u8; 32],
    pub(in crate::query) row: BindingRow,
    pub(in crate::query) memory: MemoryReservation,
}

#[derive(Debug)]
pub(in crate::query) struct ReservedSpillBatch {
    rows: std::vec::IntoIter<SpilledBinding>,
    pub(in crate::query) after: i64,
    _memory: MemoryReservation,
}

impl ReservedSpillBatch {
    pub(in crate::query) fn resident_bytes(&self) -> QueryResult<u64> {
        self.rows
            .as_slice()
            .iter()
            .try_fold(self._memory.bytes(), |bytes, row| {
                checked_add(bytes, row.memory.bytes())
            })
    }
}

impl Iterator for ReservedSpillBatch {
    type Item = SpilledBinding;

    fn next(&mut self) -> Option<Self::Item> {
        self.rows.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rows.size_hint()
    }
}

impl ExactSizeIterator for ReservedSpillBatch {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::query) enum RepartitionOutcome {
    Children([PartitionId; FANOUT]),
    Skew(PartitionId),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::query) struct GraceCounters {
    pub(in crate::query) written_rows: [u64; 2],
    pub(in crate::query) read_rows: [u64; 2],
    pub(in crate::query) written_bytes: [u64; 2],
    pub(in crate::query) read_batches: u64,
    pub(in crate::query) repartition_batches: u64,
    pub(in crate::query) repartition_rows: u64,
    pub(in crate::query) retired_probe_rows: u64,
    pub(in crate::query) retired_probe_batches: u64,
    pub(in crate::query) partitions: u64,
    pub(in crate::query) max_depth: u8,
    pub(in crate::query) sqlite_vm_steps: u64,
    pub(in crate::query) sqlite_fullscan_steps: u64,
    pub(in crate::query) sqlite_sorts: u64,
    /// Logical TEMP allocation, not proof of file bytes or disk residency.
    pub(in crate::query) temp_logical_bytes_peak: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct SqlCounts {
    vm: u64,
    scans: u64,
    sorts: u64,
}

impl SqlCounts {
    fn take(statement: &rusqlite::Statement<'_>) -> QueryResult<Self> {
        let status = |kind| {
            u64::try_from(statement.reset_status(kind)).map_err(|_| {
                super::super::resources::memory_error("private spill SQLite counter overflow")
            })
        };
        Ok(Self {
            vm: status(StatementStatus::VmStep)?,
            scans: status(StatementStatus::FullscanStep)?,
            sorts: status(StatementStatus::Sort)?,
        })
    }
}

#[derive(Debug)]
pub(in crate::query) struct GracePartitions {
    connection: Option<Connection>,
    table: String,
    resources: QueryResources,
    directory: Directory,
    _memory: MemoryReservation,
    sequence: i64,
    counters: GraceCounters,
}

impl GracePartitions {
    pub(in crate::query) fn create(
        resources: QueryResources,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        control.check()?;
        // Includes the table name and one bounded SQL-template scratch string.
        let mut memory = resources.reserve(BASE_MEMORY_BYTES)?;
        memory.merge(control.reserve(0)?)?;
        let directory = Directory::new(&resources)?;
        let connection = open_spill_connection().map_err(private_temp_error)?;
        connection.set_prepared_statement_cache_capacity(SQL_CACHE_SLOTS);
        let table = spill_table("grace");
        let mut store = Self {
            connection: Some(connection),
            table,
            resources,
            directory,
            _memory: memory,
            sequence: 0,
            counters: GraceCounters {
                partitions: 1,
                ..GraceCounters::default()
            },
        };
        // This connection belongs only to the Grace owner. Close destroys its
        // scratch state and rolls this transaction back; the host is separate.
        store.sql("BEGIN", |statement| {
            statement.execute([])?;
            Ok(())
        })?;
        let sql = format!(
            "CREATE TEMP TABLE temp.{}(partition INTEGER NOT NULL,side INTEGER NOT NULL,\
             seq INTEGER NOT NULL,digest BLOB NOT NULL,binding_json TEXT NOT NULL,\
             PRIMARY KEY(partition,side,seq)) WITHOUT ROWID",
            store.table
        );
        store.sql(&sql, |statement| {
            statement.execute([])?;
            Ok(())
        })?;
        control.check()?;
        Ok(store)
    }

    pub(in crate::query) fn append_reserved(
        &mut self,
        partition: PartitionId,
        side: HashSide,
        digest: [u8; 32],
        row: BindingRow,
        memory: MemoryReservation,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<i64> {
        // Field drop order keeps the live row covered even on an early error.
        let mut input = SpilledBinding {
            seq: 0,
            digest,
            row,
            memory,
        };
        let result = (|| {
            input.memory.merge(self.resources.reserve(0)?)?;
            self.append(partition, side, &input, control)
        })();
        drop(input);
        self.close_on_error(result)
    }

    fn append(
        &mut self,
        partition: PartitionId,
        side: HashSide,
        input: &SpilledBinding,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<i64> {
        control.check()?;
        if self.route_digest(&input.digest)? != partition {
            return Err(QueryError::internal(
                "spill append targets a retired or wrong leaf",
            ));
        }
        let next = self.sequence.checked_add(1).ok_or_else(|| {
            super::super::resources::memory_error("Grace spill sequence overflow")
        })?;
        let payload = binding_row_bytes(&input.row, control)?;
        if input.memory.bytes() < payload {
            return Err(QueryError::internal(
                "Grace input row is not fully reserved",
            ));
        }
        let _encoding = self.resources.reserve(serialization_bytes(payload)?)?;
        let encoded = encode_binding_row(&input.row, control)?;
        let seq = self.sequence;
        self.insert_row(partition, side, input, &encoded)?;
        control.check()?;
        let bytes = encoded.len() as u64;
        self.directory.add(partition, side, bytes)?;
        self.record_append(side, bytes)?;
        self.sequence = next;
        Ok(seq)
    }

    fn insert_row(
        &mut self,
        partition: PartitionId,
        side: HashSide,
        input: &SpilledBinding,
        encoded: &str,
    ) -> QueryResult<()> {
        let sql = format!("INSERT INTO temp.{} VALUES(?1,?2,?3,?4,?5)", self.table);
        let seq = self.sequence;
        self.sql(&sql, |statement| {
            statement.execute(params![
                partition.0,
                side as i64,
                seq,
                input.digest.as_slice(),
                encoded
            ])?;
            Ok(())
        })
    }

    fn record_append(&mut self, side: HashSide, bytes: u64) -> QueryResult<()> {
        let index = side as usize;
        self.counters.written_rows[index] = checked_add(self.counters.written_rows[index], 1)?;
        self.counters.written_bytes[index] =
            checked_add(self.counters.written_bytes[index], bytes)?;
        Ok(())
    }

    pub(in crate::query) fn read_batch_reserved(
        &mut self,
        partition: PartitionId,
        side: HashSide,
        after: i64,
        limit: SpillReadLimit,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<ReservedSpillBatch> {
        let mut budget_refused = false;
        let result = io::read_page(
            self,
            partition,
            side,
            after,
            limit,
            control,
            &mut budget_refused,
        );
        // Only a known local reservation refusal leaves a retriable read.
        // A SQLite failure may have rolled the entire private transaction back.
        match result {
            Err(error)
                if budget_refused
                    && self
                        .connection
                        .as_ref()
                        .is_some_and(|connection| !connection.is_autocommit()) =>
            {
                Err(error)
            }
            result => self.close_on_error(result),
        }
    }

    pub(in crate::query) fn repartition(
        &mut self,
        partition: PartitionId,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<RepartitionOutcome> {
        let result = io::repartition(self, partition, control);
        self.close_on_error(result)
    }

    pub(in crate::query) fn route_digest(&self, digest: &[u8; 32]) -> QueryResult<PartitionId> {
        self.connection()?;
        self.directory.route(digest)
    }

    pub(in crate::query) fn stats(&self, partition: PartitionId) -> QueryResult<PartitionStats> {
        self.connection()?;
        self.directory.stats(partition)
    }

    pub(in crate::query) fn next_leaf(&self, after: Option<PartitionId>) -> Option<PartitionId> {
        self.directory.next_leaf(after)
    }

    pub(in crate::query) fn retire_probe_run(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let result = io::retire_probe(self, control);
        self.close_on_error(result)
    }

    pub(in crate::query) fn counters(&self) -> GraceCounters {
        self.counters
    }

    pub(in crate::query) fn resident_bytes(&self) -> QueryResult<u64> {
        checked_add(self._memory.bytes(), self.directory.resident_bytes())
    }

    pub(in crate::query) fn temp_logical_bytes(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<u64> {
        control.check()?;
        let pages: i64 = self.sql("PRAGMA temp.page_count", |s| {
            Ok(s.query_row([], |r| r.get(0))?)
        })?;
        let size: i64 = self.sql("PRAGMA temp.page_size", |s| {
            Ok(s.query_row([], |r| r.get(0))?)
        })?;
        let bytes = checked_mul(nonnegative(pages)?, nonnegative(size)?)?;
        self.counters.temp_logical_bytes_peak = self.counters.temp_logical_bytes_peak.max(bytes);
        control.check()?;
        Ok(bytes)
    }

    pub(in crate::query) fn finish(mut self) -> QueryResult<GraceCounters> {
        let resources = self.resources.clone();
        let control = ExecutionControl::new(&resources, &|| false);
        let result = self.temp_logical_bytes(&control);
        let closed = self.close();
        result?;
        closed?;
        Ok(self.counters)
    }

    pub(in crate::query) fn abort(mut self) -> QueryResult<()> {
        self.close()
    }

    fn connection(&self) -> QueryResult<&Connection> {
        self.connection
            .as_ref()
            .ok_or_else(|| QueryError::internal("Grace spill is closed"))
    }

    fn close(&mut self) -> QueryResult<()> {
        match self.connection.take() {
            Some(connection) => connection.close().map_err(|(_, error)| error.into()),
            None => Ok(()),
        }
    }

    fn close_on_error<T>(&mut self, result: QueryResult<T>) -> QueryResult<T> {
        if result.is_err() {
            let _ = self.close();
        }
        result
    }

    fn sql<T>(
        &mut self,
        sql: &str,
        apply: impl FnOnce(&mut CachedStatement<'_>) -> QueryResult<T>,
    ) -> QueryResult<T> {
        if sql.len() > SQL_TEXT_BYTES {
            return Err(QueryError::internal(
                "Grace SQL exceeds its reserved template width",
            ));
        }
        let mut statement = self
            .connection()?
            .prepare_cached(sql)
            .map_err(|error| private_temp_error(error.into()))?;
        let _ = SqlCounts::take(&statement)?;
        let result = apply(&mut statement).map_err(private_temp_error);
        let counts = SqlCounts::take(&statement);
        drop(statement);
        let counts = counts?;
        self.counters.sqlite_vm_steps = checked_add(self.counters.sqlite_vm_steps, counts.vm)?;
        self.counters.sqlite_fullscan_steps =
            checked_add(self.counters.sqlite_fullscan_steps, counts.scans)?;
        self.counters.sqlite_sorts = checked_add(self.counters.sqlite_sorts, counts.sorts)?;
        result
    }
}

fn private_temp_error(error: QueryError) -> QueryError {
    let code = error.sqlite_code;
    if matches!(
        code,
        Some(
            rusqlite::ffi::SQLITE_READONLY
                | rusqlite::ffi::SQLITE_CANTOPEN
                | rusqlite::ffi::SQLITE_PERM
        )
    ) {
        // These codes mean that this query-private scratch cannot be used.
        // TOOBIG preserves the existing Resource boundary: retaining 8/14
        // would make the public bridge force Io even with kind=Resource.
        return super::super::resources::memory_error(&format!(
            "private Grace TEMP storage is unavailable (SQLite code {})",
            code.unwrap_or_default()
        ));
    }
    error
}

fn nonnegative(value: i64) -> QueryResult<u64> {
    u64::try_from(value).map_err(|_| QueryError::internal("negative Grace SQLite size"))
}

impl Drop for GracePartitions {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
