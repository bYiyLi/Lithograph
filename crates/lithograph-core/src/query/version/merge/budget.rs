use std::io::Write;

use super::*;
use crate::query::resources::encoded_json_bytes_controlled;

pub(super) fn reserve_push<T>(
    connection: &Connection,
    values: &Vec<T>,
    memory: &mut MemoryReservation,
) -> QueryResult<()> {
    connection.checkpoint()?;
    if values.len() == values.capacity() {
        let capacity = values.capacity().max(2).checked_mul(2).ok_or_else(|| {
            crate::query::resources::memory_error("version output capacity overflow")
        })?;
        memory.grow(allocation_bytes::<T>(capacity - values.capacity())?)?;
    }
    Ok(())
}

pub(super) fn copy_optional(
    connection: &Connection,
    value: Option<&Value>,
) -> QueryResult<(Option<Value>, MemoryReservation)> {
    match value {
        Some(value) => connection
            .copy_value(value)
            .map(|(value, memory)| (Some(value), memory)),
        None => Ok((None, connection.reserve(0)?)),
    }
}

pub(super) fn conflict_bytes(connection: &Connection, value: &MergeConflict) -> QueryResult<u64> {
    let mut bytes = checked_add(
        std::mem::size_of::<MergeConflict>() as u64,
        value.slot.capacity() as u64,
    )?;
    for value in [&value.base, &value.ours, &value.theirs, &value.resolution]
        .into_iter()
        .flatten()
    {
        bytes = checked_add(bytes, value_bytes_controlled(value, connection.control())?)?;
    }
    Ok(bytes)
}

pub(super) fn copy_conflict(
    connection: &Connection,
    value: &MergeConflict,
) -> QueryResult<VersionOwned<MergeConflict>> {
    let memory = connection.reserve(conflict_bytes(connection, value)?)?;
    let copy = |value: Option<&Value>| {
        value
            .map(|value| {
                crate::query::expression::clone_value_unreserved_controlled(
                    value,
                    Some(connection.control()),
                )
            })
            .transpose()
    };
    let slot =
        crate::query::expression::clone_string_controlled(&value.slot, Some(connection.control()))?;
    Ok(VersionOwned {
        value: MergeConflict {
            id: value.id,
            slot,
            base: copy(value.base.as_ref())?,
            ours: copy(value.ours.as_ref())?,
            theirs: copy(value.theirs.as_ref())?,
            resolution: copy(value.resolution.as_ref())?,
        },
        memory,
    })
}

