use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use rusqlite::{Connection, params};

use crate::cypher::{self, Value};
use crate::storage::RelationshipRecord;

use super::expression::{BindingRow, BindingValue, LoadCsvContext};
use super::resources::{
    ExecutionControl, MemoryReservation, QueryResources, allocation_bytes, checked_add,
    checked_mul, owned_row_bytes_controlled, row_bytes_controlled,
};
use super::{QueryError, QueryErrorKind, QueryResult};

mod codec;
pub(super) use codec::distinct_row_key;
use codec::{decode_binding_row, decode_row, encode_binding_row, encode_row};

const SORT_RUN_ROWS: usize = 1_024;
static NEXT_SPILL_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn open_spill_connection() -> QueryResult<Connection> {
    let connection = Connection::open_in_memory()?;
    connection.execute_batch("PRAGMA temp_store = FILE")?;
    Ok(connection)
}

#[derive(Debug)]
pub(super) struct SpillOutput {
    pub(super) table: String,
    pub(super) after: i64,
    pub(super) total: usize,
}

#[derive(Debug)]
pub(super) struct RowSpill {
    resources: QueryResources,
    table: String,
    sequence: i64,
}

impl RowSpill {
    pub(super) fn create_with_resources(
        connection: &Connection,
        resources: QueryResources,
    ) -> QueryResult<Self> {
        let table = spill_table("rows");
        create_output_table(connection, &table, false)?;
        Ok(Self {
            resources,
            table,
            sequence: 0,
        })
    }

    pub(super) fn push(
        &mut self,
        connection: &Connection,
        row: &[Value],
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        control.check()?;
        let sql = format!(
            "INSERT INTO temp.{}(seq,row_json) VALUES(?1,?2)",
            self.table
        );
        let _encoding = self
            .resources
            .reserve(serialization_bytes(row_bytes_controlled(row, control)?)?)?;
        connection.execute(&sql, params![self.sequence, encode_row(row, control)?])?;
        control.check()?;
        self.sequence = self.sequence.saturating_add(1);
        Ok(())
    }

    pub(super) fn push_all(
        &mut self,
        connection: &Connection,
        rows: &[Vec<Value>],
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for row in rows {
            control.work(1)?;
            self.push(connection, row, control)?;
        }
        Ok(())
    }

    pub(super) fn output(self) -> SpillOutput {
        SpillOutput {
            table: self.table,
            after: -1,
            total: usize::try_from(self.sequence).unwrap_or(usize::MAX),
        }
    }
}

#[derive(Debug)]
pub(super) struct BindingSpill {
    resources: QueryResources,
    table: String,
    sequence: i64,
}

pub(super) const BINDING_SPILL_BATCH_ROWS: usize = 256;

#[derive(Debug)]
pub(super) struct SpilledBindings {
    pub(super) columns: Vec<String>,
    pub(super) spill: BindingSpill,
}

impl SpilledBindings {
    pub(super) fn from_rows_with_resources(
        connection: &Connection,
        columns: Vec<String>,
        rows: &[BindingRow],
        resources: QueryResources,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        Ok(Self {
            columns,
            spill: BindingSpill::from_rows_with_resources(connection, rows, resources, control)?,
        })
    }

    pub(super) fn with_spill(columns: Vec<String>, spill: BindingSpill) -> Self {
        Self { columns, spill }
    }

    pub(super) fn transform_batches(
        self,
        connection: &Connection,
        columns: Vec<String>,
        control: &ExecutionControl<'_>,
        mut transform: impl FnMut(&[String], Vec<BindingRow>, &mut BindingSpill) -> QueryResult<()>,
    ) -> QueryResult<Self> {
        let mut output =
            BindingSpill::create_with_resources(connection, self.spill.resources.clone())?;
        let result =
            self.spill
                .for_each_batch(connection, BINDING_SPILL_BATCH_ROWS, control, |rows| {
                    transform(&self.columns, rows, &mut output)
                });
        if let Err(error) = result {
            let _ = self.spill.abort(connection);
            let _ = output.abort(connection);
            return Err(error);
        }
        if let Err(error) = self.spill.abort(connection) {
            let _ = output.abort(connection);
            return Err(error);
        }
        Ok(Self::with_spill(columns, output))
    }

