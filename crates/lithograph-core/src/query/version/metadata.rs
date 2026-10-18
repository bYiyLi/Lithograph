//! Pre-allocation accounting for the legacy ref and Commit metadata procedures.
//! SQLite-owned strings are inspected before the procedure makes owned copies.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::mem::size_of;

use rusqlite::types::ValueRef;

use crate::cypher::Value;
use crate::query::resources::{
    MemoryReservation, allocation_bytes, checked_add, checked_mul, encoded_json_bytes_controlled,
    map_entry_bytes, value_bytes_controlled,
};
use crate::query::{ExecutionOptions, QueryError, QueryErrorKind, QueryResult};
use crate::storage::{self, HashId};

use super::{ProcedureRow, VersionExecution, VersionOwned, VersionRows};

pub(super) fn reserve_metadata(
    connection: &VersionExecution<'_, '_>,
    name: &str,
    args: &[Value],
    options: &ExecutionOptions,
) -> QueryResult<MemoryReservation> {
    let supported = name.starts_with("lithograph.branch.")
        || name.starts_with("lithograph.tag.")
        || name.starts_with("lithograph.commit.")
        || matches!(name, "lithograph.log" | "lithograph.index.rebuild");
    let mut memory = connection.reserve(0)?;
    if !supported {
        return Ok(memory);
    }
    reserve_input_copies(connection, &mut memory, args, options)?;
    match name {
        "lithograph.branch.list" => {
            reserve_refs(connection, &mut memory, "_lithograph_branches", 3)?
        }
        "lithograph.tag.list" => reserve_refs(connection, &mut memory, "_lithograph_tags", 2)?,
        "lithograph.commit.get" => reserve_commit_get(connection, &mut memory, args)?,
        "lithograph.log" => reserve_log(connection, &mut memory, args)?,
        _ => memory.grow(row_overhead(7)?)?,
    }
    Ok(memory)
}

fn reserve_input_copies(
    connection: &VersionExecution<'_, '_>,
    memory: &mut MemoryReservation,
    args: &[Value],
    options: &ExecutionOptions,
) -> QueryResult<()> {
    // Commit Data writes can keep a copied Value, the JSON tree, its serialized
    // text, and Storage's JSON validation tree alive at the same time.
    for value in args {
        connection.checkpoint()?;
        let value_bytes = value_bytes_controlled(value, connection.control())?;
        let json_bytes = encoded_json_bytes_controlled(value, connection.control())?;
        let encoded_text = json_text_bound(connection, value)?;
        let copies = checked_add(
            checked_add(value_bytes, checked_mul(json_bytes, 2)?)?,
            checked_mul(encoded_text, 2)?,
        )?;
        memory.grow(copies)?;
    }
    for value in [&options.author, &options.message].into_iter().flatten() {
        memory.grow(checked_mul(value.len() as u64, 2)?)?;
    }
    // Descriptor resolution and active Branch reads own only bounded ref names
    // and binary Commit identifiers. Ref names have a 255 UTF-8 byte contract.
    memory.grow(1024)
}

fn json_text_bound(connection: &VersionExecution<'_, '_>, value: &Value) -> QueryResult<u64> {
    connection.checkpoint()?;
    match value {
        Value::String(value) => checked_add(checked_mul(value.len() as u64, 6)?, 2),
        Value::List(values) => values.iter().try_fold(2, |bytes, value| {
            checked_add(bytes, checked_add(json_text_bound(connection, value)?, 1)?)
        }),
        Value::Map(values) => values.iter().try_fold(2, |bytes, (key, value)| {
            let key = checked_add(checked_mul(key.len() as u64, 6)?, 4)?;
            checked_add(
                bytes,
                checked_add(key, json_text_bound(connection, value)?)?,
            )
        }),
        // Other values either have a bounded JSON primitive representation or
        // are rejected by the existing Commit Data validation before encoding.
        _ => Ok(32),
    }
}

fn row_overhead(fields: u64) -> QueryResult<u64> {
    checked_add(
        checked_mul(fields, checked_add(map_entry_bytes::<Value>()?, 32)?)?,
        // Parent descriptors, short status strings, and Vec growth slack.
        2048,
    )
}