pub(super) fn resolve_value(
    connection: &Connection,
    conflict: &MergeConflict,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<VersionOwned<Option<Value>>> {
    let source = match resolutions.get(&conflict.id) {
        None => conflict.ours.as_ref(),
        Some(Value::Map(map)) => match string_map(map, "choice")? {
            "ours" => conflict.ours.as_ref(),
            "theirs" => conflict.theirs.as_ref(),
            "value" => map
                .get("value")
                .filter(|value| !matches!(value, Value::Null)),
            _ => {
                return Err(QueryError::invalid_argument(
                    "resolution choice must be ours, theirs, or value",
                ));
            }
        },
        Some(_) => {
            return Err(QueryError::invalid_argument(
                "merge resolution must be a Map",
            ));
        }
    };
    let (value, memory) = copy_optional(connection, source)?;
    Ok(VersionOwned { value, memory })
}

pub(super) fn conflict_hash(
    connection: &Connection,
    identity: &MergeIdentity<'_>,
    slot: &str,
    base: Option<&Value>,
    ours: Option<&Value>,
    theirs: Option<&Value>,
) -> QueryResult<HashId> {
    let text_bytes = [
        &identity.base_identity,
        &identity.ours_identity,
        &identity.theirs_identity,
        slot,
    ]
    .iter()
    .try_fold(0, |bytes, text| checked_add(bytes, text.len() as u64))?;
    let mut bytes = checked_add(
        text_bytes,
        checked_mul(6, map_entry_bytes::<serde_json::Value>()?)?,
    )?;
    for value in [base, ours, theirs].into_iter().flatten() {
        bytes = checked_add(
            bytes,
            encoded_json_bytes_controlled(value, connection.control())?,
        )?;
    }
    let _memory = connection.reserve(checked_add(bytes, 4096)?)?;
    let mut values = Vec::with_capacity(7);
    for text in [
        &identity.base_identity,
        &identity.ours_identity,
        &identity.theirs_identity,
        slot,
    ] {
        values.push(serde_json::Value::String(
            crate::query::expression::clone_string_controlled(text, Some(connection.control()))?,
        ));
    }
    for source in [base, ours, theirs] {
        let present = source.is_some();
        let mut checkpoint = |work, bytes| {
            connection
                .control()
                .work(work)
                .and_then(|()| connection.control().bytes(bytes))
        };
        let value = match source {
            Some(value) => cypher::encode_json_checked(value, &mut checkpoint)?,
            None => serde_json::Value::Null,
        };
        let mut map = serde_json::Map::new();
        map.insert("present".to_owned(), serde_json::Value::Bool(present));
        map.insert("value".to_owned(), value);
        values.push(serde_json::Value::Object(map));
    }
    hash_json(connection, &serde_json::Value::Array(values))
}

pub(super) fn logical_identity(
    connection: &Connection,
    slots: &BTreeMap<String, Value>,
) -> QueryResult<String> {
    let mut bytes = 0;
    for (key, value) in slots {
        connection.checkpoint()?;
        bytes = checked_add(
            bytes,
            checked_add(map_entry_bytes::<serde_json::Value>()?, key.len() as u64)?,
        )?;
        bytes = checked_add(
            bytes,
            encoded_json_bytes_controlled(value, connection.control())?,
        )?;
    }
    let _memory = connection.reserve(checked_add(bytes, 128)?)?;
    let mut map = serde_json::Map::new();
    for (key, value) in slots {
        let mut checkpoint = |work, bytes| {
            connection
                .control()
                .work(work)
                .and_then(|()| connection.control().bytes(bytes))
        };
        let value = cypher::encode_json_checked(value, &mut checkpoint)?;
        map.insert(
            crate::query::expression::clone_string_controlled(key, Some(connection.control()))?,
            value,
        );
    }
    let hash = hash_json(connection, &serde_json::Value::Object(map))?;
    Ok(format!("state/{}", hash.to_hex()))
}

struct HashWriter<'a, 'b> {
    connection: &'a Connection<'a, 'b>,
    hasher: blake3::Hasher,
    interrupted: bool,
    length: u64,
}

impl Write for HashWriter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.length = self
            .length
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        for chunk in bytes.chunks(64 * 1024) {
            if self.connection.control().bytes(chunk.len() as u64).is_err() {
                self.interrupted = true;
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            self.hasher.update(chunk);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn hash_json(connection: &Connection, value: &serde_json::Value) -> QueryResult<HashId> {
    hash_and_length(connection, value).map(|(hash, _)| hash)
}

pub(super) fn json_length(connection: &Connection, value: &serde_json::Value) -> QueryResult<u64> {
    hash_and_length(connection, value).map(|(_, length)| length)
}

fn hash_and_length(
    connection: &Connection,
    value: &serde_json::Value,
) -> QueryResult<(HashId, u64)> {
    let mut writer = HashWriter {
        connection,
        hasher: blake3::Hasher::new(),
        interrupted: false,
        length: 0,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, value) {
        return Err(if writer.interrupted {
            QueryError::interrupted()
        } else {
            QueryError::internal(format!("failed to hash merge value: {error}"))
        });
    }
    Ok((
        HashId::from_bytes(*writer.hasher.finalize().as_bytes()),
        writer.length,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_merge_hashes_preserve_existing_identity_and_explicit_null_presence() {
        let raw = rusqlite::Connection::open_in_memory().unwrap();
        let resources = crate::query::QueryResources::new(Some(1_048_576));
        let never = || false;
        let control = crate::query::ExecutionControl::new(&resources, &never);
        let connection = Connection::new(&raw, &control);
        let resolutions = BTreeMap::new();
        let identity = MergeIdentity {
            base_identity: "merge-base/a".to_owned(),
            ours_commit: HashId::from_bytes([1; 32]),
            ours_identity: "commit/b".to_owned(),
            theirs_identity: "commit/c".to_owned(),
            resolutions: &resolutions,
        };
        let nested = Value::Map(BTreeMap::from([
            ("$type".to_owned(), Value::Float(-0.0)),
            (
                "items".to_owned(),
                Value::List(vec![Value::Integer(i64::MAX), Value::Null]),
            ),
        ]));
        for base in [None, Some(&Value::Null), Some(&nested)] {
            let actual = conflict_hash(
                &connection,
                &identity,
                "node/1/property/x",
                base,
                Some(&nested),
                None,
            )
            .unwrap();
            let expected = super::super::support::conflict_id(
                &identity.base_identity,
                &identity.ours_identity,
                &identity.theirs_identity,
                "node/1/property/x",
                base,
                Some(&nested),
                None,
            );
            assert_eq!(actual, expected);
        }
        let slots = BTreeMap::from([("node/1/property/x".to_owned(), nested)]);
        assert_eq!(
            logical_identity(&connection, &slots).unwrap(),
            super::super::support::logical_state_identity(&slots).unwrap()
        );
        assert_eq!(resources.used(), 0);
    }
}
