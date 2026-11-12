//! The complete ASP semijoin NodeID set, before the filtered build starts.

use std::collections::BTreeSet;
use std::mem::size_of;
use std::ops::Bound::{Excluded, Unbounded};

use rusqlite::{Connection, Statement, StatementStatus, params};

use super::table::{FACTOR_PRIVATE_RUST_BYTES, FACTOR_RESIDENT_BYTES};
use crate::query::resources::{allocation_bytes, checked_add, map_entry_bytes, memory_error};
use crate::query::spill::{open_spill_connection, private_temp_error};
use crate::query::{ExecutionControl, MemoryReservation, QueryError, QueryResult};

#[cfg(test)]
mod tests;

const PAGE_MIN: usize = 4;
const PAGE_MAX: usize = 256;
const INSERT_ID: &str = "INSERT OR IGNORE INTO temp.asp_node_ids(id) VALUES(?1)";
const FIRST_PAGE: &str = "SELECT id FROM temp.asp_node_ids ORDER BY id LIMIT ?1";
const NEXT_PAGE: &str = "SELECT id FROM temp.asp_node_ids WHERE id>?1 ORDER BY id LIMIT ?2";

#[derive(Debug)]
pub(super) struct AspNodeIds {
    resident: BTreeSet<i64>,
    connection: Option<Connection>,
    resident_memory: MemoryReservation,
    private_memory: Option<MemoryReservation>,
    metadata_memory: MemoryReservation,
    len: u64,
    emitted: u64,
    after: Option<i64>,
    reading: bool,
    finished: bool,
    failed: bool,
    sqlite_vm_steps: u64,
}

impl AspNodeIds {
    pub(super) fn new(control: &ExecutionControl<'_>) -> QueryResult<Self> {
        let memory = control.reserve(size_of::<Self>() as u64)?;
        Ok(Self {
            resident: BTreeSet::new(),
            connection: None,
            resident_memory: control.reserve(0)?,
            private_memory: None,
            metadata_memory: memory,
            len: 0,
            emitted: 0,
            after: None,
            reading: false,
            finished: false,
            failed: false,
            sqlite_vm_steps: 0,
        })
    }

    pub(super) fn insert(&mut self, id: i64, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let result = self.insert_inner(id, control);
        self.close_on_error(result)
    }

    pub(super) fn len(&self) -> u64 {
        self.len
    }

    pub(super) fn sqlite_vm_steps(&self) -> u64 {
        self.sqlite_vm_steps
    }