fn reserve_refs(
    connection: &VersionExecution<'_, '_>,
    memory: &mut MemoryReservation,
    table: &str,
    fields: u64,
) -> QueryResult<()> {
    let sql = format!("SELECT length(CAST(name AS BLOB)) FROM main.{table}");
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        connection.checkpoint()?;
        let name_bytes = unsigned(row.get(0)?)?;
        // list_branches keeps its owned NamedRef and a cloned row name alive;
        // list_tags transfers the name, so this is also a bound for that path.
        memory.grow(checked_add(
            row_overhead(fields)?,
            checked_mul(name_bytes, 2)?,
        )?)?;
    }
    Ok(())
}

fn reserve_commit_get(
    connection: &VersionExecution<'_, '_>,
    memory: &mut MemoryReservation,
    args: &[Value],
) -> QueryResult<()> {
    let commit = super::resolve_descriptor_at(connection, args, 0, "version")?;
    let record = read_commit(connection, commit)?;
    memory.grow(checked_add(row_overhead(7)?, record.metadata_bytes)?)?;
    let mut statement = connection
        .prepare("SELECT data_json FROM main._lithograph_commit_data WHERE commit_id = ?1")?;
    let mut rows = statement.query([commit.as_bytes().as_slice()])?;
    if let Some(row) = rows.next()? {
        let text = row.get_ref(0)?.as_str().map_err(|error| {
            QueryError::internal(format!("persisted Commit Data is not TEXT: {error}"))
        })?;
        memory.grow(json_decode_bound(connection, text)?)?;
    }
    Ok(())
}

pub(super) fn json_decode_bound(
    connection: &VersionExecution<'_, '_>,
    text: &str,
) -> QueryResult<u64> {
    let mut scanner = JsonTokens::default();
    for byte in text.bytes() {
        connection.control().bytes(1)?;
        scanner.inspect(byte)?;
    }
    scanner.required(text.len() as u64)
}

#[derive(Default)]
pub(super) struct JsonTokens {
    tokens: u64,
    entries: u64,
    quoted: bool,
    escaped: bool,
    atom: bool,
}

impl JsonTokens {
    pub(super) fn required(&self, bytes: u64) -> QueryResult<u64> {
        let values = checked_mul(
            self.tokens,
            (2 * (size_of::<Value>() + size_of::<serde_json::Value>())) as u64,
        )?;
        let nodes = checked_mul(
            self.entries,
            checked_add(
                map_entry_bytes::<Value>()?,
                map_entry_bytes::<serde_json::Value>()?,
            )?,
        )?;
        checked_add(checked_add(values, nodes)?, checked_mul(bytes, 4)?)
    }

    pub(super) fn inspect(&mut self, byte: u8) -> QueryResult<()> {
        if self.quoted {
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.quoted = false;
            }
            return Ok(());
        }
        match byte {
            b'"' => {
                self.tokens = checked_add(self.tokens, 1)?;
                self.quoted = true;
                self.atom = false;
            }
            b'{' | b'[' => {
                self.tokens = checked_add(self.tokens, 1)?;
                self.atom = false;
            }
            b':' => {
                self.entries = checked_add(self.entries, 1)?;
                self.atom = false;
            }
            b'}' | b']' | b',' => self.atom = false,
            value if value.is_ascii_whitespace() => self.atom = false,
            _ if !self.atom => {
                self.tokens = checked_add(self.tokens, 1)?;
                self.atom = true;
            }
            _ => {}
        }
        Ok(())
    }
}

struct CommitProbe {
    parent1: Option<HashId>,
    parent2: Option<HashId>,
    committed_at: i64,
    metadata_bytes: u64,
}

fn read_commit(connection: &VersionExecution<'_, '_>, commit: HashId) -> QueryResult<CommitProbe> {
    connection.checkpoint()?;
    let mut statement = connection.prepare(
        "SELECT parent1,parent2,committed_at,length(CAST(author AS BLOB)),length(CAST(message AS BLOB)) \
         FROM main._lithograph_commits WHERE id = ?1",
    )?;
    let mut rows = statement.query([commit.as_bytes().as_slice()])?;
    let row = rows.next()?.ok_or_else(|| {
        QueryError::new(
            QueryErrorKind::VersionNotFound,
            format!("Commit {} was not found", commit.to_hex()),
        )
    })?;
    Ok(CommitProbe {
        parent1: parent(row.get_ref(0)?)?,
        parent2: parent(row.get_ref(1)?)?,
        committed_at: row.get(2)?,
        metadata_bytes: metadata_bytes(row)?,
    })
}

