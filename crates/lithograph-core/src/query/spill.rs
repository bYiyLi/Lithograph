use std::cell::RefCell;
use std::cmp::Ordering;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use rusqlite::{Connection, OptionalExtension, params};

use crate::cypher::{self, Value};

use super::{QueryError, QueryErrorKind, QueryResult};

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
pub(super) struct DistinctSpill {
    table: String,
    sequence: i64,
}

impl DistinctSpill {
    pub(super) fn create(connection: &Connection) -> QueryResult<Self> {
        let table = spill_table("distinct");
        create_output_table(connection, &table, true)?;
        Ok(Self { table, sequence: 0 })
    }

    pub(super) fn push(&mut self, connection: &Connection, row: &[Value]) -> QueryResult<()> {
        let key = distinct_row_key(row)?;
        let encoded = encode_row(row)?;
        let sql = format!(
            "INSERT OR IGNORE INTO temp.{}(seq,row_key,row_json) VALUES(?1,?2,?3)",
            self.table
        );
        if connection.execute(&sql, params![self.sequence, key, encoded])? == 1 {
            self.sequence += 1;
        }
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
    runs: String,
    output: String,
    seen: Option<String>,
    directions: Vec<bool>,
    chunk: Vec<SortRecord>,
    ordinal: u64,
    run: i64,
}

impl SortSpill {
    pub(super) fn create(
        connection: &Connection,
        distinct: bool,
        directions: Vec<bool>,
    ) -> QueryResult<Self> {
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
            runs,
            output,
            seen,
            directions,
            chunk: Vec::with_capacity(SORT_RUN_ROWS),
            ordinal: 0,
            run: 0,
        })
    }

    pub(super) fn push(
        &mut self,
        connection: &Connection,
        row: Vec<Value>,
        keys: Vec<Value>,
    ) -> QueryResult<()> {
        if !accept_sort_row(connection, self.seen.as_deref(), &row)? {
            return Ok(());
        }
        self.chunk.push(SortRecord {
            row,
            keys,
            ordinal: self.ordinal,
        });
        self.ordinal = self.ordinal.saturating_add(1);
        if self.chunk.len() == SORT_RUN_ROWS {
            self.flush(connection)?;
        }
        Ok(())
    }

    pub(super) fn finish(
        &mut self,
        connection: &Connection,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<usize> {
        if !self.chunk.is_empty() {
            self.flush(connection)?;
        }
        merge_sort_runs(
            connection,
            is_interrupted,
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

    fn flush(&mut self, connection: &Connection) -> QueryResult<()> {
        flush_sort_run(
            connection,
            &self.runs,
            self.run,
            &mut self.chunk,
            &self.directions,
        )?;
        self.run += 1;
        Ok(())
    }
}

#[derive(Debug)]
struct SortRecord {
    row: Vec<Value>,
    keys: Vec<Value>,
    ordinal: u64,
}

#[derive(Debug)]
struct RunHead {
    run: i64,
    seq: i64,
    row_json: String,
    keys: Vec<Value>,
    ordinal: u64,
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
) -> QueryResult<bool> {
    seen.map_or(Ok(true), |table| insert_seen(connection, table, row))
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

fn encode_row(row: &[Value]) -> QueryResult<String> {
    let values = row.iter().map(cypher::encode_json).collect::<Vec<_>>();
    serde_json::to_string(&values)
        .map_err(|error| QueryError::internal(format!("failed to encode spill row: {error}")))
}

fn decode_row(text: &str) -> QueryResult<Vec<Value>> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| QueryError::internal(format!("failed to decode spill row: {error}")))?;
    let array = value
        .as_array()
        .ok_or_else(|| QueryError::internal("spill row is not a JSON array"))?;
    array
        .iter()
        .map(cypher::decode_json)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub(super) fn distinct_row_key(row: &[Value]) -> QueryResult<String> {
    let key = row
        .iter()
        .map(distinct_value_key)
        .collect::<QueryResult<Vec<_>>>()?;
    serde_json::to_string(&key)
        .map_err(|error| QueryError::internal(format!("failed to encode DISTINCT key: {error}")))
}

fn distinct_value_key(value: &Value) -> QueryResult<serde_json::Value> {
    use serde_json::json;

    match value {
        Value::Null => Ok(json!(["null"])),
        Value::Boolean(value) => Ok(json!(["boolean", value])),
        Value::Integer(value) => Ok(json!(["number", format!("i:{value}")])),
        Value::Float(value) => Ok(json!(["number", float_distinct_key(*value)])),
        Value::String(value) => Ok(json!(["string", value])),
        Value::List(values) => Ok(json!([
            "list",
            values
                .iter()
                .map(distinct_value_key)
                .collect::<QueryResult<Vec<_>>>()?
        ])),
        Value::Map(values) => {
            let entries = values
                .iter()
                .map(|(key, value)| Ok((key, distinct_value_key(value)?)))
                .collect::<QueryResult<Vec<_>>>()?;
            Ok(json!(["map", entries]))
        }
        Value::Node(value) => Ok(json!(["node", value.element_id])),
        Value::Relationship(value) => Ok(json!(["relationship", value.element_id])),
        Value::Path(value) => Ok(json!([
            "path",
            value
                .nodes
                .iter()
                .map(|node| node.element_id.as_str())
                .collect::<Vec<_>>(),
            value
                .relationships
                .iter()
                .map(|relationship| relationship.element_id.as_str())
                .collect::<Vec<_>>()
        ])),
        _ => Ok(json!([
            "value",
            canonicalize_json_numbers(cypher::encode_json(value))
        ])),
    }
}

fn float_distinct_key(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_finite() && value.fract() == 0.0 {
        const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
        const I64_LOWER_INCLUSIVE: f64 = -9_223_372_036_854_775_808.0;
        if (I64_LOWER_INCLUSIVE..I64_UPPER_EXCLUSIVE).contains(&value) {
            return format!("i:{}", value as i64);
        }
    }
    format!("f:{:016x}", value.to_bits())
}

fn canonicalize_json_numbers(value: serde_json::Value) -> serde_json::Value {
    use serde_json::{Value as JsonValue, json};

    match value {
        JsonValue::Number(number) => number.as_f64().map_or(
            JsonValue::Number(number),
            |value| json!({"$number": float_distinct_key(value)}),
        ),
        JsonValue::Array(values) => {
            JsonValue::Array(values.into_iter().map(canonicalize_json_numbers).collect())
        }
        JsonValue::Object(values) => JsonValue::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json_numbers(value)))
                .collect(),
        ),
        value => value,
    }
}