    /// The first page freezes insertion; only a complete accumulated set is read.
    pub(super) fn next_page(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<(Vec<i64>, MemoryReservation)>> {
        let result = self.next_page_inner(control);
        self.close_on_error(result)
    }

    fn check_owner(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        control.check()?;
        self.metadata_memory.merge(control.reserve(0)?)?;
        if self.failed {
            return Err(QueryError::internal("ASP NodeID set is closed"));
        }
        Ok(())
    }

    fn insert_inner(&mut self, id: i64, control: &ExecutionControl<'_>) -> QueryResult<()> {
        self.check_owner(control)?;
        control.work(1)?;
        if self.reading {
            return Err(QueryError::internal("ASP NodeID set is already being read"));
        }
        if self.connection.is_none() {
            if self.resident.contains(&id) {
                return Ok(());
            }
            let bytes = map_entry_bytes::<i64>()?;
            if self.must_spill(bytes, control)? {
                self.spill(control)?;
            } else {
                let len = checked_add(self.len, 1)?;
                self.resident_memory.grow(bytes)?;
                self.resident.insert(id);
                self.len = len;
                return Ok(());
            }
        }
        let connection = self.connection.as_ref().ok_or_else(closed_private)?;
        let inserted = execute_private(
            connection,
            INSERT_ID,
            Some(id),
            &mut self.sqlite_vm_steps,
            control,
        )?;
        self.len = checked_add(self.len, inserted as u64)?;
        Ok(())
    }

    fn must_spill(&self, incoming: u64, control: &ExecutionControl<'_>) -> QueryResult<bool> {
        if checked_add(self.resident_memory.bytes(), incoming)? > FACTOR_RESIDENT_BYTES {
            return Ok(true);
        }
        // Preserve room to migrate the complete set and return the smallest page.
        let headroom = checked_add(FACTOR_PRIVATE_RUST_BYTES, page_bytes(PAGE_MIN)?)?;
        let required = checked_add(incoming, headroom)?;
        Ok(control
            .resources()
            .limit()
            .is_some_and(|limit| limit.saturating_sub(control.resources().used()) < required))
    }

    fn spill(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let memory = control.reserve(FACTOR_PRIVATE_RUST_BYTES)?;
        let connection = open_spill_connection().map_err(private_temp_error)?;
        execute_private(
            &connection,
            "BEGIN",
            None,
            &mut self.sqlite_vm_steps,
            control,
        )?;
        execute_private(
            &connection,
            "CREATE TEMP TABLE asp_node_ids(id INTEGER PRIMARY KEY)",
            None,
            &mut self.sqlite_vm_steps,
            control,
        )?;
        for id in &self.resident {
            control.work(1)?;
            execute_private(
                &connection,
                INSERT_ID,
                Some(*id),
                &mut self.sqlite_vm_steps,
                control,
            )?;
        }
        self.connection = Some(connection);
        self.private_memory = Some(memory);
        self.resident.clear();
        self.resident_memory.resize(0)?;
        Ok(())
    }

    fn next_page_inner(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<(Vec<i64>, MemoryReservation)>> {
        self.check_owner(control)?;
        self.reading = true;
        if self.finished || self.len == 0 {
            self.finished = true;
            return Ok(None);
        }
        // Declare the claim before its data, including on allocation/read errors.
        let capacity = page_capacity(control)?;
        let mut memory = control.reserve(page_bytes(capacity)?)?;
        let mut ids = Vec::new();
        ids.try_reserve_exact(capacity)
            .map_err(|_| memory_error("unable to allocate ASP NodeID page"))?;
        memory.resize(page_bytes(ids.capacity())?)?;
        self.fill_page(&mut ids, capacity, control)?;
        control.check()?;
        let emitted = checked_add(self.emitted, ids.len() as u64)?;
        if ids.is_empty() || emitted > self.len {
            return Err(QueryError::internal(
                "ASP NodeID page differs from its complete set",
            ));
        }
        self.emitted = emitted;
        self.finished = emitted == self.len;
        self.after = ids.last().copied();
        Ok(Some((ids, memory)))
    }

    fn fill_page(
        &mut self,
        ids: &mut Vec<i64>,
        capacity: usize,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if let Some(connection) = &self.connection {
            read_private_page(
                connection,
                self.after,
                capacity,
                ids,
                &mut self.sqlite_vm_steps,
                control,
            )
        } else {
            let lower = self.after.map_or(Unbounded, Excluded);
            for id in self.resident.range((lower, Unbounded)).take(capacity) {
                control.work(1)?;
                ids.push(*id);
            }
            Ok(())
        }
    }

    fn close_on_error<T>(&mut self, result: QueryResult<T>) -> QueryResult<T> {
        if result.is_err() {
            self.failed = true;
            self.connection = None;
            self.resident.clear();
            self.private_memory = None;
            let _ = self.resident_memory.resize(0);
        }
        result
    }
}

fn closed_private() -> QueryError {
    QueryError::internal("ASP NodeID private store is closed")
}

fn page_bytes(capacity: usize) -> QueryResult<u64> {
    checked_add(
        size_of::<Vec<i64>>() as u64,
        allocation_bytes::<i64>(capacity)?,
    )
}

fn page_capacity(control: &ExecutionControl<'_>) -> QueryResult<usize> {
    let Some(limit) = control.resources().limit() else {
        return Ok(PAGE_MAX);
    };
    let available = limit.saturating_sub(control.resources().used());
    let slots = available.saturating_sub(size_of::<Vec<i64>>() as u64) / size_of::<i64>() as u64;
    if slots < PAGE_MIN as u64 {
        return Err(memory_error(
            "query budget cannot retain an ASP NodeID page",
        ));
    }
    Ok(slots.min(PAGE_MAX as u64) as usize)
}

fn execute_private(
    connection: &Connection,
    sql: &str,
    id: Option<i64>,
    vm_steps: &mut u64,
    control: &ExecutionControl<'_>,
) -> QueryResult<usize> {
    control.check()?;
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| private_temp_error(error.into()))?;
    let result = match id {
        Some(id) => statement.execute(params![id]),
        None => statement.execute([]),
    };
    record_vm(&statement, vm_steps)?;
    let changed = result.map_err(|error| private_temp_error(error.into()))?;
    control.check()?;
    Ok(changed)
}

fn read_private_page(
    connection: &Connection,
    after: Option<i64>,
    limit: usize,
    ids: &mut Vec<i64>,
    vm_steps: &mut u64,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    control.check()?;
    let sql = if after.is_some() {
        NEXT_PAGE
    } else {
        FIRST_PAGE
    };
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| private_temp_error(error.into()))?;
    let result = read_ids(&mut statement, after, limit, ids, control);
    record_vm(&statement, vm_steps)?;
    result
}

fn read_ids(
    statement: &mut Statement<'_>,
    after: Option<i64>,
    limit: usize,
    ids: &mut Vec<i64>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let rows = match after {
        Some(after) => statement.query(params![after, limit as i64]),
        None => statement.query(params![limit as i64]),
    };
    let mut rows = rows.map_err(|error| private_temp_error(error.into()))?;
    while let Some(row) = rows
        .next()
        .map_err(|error| private_temp_error(error.into()))?
    {
        control.work(1)?;
        ids.push(
            row.get(0)
                .map_err(|error| private_temp_error(error.into()))?,
        );
    }
    Ok(())
}

fn record_vm(statement: &Statement<'_>, total: &mut u64) -> QueryResult<()> {
    let steps = u64::try_from(statement.reset_status(StatementStatus::VmStep))
        .map_err(|_| memory_error("ASP NodeID private SQLite counter overflow"))?;
    *total = checked_add(*total, steps)?;
    Ok(())
}