fn metadata_bytes(row: &rusqlite::Row<'_>) -> QueryResult<u64> {
    checked_add(
        unsigned(row.get::<_, Option<i64>>(3)?.unwrap_or(0))?,
        unsigned(row.get::<_, Option<i64>>(4)?.unwrap_or(0))?,
    )
}

fn parent(value: ValueRef<'_>) -> QueryResult<Option<HashId>> {
    match value {
        ValueRef::Null => Ok(None),
        ValueRef::Blob(bytes) => HashId::from_slice(bytes).map(Some).map_err(Into::into),
        _ => Err(QueryError::internal("Commit parent is not a BLOB")),
    }
}

fn unsigned(value: i64) -> QueryResult<u64> {
    u64::try_from(value).map_err(|_| QueryError::internal("negative metadata allocation size"))
}

fn reserve_log(
    connection: &VersionExecution<'_, '_>,
    memory: &mut MemoryReservation,
    args: &[Value],
) -> QueryResult<()> {
    let limit = super::log_limit(args)?;
    let mut traversal = metadata_log_start(connection, memory, args)?;
    let mut largest_temporary = 0;
    for _ in 0..limit {
        connection.checkpoint()?;
        let Some((_, commit)) = traversal.ready.pop_first() else {
            break;
        };
        let record = read_commit(connection, commit)?;
        largest_temporary = largest_temporary.max(record.metadata_bytes);
        metadata_log_parents(
            connection,
            memory,
            &mut traversal,
            &record,
            &mut largest_temporary,
        )?;
        let cursor_bytes = cursor_bound(traversal.ready.len(), traversal.pending_children.len())?;
        memory.grow(checked_add(
            row_overhead(6)?,
            checked_add(record.metadata_bytes, cursor_bytes)?,
        )?)?;
    }
    memory.grow(largest_temporary)
}

fn metadata_log_start(
    connection: &VersionExecution<'_, '_>,
    memory: &mut MemoryReservation,
    args: &[Value],
) -> QueryResult<super::LogTraversal> {
    let Some(cursor) = args.get(2) else {
        return metadata_log_initial(connection, memory, args);
    };
    let cursor = super::string_value(cursor, "cursor")?;
    let parts = super::validated_log_cursor_parts_controlled(connection, cursor)?;
    let start = parse_hash(parts[1])?;
    memory.grow(read_commit(connection, start)?.metadata_bytes)?;
    let mut traversal = super::LogTraversal {
        start,
        ready: BTreeSet::new(),
        pending_children: BTreeMap::new(),
    };
    metadata_log_ready(connection, memory, &mut traversal, parts[2])?;
    metadata_log_pending(connection, memory, &mut traversal, parts[3])?;
    Ok(traversal)
}

fn metadata_log_initial(
    connection: &VersionExecution<'_, '_>,
    memory: &mut MemoryReservation,
    args: &[Value],
) -> QueryResult<super::LogTraversal> {
    let start = match args.first() {
        Some(value) => super::resolve_descriptor(connection, value, "version")?,
        None => super::active_head(connection)?,
    };
    let record = read_commit(connection, start)?;
    memory.grow(checked_add(1024, record.metadata_bytes)?)?;
    Ok(super::LogTraversal {
        start,
        ready: BTreeSet::from([(Reverse(record.committed_at), start)]),
        pending_children: BTreeMap::new(),
    })
}

fn metadata_log_ready(
    connection: &VersionExecution<'_, '_>,
    memory: &mut MemoryReservation,
    traversal: &mut super::LogTraversal,
    encoded: &str,
) -> QueryResult<()> {
    for encoded in encoded.split(',').filter(|value| !value.is_empty()) {
        let commit = parse_hash(encoded)?;
        let record = read_commit(connection, commit)?;
        memory.grow(checked_add(1024, record.metadata_bytes)?)?;
        traversal
            .ready
            .insert((Reverse(record.committed_at), commit));
    }
    Ok(())
}