    pub(super) fn into_rows_reserved(
        self,
        connection: &Connection,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<(Vec<String>, Vec<BindingRow>, MemoryReservation)> {
        let result = self.spill.collect_reserved(connection, control);
        let cleanup = self.spill.abort(connection);
        let (rows, memory) = result?;
        cleanup?;
        Ok((self.columns, rows, memory))
    }
}

impl BindingSpill {
    pub(super) fn len(&self) -> usize {
        usize::try_from(self.sequence).unwrap_or(usize::MAX)
    }

    pub(super) fn create_with_resources(
        connection: &Connection,
        resources: QueryResources,
    ) -> QueryResult<Self> {
        let table = spill_table("bindings");
        connection.execute_batch(&format!(
            "CREATE TEMP TABLE temp.{table}(seq INTEGER PRIMARY KEY,row_json TEXT NOT NULL)"
        ))?;
        Ok(Self {
            resources,
            table,
            sequence: 0,
        })
    }

    pub(super) fn push(
        &mut self,
        connection: &Connection,
        row: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        control.check()?;
        let sql = format!(
            "INSERT INTO temp.{}(seq,row_json) VALUES(?1,?2)",
            self.table
        );
        let _encoding = self.resources.reserve(serialization_bytes(
            super::expression::binding_row_bytes_controlled(row, Some(control))?,
        )?)?;
        connection.execute(
            &sql,
            params![self.sequence, encode_binding_row(row, control)?],
        )?;
        control.check()?;
        self.sequence = self.sequence.saturating_add(1);
        Ok(())
    }

    pub(super) fn from_rows_with_resources(
        connection: &Connection,
        rows: &[BindingRow],
        resources: QueryResources,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let mut spill = Self::create_with_resources(connection, resources)?;
        if let Err(error) = spill.push_all(connection, rows, control) {
            let _ = spill.abort(connection);
            return Err(error);
        }
        Ok(spill)
    }

    pub(super) fn push_all(
        &mut self,
        connection: &Connection,
        rows: &[BindingRow],
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for row in rows {
            control.work(1)?;
            self.push(connection, row, control)?;
        }
        Ok(())
    }

    pub(super) fn for_each_batch(
        &self,
        connection: &Connection,
        limit: usize,
        control: &ExecutionControl<'_>,
        mut apply: impl FnMut(Vec<BindingRow>) -> QueryResult<()>,
    ) -> QueryResult<()> {
        let mut after = -1;
        loop {
            let (batch, _memory) = self.read_batch_reserved(connection, after, limit, control)?;
            if batch.is_empty() {
                return Ok(());
            }
            after = batch.last().map_or(after, |(sequence, _)| *sequence);
            // Iterator collection may reuse the tuple buffer, but correctness
            // must also cover implementations that allocate a second Vec.
            let _projection = self
                .resources
                .reserve(allocation_bytes::<BindingRow>(batch.len())?)?;
            apply(batch.into_iter().map(|(_, row)| row).collect())?;
        }
    }