fn insert_seen(connection: &Connection, table: &str, row: &[Value]) -> QueryResult<bool> {
    let key = distinct_row_key(row)?;
    let sql = format!("INSERT OR IGNORE INTO temp.{table}(row_key) VALUES(?1)");
    Ok(connection.execute(&sql, [key])? == 1)
}

pub(super) fn read_output_row(
    connection: &Connection,
    table: &str,
    after: i64,
) -> QueryResult<Option<(i64, Vec<Value>)>> {
    let sql = format!("SELECT seq,row_json FROM temp.{table} WHERE seq > ?1 ORDER BY seq LIMIT 1");
    let value = connection
        .query_row(&sql, [after], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .optional()?;
    value
        .map(|(seq, row)| decode_row(&row).map(|row| (seq, row)))
        .transpose()
}

fn flush_sort_run(
    connection: &Connection,
    table: &str,
    run: i64,
    chunk: &mut Vec<SortRecord>,
    directions: &[bool],
) -> QueryResult<()> {
    for record in chunk.iter() {
        for key in &record.keys {
            let _ = cypher::cypher_order_compare(key, key)?;
        }
    }
    let failure = RefCell::new(None);
    chunk.sort_by(
        |left, right| match compare_sort_records(left, right, directions) {
            Ok(ordering) => ordering,
            Err(error) => {
                *failure.borrow_mut() = Some(error);
                Ordering::Equal
            }
        },
    );
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
    let sql = format!(
        "INSERT INTO temp.{table}(run,seq,row_json,key_json,ordinal) VALUES(?1,?2,?3,?4,?5)"
    );
    for (seq, record) in chunk.iter().enumerate() {
        let ordinal = i64::try_from(record.ordinal).map_err(|_| {
            QueryError::new(QueryErrorKind::Resource, "sort ordinal exceeds INTEGER64")
        })?;
        connection.execute(
            &sql,
            params![
                run,
                seq as i64,
                encode_row(&record.row)?,
                encode_row(&record.keys)?,
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
) -> QueryResult<Ordering> {
    let ordering = compare_sort_keys(&left.keys, &right.keys, directions)?;
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
) -> QueryResult<Ordering> {
    for (index, (left, right)) in left.iter().zip(right).enumerate() {
        let mut ordering = cypher::cypher_order_compare(left, right)?;
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
    is_interrupted: &dyn Fn() -> bool,
    runs: &str,
    output: &str,
    run_count: i64,
    directions: &[bool],
) -> QueryResult<usize> {
    let mut heads = (0..run_count)
        .map(|run| load_run_head(connection, runs, run, 0))
        .collect::<QueryResult<Vec<_>>>()?;
    let insert = format!("INSERT INTO temp.{output}(seq,row_json) VALUES(?1,?2)");
    let mut output_seq = 0_i64;
    while let Some(index) = least_head_index(&heads, directions)? {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        let head = heads[index]
            .take()
            .ok_or_else(|| QueryError::internal("selected empty sort run head"))?;
        connection.execute(&insert, params![output_seq, head.row_json])?;
        output_seq += 1;
        heads[index] = load_run_head(connection, runs, head.run, head.seq + 1)?;
    }
    usize::try_from(output_seq).map_err(|_| {
        QueryError::new(
            QueryErrorKind::Resource,
            "sorted output row count exceeds usize",
        )
    })
}

fn least_head_index(heads: &[Option<RunHead>], directions: &[bool]) -> QueryResult<Option<usize>> {
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
        let ordering = compare_sort_keys(&candidate.keys, &current.keys, directions)?;
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
) -> QueryResult<Option<RunHead>> {
    let sql = format!("SELECT row_json,key_json,ordinal FROM temp.{table} WHERE run=?1 AND seq=?2");
    let value = connection
        .query_row(&sql, params![run, seq], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .optional()?;
    let Some((row_json, key_json, ordinal)) = value else {
        return Ok(None);
    };
    let ordinal = u64::try_from(ordinal)
        .map_err(|_| QueryError::internal("negative sort ordinal in TEMP run"))?;
    Ok(Some(RunHead {
        run,
        seq,
        row_json,
        keys: decode_row(&key_json)?,
        ordinal,
    }))
}