fn metadata_log_pending(
    connection: &VersionExecution<'_, '_>,
    memory: &mut MemoryReservation,
    traversal: &mut super::LogTraversal,
    encoded: &str,
) -> QueryResult<()> {
    for encoded in encoded.split(';').filter(|value| !value.is_empty()) {
        let (commit, count) = encoded
            .split_once(',')
            .ok_or_else(|| QueryError::invalid_argument("invalid log cursor"))?;
        let commit = parse_hash(commit)?;
        let count = count
            .parse::<usize>()
            .ok()
            .filter(|count| *count > 0)
            .ok_or_else(|| QueryError::invalid_argument("invalid log cursor"))?;
        memory.grow(checked_add(
            1024,
            read_commit(connection, commit)?.metadata_bytes,
        )?)?;
        traversal.pending_children.insert(commit, count);
    }
    Ok(())
}

fn parse_hash(value: &str) -> QueryResult<HashId> {
    HashId::from_hex(value).map_err(|_| QueryError::invalid_argument("invalid log cursor"))
}

fn metadata_log_parents(
    connection: &VersionExecution<'_, '_>,
    memory: &mut MemoryReservation,
    traversal: &mut super::LogTraversal,
    record: &CommitProbe,
    largest_temporary: &mut u64,
) -> QueryResult<()> {
    for parent in [record.parent1, record.parent2]
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>()
    {
        connection.checkpoint()?;
        if !traversal.pending_children.contains_key(&parent) {
            let count = storage::reachable_child_count(connection, traversal.start, parent)?;
            memory.grow(1024)?;
            traversal.pending_children.insert(parent, count);
        }
        let remaining = traversal
            .pending_children
            .get_mut(&parent)
            .ok_or_else(|| QueryError::internal("log metadata preflight lost parent"))?;
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| QueryError::internal("log metadata child count underflow"))?;
        if *remaining == 0 {
            traversal.pending_children.remove(&parent);
            let parent_record = read_commit(connection, parent)?;
            *largest_temporary = (*largest_temporary).max(parent_record.metadata_bytes);
            traversal
                .ready
                .insert((Reverse(parent_record.committed_at), parent));
        }
    }
    Ok(())
}

fn cursor_bound(ready: usize, pending: usize) -> QueryResult<u64> {
    let text = checked_add(
        128,
        checked_add(
            checked_mul(ready as u64, 65)?,
            checked_mul(pending as u64, 86)?,
        )?,
    )?;
    // Per-row output plus ready/pending String arrays, joined text, payload and
    // final checksum text coexist inside the existing cursor encoder.
    checked_add(
        checked_mul(text, 6)?,
        checked_mul(
            checked_add(ready as u64, pending as u64)?,
            size_of::<String>() as u64,
        )?,
    )
}

pub(super) fn guard_rows(
    connection: &VersionExecution<'_, '_>,
    mut memory: MemoryReservation,
    rows: Vec<ProcedureRow>,
) -> QueryResult<VersionRows> {
    let mut bytes = allocation_bytes::<ProcedureRow>(rows.capacity())?;
    for row in &rows {
        for (key, value) in row {
            connection.checkpoint()?;
            bytes = checked_add(
                bytes,
                checked_add(map_entry_bytes::<Value>()?, key.capacity() as u64)?,
            )?;
            bytes = checked_add(bytes, value_bytes_controlled(value, connection.control())?)?;
        }
    }
    memory.resize(bytes)?;
    Ok(VersionOwned {
        value: rows,
        memory,
    })
}

pub(super) fn serialize_json(
    connection: &VersionExecution<'_, '_>,
    value: &serde_json::Value,
) -> QueryResult<String> {
    connection.control().check()?;
    let mut writer = JsonWriter {
        control: connection.control(),
        output: Vec::new(),
        error: None,
    };
    let result = serde_json::to_writer(&mut writer, value);
    if let Some(error) = writer.error {
        return Err(error);
    }
    result.map_err(|error| {
        QueryError::internal(format!("failed to serialize Commit Data: {error}"))
    })?;
    connection.control().check()?;
    String::from_utf8(writer.output)
        .map_err(|error| QueryError::internal(format!("Commit Data is not UTF-8: {error}")))
}