    pub(super) fn read_batch_reserved(
        &self,
        connection: &Connection,
        after: i64,
        limit: usize,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<(Vec<(i64, BindingRow)>, MemoryReservation)> {
        let limit = i64::try_from(limit)
            .map_err(|_| QueryError::internal("binding spill batch size exceeds INTEGER64"))?;
        let sql = format!(
            "SELECT seq,row_json FROM temp.{} WHERE seq > ?1 ORDER BY seq LIMIT ?2",
            self.table
        );
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query(params![after, limit])?;
        let mut memory = self.resources.reserve(0)?;
        let mut output = Vec::new();
        while let Some(row) = rows.next()? {
            control.work(1)?;
            let seq = row.get::<_, i64>(0)?;
            let (decoded, decoded_memory) =
                decode_binding_record(row, 1, &self.resources, control)?;
            reserve_vec_slot(&mut output, &mut memory)?;
            memory.merge(decoded_memory)?;
            output.push((seq, decoded));
        }
        Ok((output, memory))
    }

    pub(super) fn collect_reserved(
        &self,
        connection: &Connection,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<(Vec<BindingRow>, MemoryReservation)> {
        let sql = format!("SELECT row_json FROM temp.{} ORDER BY seq", self.table);
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query([])?;
        let mut memory = self.resources.reserve(0)?;
        let mut output = Vec::new();
        while let Some(row) = rows.next()? {
            control.work(1)?;
            let (decoded, decoded_memory) =
                decode_binding_record(row, 0, &self.resources, control)?;
            reserve_vec_slot(&mut output, &mut memory)?;
            memory.merge(decoded_memory)?;
            output.push(decoded);
        }
        Ok((output, memory))
    }

    pub(super) fn abort(&self, connection: &Connection) -> QueryResult<()> {
        drop_temp_table(connection, &self.table)
    }
}

#[derive(Debug)]
pub(super) struct DistinctSpill {
    resources: QueryResources,
    table: String,
    sequence: i64,
}

impl DistinctSpill {
    pub(super) fn create_with_resources(
        connection: &Connection,
        resources: QueryResources,
    ) -> QueryResult<Self> {
        let table = spill_table("distinct");
        create_output_table(connection, &table, true)?;
        Ok(Self {
            resources,
            table,
            sequence: 0,
        })
    }

    pub(super) fn push(
        &mut self,
        connection: &Connection,
        row: &[Value],
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        // Both key and row JSON are live during the SQLite bind.
        let _encoding = self.resources.reserve(checked_mul(
            serialization_bytes(row_bytes_controlled(row, control)?)?,
            2,
        )?)?;
        let key = distinct_row_key(row, Some(control))?;
        let encoded = encode_row(row, control)?;
        let sql = format!(
            "INSERT OR IGNORE INTO temp.{}(seq,row_key,row_json) VALUES(?1,?2,?3)",
            self.table
        );
        if connection.execute(&sql, params![self.sequence, key, encoded])? == 1 {
            self.sequence += 1;
        }
        control.check()?;
        Ok(())
    }

    pub(super) fn output(self) -> SpillOutput {
        SpillOutput {
            table: self.table,
            after: -1,
            total: self.sequence as usize,
        }
    }

    pub(super) fn abort(&self, connection: &Connection) -> QueryResult<()> {
        drop_temp_table(connection, &self.table)
    }
}

#[derive(Debug)]
pub(super) struct SortSpill {
    resources: QueryResources,
    runs: String,
    output: String,
    seen: Option<String>,
    directions: Vec<bool>,
    chunk: Vec<SortRecord>,
    ordinal: u64,
    run: i64,
    chunk_memory: MemoryReservation,
    scratch_memory: MemoryReservation,
}

impl SortSpill {
    pub(super) fn create_with_resources(
        connection: &Connection,
        distinct: bool,
        directions: Vec<bool>,
        resources: QueryResources,
    ) -> QueryResult<Self> {
        let chunk_memory = resources.reserve(0)?;
        let scratch_memory = resources.reserve(0)?;
        let runs = spill_table("runs");
        let output = spill_table("sorted");
        create_runs_table(connection, &runs)?;
        if let Err(error) = create_output_table(connection, &output, false) {
            let _ = drop_temp_table(connection, &runs);
            return Err(error);
        }
        let seen = match create_optional_seen_table(connection, distinct) {
            Ok(seen) => seen,
            Err(error) => {
                let _ = drop_temp_table(connection, &runs);
                let _ = drop_temp_table(connection, &output);
                return Err(error);
            }
        };
        Ok(Self {
            resources,
            chunk_memory,
            scratch_memory,
            runs,
            output,
            seen,
            directions,
            chunk: Vec::new(),
            ordinal: 0,
            run: 0,
        })
    }

    pub(super) fn push(
        &mut self,
        connection: &Connection,
        row: Vec<Value>,
        keys: Vec<Value>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let bytes = sort_payload_bytes(&row, &keys, control)?;
        let memory = match self.resources.reserve(bytes) {
            Ok(memory) => memory,
            Err(error) if !self.chunk.is_empty() => {
                self.flush(connection, control)?;
                self.resources.reserve(bytes).map_err(|_| error)?
            }
            Err(error) => return Err(error),
        };
        let empty = self.resources.reserve(0)?;
        self.push_reserved(connection, row, keys, memory, empty, control)
    }

    pub(super) fn push_reserved(
        &mut self,
        connection: &Connection,
        row: Vec<Value>,
        keys: Vec<Value>,
        mut row_memory: MemoryReservation,
        key_memory: MemoryReservation,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        row_memory.merge(key_memory)?;
        let bytes = sort_payload_bytes(&row, &keys, control)?;
        row_memory.resize(bytes)?;
        let memory = row_memory;
        if let Err(error) = self.reserve_record_storage(bytes) {
            if self.chunk.is_empty() {
                return Err(error);
            }
            self.flush(connection, control)?;
            self.reserve_record_storage(bytes)?;
        }
        if !accept_sort_row(connection, self.seen.as_deref(), &row, control)? {
            return Ok(());
        }
        self.chunk.push(SortRecord {
            _memory: memory,
            row,
            keys,
            ordinal: self.ordinal,
        });
        self.ordinal = self.ordinal.saturating_add(1);
        if self.chunk.len() == SORT_RUN_ROWS || self.needs_producer_headroom(bytes)? {
            self.flush(connection, control)?;
        }
        Ok(())
    }

    pub(super) fn finish(
        &mut self,
        connection: &Connection,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<usize> {
        if !self.chunk.is_empty() {
            self.flush(connection, control)?;
        } else {
            self.chunk = Vec::new();
            self.chunk_memory.resize(0)?;
            self.scratch_memory.resize(0)?;
        }
        merge_sort_runs(
            connection,
            control,
            &self.runs,
            &self.output,
            self.run,
            &self.directions,
        )
    }

    pub(super) fn cleanup_aux(&self, connection: &Connection) -> QueryResult<()> {
        drop_temp_table(connection, &self.runs)?;
        drop_optional_temp_table(connection, self.seen.as_deref())
    }

    pub(super) fn abort(&self, connection: &Connection) -> QueryResult<()> {
        let mut first_error = None;
        for table in [
            Some(self.runs.as_str()),
            self.seen.as_deref(),
            Some(self.output.as_str()),
        ]
        .into_iter()
        .flatten()
        {
            if let Err(error) = drop_temp_table(connection, table)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub(super) fn into_output(self, total: usize) -> SpillOutput {
        SpillOutput {
            table: self.output,
            after: -1,
            total,
        }
    }

    fn needs_producer_headroom(&self, bytes: u64) -> QueryResult<bool> {
        let Some(limit) = self.resources.limit() else {
            return Ok(false);
        };
        // The producer materializes the next binding, projections and aliases
        // before push can react to pressure. Keep a conservative row working
        // allowance free now, while this run still has scratch for flushing.
        Ok(checked_add(self.resources.used(), serialization_bytes(bytes)?)? > limit)
    }

    fn reserve_record_storage(&mut self, bytes: u64) -> QueryResult<()> {
        // One shared scratch allowance is sufficient: only the current record
        // is encoded at a time, while row JSON and key JSON coexist.
        let scratch = checked_mul(serialization_bytes(bytes)?, 2)?;
        if scratch > self.scratch_memory.bytes() {
            self.scratch_memory.resize(scratch)?;
        }
        if self.chunk.len() == self.chunk.capacity() {
            let additional = self
                .chunk
                .capacity()
                .max(1)
                .min(SORT_RUN_ROWS - self.chunk.len());
            self.chunk_memory
                .grow(allocation_bytes::<SortRecord>(additional)?)?;
            self.chunk.try_reserve_exact(additional).map_err(|_| {
                super::resources::memory_error("unable to allocate sort run buffer")
            })?;
        }
        Ok(())
    }

    fn flush(
        &mut self,
        connection: &Connection,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        flush_sort_run(
            connection,
            &self.runs,
            self.run,
            &mut self.chunk,
            &self.directions,
            control,
        )?;
        self.chunk = Vec::new();
        self.chunk_memory.resize(0)?;
        self.scratch_memory.resize(0)?;
        self.run += 1;
        Ok(())
    }
}

#[derive(Debug)]
struct SortRecord {
    row: Vec<Value>,
    keys: Vec<Value>,
    ordinal: u64,
    _memory: MemoryReservation,
}

#[derive(Debug)]
struct RunHead {
    run: i64,
    seq: i64,
    row_json: String,
    key_json: String,
    keys: Vec<Value>,
    ordinal: u64,
    _memory: MemoryReservation,
}

fn sort_payload_bytes(
    row: &Vec<Value>,
    keys: &Vec<Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    checked_add(
        owned_row_bytes_controlled(row, control)?,
        owned_row_bytes_controlled(keys, control)?,
    )
}

fn spill_table(kind: &str) -> String {
    let id = NEXT_SPILL_ID.fetch_add(1, AtomicOrdering::Relaxed);
    format!("_lithograph_query_{kind}_{id}")
}

fn create_output_table(connection: &Connection, table: &str, distinct: bool) -> QueryResult<()> {
    let sql = if distinct {
        format!(
            "CREATE TEMP TABLE temp.{table}(seq INTEGER PRIMARY KEY,row_key TEXT NOT NULL UNIQUE,row_json TEXT NOT NULL)"
        )
    } else {
        format!("CREATE TEMP TABLE temp.{table}(seq INTEGER PRIMARY KEY,row_json TEXT NOT NULL)")
    };
    connection.execute_batch(&sql)?;
    Ok(())
}

fn create_seen_table(connection: &Connection, table: &str) -> QueryResult<()> {
    connection.execute_batch(&format!(
        "CREATE TEMP TABLE temp.{table}(row_key TEXT PRIMARY KEY) WITHOUT ROWID"
    ))?;
    Ok(())
}

fn create_optional_seen_table(
    connection: &Connection,
    distinct: bool,
) -> QueryResult<Option<String>> {
    if !distinct {
        return Ok(None);
    }
    let table = spill_table("seen");
    create_seen_table(connection, &table)?;
    Ok(Some(table))
}

fn accept_sort_row(
    connection: &Connection,
    seen: Option<&str>,
    row: &[Value],
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    seen.map_or(Ok(true), |table| {
        insert_seen(connection, table, row, control)
    })
}

fn drop_optional_temp_table(connection: &Connection, table: Option<&str>) -> QueryResult<()> {
    table.map_or(Ok(()), |table| drop_temp_table(connection, table))
}

fn create_runs_table(connection: &Connection, table: &str) -> QueryResult<()> {
    connection.execute_batch(&format!(
        "CREATE TEMP TABLE temp.{table}(run INTEGER NOT NULL,seq INTEGER NOT NULL,row_json TEXT NOT NULL,key_json TEXT NOT NULL,ordinal INTEGER NOT NULL,PRIMARY KEY(run,seq)) WITHOUT ROWID"
    ))?;
    Ok(())
}

pub(super) fn drop_temp_table(connection: &Connection, table: &str) -> QueryResult<()> {
    connection.execute_batch(&format!("DROP TABLE IF EXISTS temp.{table}"))?;
    Ok(())
}

// Includes the transient JSON DOM, escaped output and String growth. These are
// conservative working-memory allowances, rather than a serialized-size quota.
fn serialization_bytes(bytes: u64) -> QueryResult<u64> {
    checked_add(checked_mul(bytes, 16)?, 1024)
}

fn decoding_bytes(encoded: &str, control: &ExecutionControl<'_>) -> QueryResult<u64> {
    // Scan borrowed SQLite text before constructing either DOM. Count logical
    // tokens separately from text bytes: compact numeric arrays expand heavily,
    // while one large String does not need a per-byte Value allowance.
    let mut tokens = 0_u64;
    let mut quoted = false;
    let mut escaped = false;
    let mut atom = false;
    for byte in encoded.bytes() {
        control.work(1)?;
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => {
                    tokens = checked_add(tokens, 1)?;
                    quoted = true;
                    atom = false;
                }
                b'[' | b'{' => {
                    tokens = checked_add(tokens, 1)?;
                    atom = false;
                }
                b']' | b'}' | b',' | b':' => {
                    atom = false;
                }
                byte if byte.is_ascii_whitespace() => {
                    atom = false;
                }
                _ if !atom => {
                    tokens = checked_add(tokens, 1)?;
                    atom = true;
                }
                _ => {}
            }
        }
    }
    decoding_capacity_bytes(tokens, encoded.len())
}

fn decoding_capacity_bytes(tokens: u64, bytes: usize) -> QueryResult<u64> {
    // Per-token allowance covers both Value representations, copied map keys,
    // BTree nodes and growth slack; four times raw bytes covers text copies.
    checked_add(
        checked_add(checked_mul(tokens, 512)?, checked_mul(bytes as u64, 4)?)?,
        1024,
    )
}

fn binding_row_bytes(row: &BindingRow, control: &ExecutionControl<'_>) -> QueryResult<u64> {
    super::expression::binding_row_bytes_controlled(row, Some(control))
}

fn sqlite_text<'a>(row: &'a rusqlite::Row<'_>, index: usize) -> QueryResult<&'a str> {
    row.get_ref(index)?
        .as_str()
        .map_err(|error| QueryError::internal(format!("spill column is not TEXT: {error}")))
}

fn decode_binding_record(
    row: &rusqlite::Row<'_>,
    index: usize,
    resources: &QueryResources,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let encoded = sqlite_text(row, index)?;
    let mut memory = resources.reserve(decoding_bytes(encoded, control)?)?;
    let decoded = decode_binding_row(encoded, control)?;
    memory.resize(binding_row_bytes(&decoded, control)?)?;
    Ok((decoded, memory))
}

fn reserve_vec_slot<T>(values: &mut Vec<T>, memory: &mut MemoryReservation) -> QueryResult<()> {
    if values.len() == values.capacity() {
        let desired = values
            .capacity()
            .max(2)
            .checked_mul(2)
            .ok_or_else(|| super::resources::memory_error("spill buffer capacity overflow"))?;
        memory.grow(allocation_bytes::<T>(desired - values.capacity())?)?;
        values
            .try_reserve_exact(desired - values.len())
            .map_err(|_| super::resources::memory_error("unable to allocate spill read buffer"))?;
    }
    Ok(())
}

fn insert_seen(
    connection: &Connection,
    table: &str,
    row: &[Value],
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let key = distinct_row_key(row, Some(control))?;
    let sql = format!("INSERT OR IGNORE INTO temp.{table}(row_key) VALUES(?1)");
    Ok(connection.execute(&sql, [key])? == 1)
}

pub(super) fn read_output_row_with_resources(
    connection: &Connection,
    table: &str,
    after: i64,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(i64, Vec<Value>, MemoryReservation)>> {
    let resources = control.resources();
    control.check()?;
    let sql = format!("SELECT seq,row_json FROM temp.{table} WHERE seq > ?1 ORDER BY seq LIMIT 1");
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query([after])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let seq = row.get::<_, i64>(0)?;
    let encoded = row
        .get_ref(1)?
        .as_str()
        .map_err(|error| QueryError::internal(format!("spill row is not TEXT: {error}")))?;
    let mut memory = resources.reserve(decoding_bytes(encoded, control)?)?;
    let decoded = decode_row(encoded, control)?;
    memory.resize(owned_row_bytes_controlled(&decoded, control)?)?;
    Ok(Some((seq, decoded, memory)))
}

fn flush_sort_run(
    connection: &Connection,
    table: &str,
    run: i64,
    chunk: &mut Vec<SortRecord>,
    directions: &[bool],
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    for record in chunk.iter() {
        control.work(1)?;
        for key in &record.keys {
            let _ = super::expression::cypher_order_compare_controlled(key, key, Some(control))?;
        }
    }
    let failure = RefCell::new(None);
    chunk.sort_unstable_by(|left, right| {
        if failure.borrow().is_some() {
            return Ordering::Equal;
        }
        match compare_sort_records(left, right, directions, control) {
            Ok(ordering) => ordering,
            Err(error) => {
                *failure.borrow_mut() = Some(error);
                Ordering::Equal
            }
        }
    });
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
    let sql = format!(
        "INSERT INTO temp.{table}(run,seq,row_json,key_json,ordinal) VALUES(?1,?2,?3,?4,?5)"
    );
    for (seq, record) in chunk.iter().enumerate() {
        control.check()?;
        let ordinal = i64::try_from(record.ordinal).map_err(|_| {
            QueryError::new(QueryErrorKind::Resource, "sort ordinal exceeds INTEGER64")
        })?;
        connection.execute(
            &sql,
            params![
                run,
                seq as i64,
                encode_row(&record.row, control)?,
                encode_row(&record.keys, control)?,
                ordinal
            ],
        )?;
    }
    chunk.clear();
    Ok(())
}

fn compare_sort_records(
    left: &SortRecord,
    right: &SortRecord,
    directions: &[bool],
    control: &ExecutionControl<'_>,
) -> QueryResult<Ordering> {
    let ordering = compare_sort_keys(&left.keys, &right.keys, directions, control)?;
    Ok(if ordering == Ordering::Equal {
        left.ordinal.cmp(&right.ordinal)
    } else {
        ordering
    })
}

fn compare_sort_keys(
    left: &[Value],
    right: &[Value],
    directions: &[bool],
    control: &ExecutionControl<'_>,
) -> QueryResult<Ordering> {
    for (index, (left, right)) in left.iter().zip(right).enumerate() {
        let mut ordering =
            super::expression::cypher_order_compare_controlled(left, right, Some(control))?;
        if directions.get(index).copied().unwrap_or(false) {
            ordering = ordering.reverse();
        }
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(left.len().cmp(&right.len()))
}

fn merge_sort_runs(
    connection: &Connection,
    control: &ExecutionControl<'_>,
    runs: &str,
    output: &str,
    run_count: i64,
    directions: &[bool],
) -> QueryResult<usize> {
    // Byte-triggered runs can be numerous. Pairwise passes keep only two heads
    // live, so flushing sooner never turns into an unbounded merge allocation.
    let scratch = spill_table("merge");
    create_runs_table(connection, &scratch)?;
    let context = MergeContext {
        connection,
        control,
        directions,
    };
    let result = (|| {
        let mut source = runs;
        let mut target = scratch.as_str();
        let mut count = run_count;
        while count > 2 {
            connection.execute_batch(&format!("DELETE FROM temp.{target}"))?;
            let mut next_run = 0;
            let mut first = 0;
            while first < count {
                merge_run_pair(
                    &context,
                    source,
                    first,
                    (first + 2).min(count),
                    target,
                    Some(next_run),
                )?;
                first += 2;
                next_run += 1;
            }
            std::mem::swap(&mut source, &mut target);
            count = next_run;
        }
        merge_run_pair(&context, source, 0, count, output, None)
    })();
    let cleanup = drop_temp_table(connection, &scratch);
    match result {
        Err(error) => Err(error),
        Ok(count) => {
            cleanup?;
            Ok(count)
        }
    }
}

struct MergeContext<'a> {
    connection: &'a Connection,
    control: &'a ExecutionControl<'a>,
    directions: &'a [bool],
}

fn merge_run_pair(
    context: &MergeContext<'_>,
    runs: &str,
    first: i64,
    end: i64,
    output: &str,
    output_run: Option<i64>,
) -> QueryResult<usize> {
    let connection = context.connection;
    let control = context.control;
    let directions = context.directions;
    let mut heads = [None, None];
    for (index, run) in (first..end).enumerate() {
        control.check()?;
        heads[index] = load_run_head(connection, runs, run, 0, control)?;
    }
    let insert = if output_run.is_some() {
        format!(
            "INSERT INTO temp.{output}(run,seq,row_json,key_json,ordinal) VALUES(?1,?2,?3,?4,?5)"
        )
    } else {
        format!("INSERT INTO temp.{output}(seq,row_json) VALUES(?1,?2)")
    };
    let mut output_seq = 0_i64;
    while let Some(index) = least_head_index(&heads, directions, control)? {
        control.check()?;
        let head = heads[index]
            .take()
            .ok_or_else(|| QueryError::internal("selected empty sort run head"))?;
        if let Some(run) = output_run {
            connection.execute(
                &insert,
                params![
                    run,
                    output_seq,
                    head.row_json,
                    head.key_json,
                    head.ordinal as i64
                ],
            )?;
        } else {
            connection.execute(&insert, params![output_seq, head.row_json])?;
        }
        output_seq += 1;
        let run = head.run;
        let seq = head.seq + 1;
        drop(head);
        heads[index] = load_run_head(connection, runs, run, seq, control)?;
    }
    usize::try_from(output_seq).map_err(|_| {
        QueryError::new(
            QueryErrorKind::Resource,
            "sorted output row count exceeds usize",
        )
    })
}

fn least_head_index(
    heads: &[Option<RunHead>],
    directions: &[bool],
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<usize>> {
    let mut best = None;
    for (index, candidate) in heads.iter().enumerate() {
        let Some(candidate) = candidate else {
            continue;
        };
        let Some(best_index) = best else {
            best = Some(index);
            continue;
        };
        let current = heads[best_index]
            .as_ref()
            .ok_or_else(|| QueryError::internal("selected empty sort run head"))?;
        let ordering = compare_sort_keys(&candidate.keys, &current.keys, directions, control)?;
        if ordering == Ordering::Less
            || (ordering == Ordering::Equal && candidate.ordinal < current.ordinal)
        {
            best = Some(index);
        }
    }
    Ok(best)
}

fn load_run_head(
    connection: &Connection,
    table: &str,
    run: i64,
    seq: i64,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<RunHead>> {
    let sql = format!("SELECT row_json,key_json,ordinal FROM temp.{table} WHERE run=?1 AND seq=?2");
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(params![run, seq])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    decode_run_head(row, run, seq, control).map(Some)
}

fn decode_run_head(
    row: &rusqlite::Row<'_>,
    run: i64,
    seq: i64,
    control: &ExecutionControl<'_>,
) -> QueryResult<RunHead> {
    let resources = control.resources();
    let row_json = sqlite_text(row, 0)?;
    let key_json = sqlite_text(row, 1)?;
    let ordinal = u64::try_from(row.get::<_, i64>(2)?)
        .map_err(|_| QueryError::internal("negative sort ordinal in TEMP run"))?;
    let bytes = checked_add(row_json.len() as u64, key_json.len() as u64)?;
    let mut memory = resources.reserve(checked_add(bytes, decoding_bytes(key_json, control)?)?)?;
    let keys = decode_row(key_json, control)?;
    let row_json = super::expression::clone_string_controlled(row_json, Some(control))?;
    let key_json = super::expression::clone_string_controlled(key_json, Some(control))?;
    memory.resize(run_head_bytes(&row_json, &key_json, &keys, control)?)?;
    Ok(RunHead {
        _memory: memory,
        run,
        seq,
        row_json,
        key_json,
        keys,
        ordinal,
    })
}

fn run_head_bytes(
    row: &String,
    key: &String,
    keys: &Vec<Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    checked_add(
        checked_add(row.capacity() as u64, key.capacity() as u64)?,
        owned_row_bytes_controlled(keys, control)?,
    )
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cancellation_tests;
