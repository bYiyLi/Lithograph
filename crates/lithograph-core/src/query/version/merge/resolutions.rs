use super::*;
use crate::query::resources::encoded_json_bytes_controlled;

pub(super) fn resolution_ids(
    connection: &Connection,
    value: Option<&Value>,
) -> QueryResult<VersionOwned<BTreeSet<HashId>>> {
    let mut result = VersionOwned {
        value: BTreeSet::new(),
        memory: connection.reserve(0)?,
    };
    for item in resolution_list(value)? {
        connection.checkpoint()?;
        let id = resolution_conflict_id(resolution_map(item)?)?;
        result.memory.grow(map_entry_bytes::<HashId>()?)?;
        if !result.insert(id) {
            return Err(QueryError::invalid_argument(
                "duplicate conflictId in resolutions",
            ));
        }
    }
    Ok(result)
}

pub(super) fn resolution_items(
    connection: &Connection,
    value: Option<&Value>,
    conflicts: &BTreeMap<HashId, MergeConflict>,
) -> QueryResult<VersionOwned<BTreeMap<HashId, Value>>> {
    let mut result = VersionOwned {
        value: BTreeMap::new(),
        memory: connection.reserve(0)?,
    };
    for item in resolution_list(value)? {
        connection.checkpoint()?;
        let map = resolution_map(item)?;
        let id = resolution_conflict_id(map)?;
        if result.contains_key(&id) {
            return Err(QueryError::invalid_argument(
                "duplicate conflictId in resolutions",
            ));
        }
        let conflict = conflicts
            .get(&id)
            .ok_or_else(|| QueryError::invalid_argument("unknown conflictId"))?;
        result.memory.grow(map_entry_bytes::<Value>()?)?;
        let normalized = normalize_resolution(connection, map, &conflict.slot, &mut result.memory)?;
        result.insert(id, normalized);
    }
    Ok(result)
}

fn normalize_resolution(
    connection: &Connection,
    map: &BTreeMap<String, Value>,
    slot: &str,
    memory: &mut MemoryReservation,
) -> QueryResult<Value> {
    let choice = string_map(map, "choice")?;
    let fields = if choice == "value" { 2 } else { 1 };
    memory.grow(checked_add(
        checked_mul(fields, map_entry_bytes::<Value>()?)?,
        checked_add(choice.len() as u64, 64)?,
    )?)?;
    let mut normalized = BTreeMap::from([("choice".to_owned(), Value::String(choice.to_owned()))]);
    append_resolution_value(connection, memory, &mut normalized, map, slot, choice)?;
    Ok(Value::Map(normalized))
}

fn append_resolution_value(
    connection: &Connection,
    memory: &mut MemoryReservation,
    normalized: &mut BTreeMap<String, Value>,
    map: &BTreeMap<String, Value>,
    slot: &str,
    choice: &str,
) -> QueryResult<()> {
    match choice {
        "ours" | "theirs" if !map.contains_key("value") => Ok(()),
        "ours" | "theirs" => Err(QueryError::invalid_argument(
            "ours/theirs resolution cannot contain value",
        )),
        "value" => {
            let value = map
                .get("value")
                .ok_or_else(|| QueryError::invalid_argument("value resolution requires value"))?;
            validate_explicit_value(slot, value)?;
            let (value, claim) = connection.copy_value(value)?;
            memory.merge(claim)?;
            normalized.insert("value".to_owned(), value);
            Ok(())
        }
        _ => Err(QueryError::invalid_argument(
            "resolution choice must be ours, theirs, or value",
        )),
    }
}

pub(super) fn encode_resolution(
    connection: &Connection,
    value: &Value,
) -> QueryResult<VersionOwned<String>> {
    let mut memory =
        connection.reserve(encoded_json_bytes_controlled(value, connection.control())?)?;
    let mut checkpoint = |work, bytes| {
        connection
            .control()
            .work(work)
            .and_then(|()| connection.control().bytes(bytes))
    };
    let json = cypher::encode_json_checked(value, &mut checkpoint)?;
    let length = budget::json_length(connection, &json)?;
    memory.grow(checked_mul(length, 3)?)?;
    let output = super::super::metadata::serialize_json(connection, &json)?;
    drop(json);
    memory.resize(output.capacity() as u64)?;
    Ok(VersionOwned {
        value: output,
        memory,
    })
}

pub(crate) fn load_resolutions(
    connection: &Connection,
    session: &str,
) -> QueryResult<VersionOwned<BTreeMap<HashId, Value>>> {
    let mut result = VersionOwned {
        value: BTreeMap::new(),
        memory: connection.reserve(0)?,
    };
    let mut statement = connection.prepare("SELECT conflict_id,resolution_json FROM main._lithograph_merge_resolutions WHERE session_id=?1 ORDER BY conflict_id")?;
    let mut rows = statement.query([session])?;
    while let Some(row) = rows.next()? {
        connection.checkpoint()?;
        let id = HashId::from_slice(row.get_ref(0)?.as_blob().map_err(rusqlite::Error::from)?)?;
        let text = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
        let decoded = decode_resolution(connection, text)?;
        result.memory.merge(decoded.memory)?;
        result.insert(id, decoded.value);
    }
    Ok(result)
}

fn decode_resolution(connection: &Connection, text: &str) -> QueryResult<VersionOwned<Value>> {
    let mut memory =
        connection.reserve(super::super::metadata::json_decode_bound(connection, text)?)?;
    let json: serde_json::Value = serde_json::from_str(text).map_err(|_| {
        QueryError::new(
            QueryErrorKind::Storage,
            "stored merge resolution JSON is invalid",
        )
    })?;
    let value =
        cypher::decode_json_checked(&json, &mut |units| connection.control().work(units as u64))?;
    drop(json);
    memory.resize(checked_add(
        map_entry_bytes::<Value>()?,
        value_bytes_controlled(&value, connection.control())?,
    )?)?;
    Ok(VersionOwned { value, memory })
}