struct JsonWriter<'a, 'b> {
    control: &'a crate::query::ExecutionControl<'b>,
    output: Vec<u8>,
    error: Option<QueryError>,
}

impl Write for JsonWriter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for chunk in bytes.chunks(64 * 1024) {
            if let Err(error) = self
                .control
                .bytes(chunk.len() as u64)
                .and_then(|()| self.control.work(1))
            {
                self.error = Some(error);
                return Err(std::io::Error::other(
                    "Commit Data serialization interrupted",
                ));
            }
            self.output.extend_from_slice(chunk);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{ExecutionControl, QueryResources};

    fn fixture() -> (rusqlite::Connection, HashId) {
        let raw = rusqlite::Connection::open_in_memory().unwrap();
        raw.execute_batch("CREATE TABLE _lithograph_commits(id BLOB PRIMARY KEY,parent1 BLOB,parent2 BLOB,committed_at INTEGER,author TEXT,message TEXT); CREATE TABLE _lithograph_commit_data(commit_id BLOB PRIMARY KEY,data_json TEXT);").unwrap();
        let chosen = HashId::from_bytes([1; 32]);
        raw.execute(
            "INSERT INTO _lithograph_commits VALUES(?1,NULL,NULL,1,'author','message')",
            [chosen.as_bytes().as_slice()],
        )
        .unwrap();
        let unrelated = HashId::from_bytes([2; 32]);
        raw.execute(
            "INSERT INTO _lithograph_commits VALUES(?1,NULL,NULL,2,?2,'message')",
            rusqlite::params![unrelated.as_bytes().as_slice(), "x".repeat(2 * 1024 * 1024)],
        )
        .unwrap();
        (raw, chosen)
    }

    #[test]
    fn metadata_preflight_only_charges_the_requested_commit_and_log_page() {
        let (raw, chosen) = fixture();
        let resources = QueryResources::new(Some(1024 * 1024));
        let never = || false;
        let control = ExecutionControl::new(&resources, &never);
        let connection = VersionExecution::new(&raw, &control);
        let version = Value::String(format!("commit/{}", chosen.to_hex()));
        for (name, args) in [
            ("lithograph.commit.get", vec![version.clone()]),
            ("lithograph.log", vec![version.clone(), Value::Integer(1)]),
        ] {
            let memory =
                reserve_metadata(&connection, name, &args, &ExecutionOptions::default()).unwrap();
            assert!(memory.bytes() < 128 * 1024);
            drop(memory);
            assert_eq!(resources.used(), 0);
        }
    }

    #[test]
    fn oversized_persisted_commit_data_is_rejected_before_owned_decode() {
        let (raw, chosen) = fixture();
        let text = format!("{{\"payload\":\"{}\"}}", "x".repeat(128 * 1024));
        raw.execute(
            "INSERT INTO _lithograph_commit_data VALUES(?1,?2)",
            rusqlite::params![chosen.as_bytes().as_slice(), text],
        )
        .unwrap();
        let resources = QueryResources::new(Some(64 * 1024));
        let never = || false;
        let control = ExecutionControl::new(&resources, &never);
        let connection = VersionExecution::new(&raw, &control);
        let args = [Value::String(format!("commit/{}", chosen.to_hex()))];
        let error = reserve_metadata(
            &connection,
            "lithograph.commit.get",
            &args,
            &ExecutionOptions::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(resources.used(), 0);
        assert!(resources.peak() < 64 * 1024);
    }

    #[test]
    fn metadata_output_guard_releases_temporary_allowance_and_follows_rows() {
        let (raw, _) = fixture();
        let resources = QueryResources::default();
        let never = || false;
        let control = ExecutionControl::new(&resources, &never);
        let connection = VersionExecution::new(&raw, &control);
        let memory = connection.reserve(128 * 1024).unwrap();
        let rows = vec![BTreeMap::from([(
            "data".to_owned(),
            Value::String("result".to_owned()),
        )])];
        let output = guard_rows(&connection, memory, rows).unwrap();
        assert!(resources.used() < 128 * 1024);
        assert_eq!(resources.used(), output.memory.bytes());
        drop(output);
        assert_eq!(resources.used(), 0);
    }
}
