use super::*;

pub(super) fn require_session(
    connection: &Connection,
    id: &str,
) -> QueryResult<MergeSessionRecord> {
    storage::load_merge_session(connection, id)?.ok_or_else(|| session_not_found(id))
}

pub(super) fn ensure_revision(session: &MergeSessionRecord, expected: i64) -> QueryResult<()> {
    if session.revision == expected {
        Ok(())
    } else {
        Err(session_changed(&session.id))
    }
}

pub(super) fn map_start_storage_error(error: storage::StorageError) -> QueryError {
    match error {
        storage::StorageError::NotFound(_) => QueryError::new(
            QueryErrorKind::VersionNotFound,
            "a pinned merge Commit no longer exists",
        ),
        storage::StorageError::BranchHeadMoved => QueryError::new(
            QueryErrorKind::BranchHeadMoved,
            "target Branch head moved while starting Merge Session",
        ),
        error => error.into(),
    }
}

pub(super) fn map_session_cas_error(
    connection: &Connection,
    id: &str,
    error: storage::StorageError,
) -> QueryError {
    match error {
        storage::StorageError::NotFound(_) => session_not_found(id),
        storage::StorageError::BranchHeadMoved => match storage::load_merge_session(connection, id)
        {
            Ok(None) => session_not_found(id),
            _ => session_changed(id),
        },
        error => error.into(),
    }
}

pub(super) fn session_not_found(id: &str) -> QueryError {
    QueryError::new(
        QueryErrorKind::MergeSessionNotFound,
        format!("Merge Session {id:?} was not found"),
    )
}

pub(super) fn session_changed(id: &str) -> QueryError {
    QueryError::new(
        QueryErrorKind::MergeSessionChanged,
        format!("Merge Session {id:?} revision changed"),
    )
}

pub(super) fn merge_conflict() -> QueryError {
    QueryError::new(
        QueryErrorKind::MergeConflict,
        "Merge Session has unresolved conflicts",
    )
}

pub(super) fn expected_commit(connection: &Connection, value: &Value) -> QueryResult<HashId> {
    let descriptor = string_value(value, "expectedHead")?;
    if !descriptor.starts_with("commit/") {
        return Err(QueryError::invalid_argument(
            "expectedHead must be a commit/<id> descriptor",
        ));
    }
    resolve_descriptor(connection, value, "expectedHead")
}

pub(super) fn merge_base_identity(bases: &[HashId]) -> String {
    let mut text = String::from("merge-base/");
    for (index, base) in bases.iter().enumerate() {
        if index != 0 {
            text.push(',');
        }
        text.push_str(&base.to_hex());
    }
    text
}

pub(super) fn parse_positive(value: &str, role: &str) -> QueryResult<i64> {
    value
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| QueryError::invalid_argument(format!("invalid {role}")))
}

pub(super) fn parse_element(value: &str, prefix: &str) -> QueryResult<i64> {
    value
        .strip_prefix(prefix)
        .ok_or_else(|| QueryError::invalid_argument("invalid elementId"))
        .and_then(|value| parse_positive(value, "elementId"))
}

pub(super) fn string_map<'a>(map: &'a BTreeMap<String, Value>, key: &str) -> QueryResult<&'a str> {
    match map.get(key) {
        Some(Value::String(value)) => Ok(value),
        _ => Err(QueryError::invalid_argument(format!(
            "{key} must be a String"
        ))),
    }
}

pub(super) fn integer_arg(args: &[Value], index: usize, role: &str) -> QueryResult<i64> {
    match args.get(index) {
        Some(Value::Integer(value)) if *value > 0 => Ok(*value),
        _ => Err(QueryError::invalid_argument(format!(
            "{role} must be a positive Integer"
        ))),
    }
}

pub(super) fn positive_limit(
    value: Option<&Value>,
    default: usize,
    role: &str,
) -> QueryResult<usize> {
    match value {
        None => Ok(default),
        Some(Value::Integer(value)) if *value > 0 => usize::try_from(*value)
            .map_err(|_| QueryError::invalid_argument(format!("{role} is too large"))),
        _ => Err(QueryError::invalid_argument(format!(
            "{role} must be a positive Integer"
        ))),
    }
}

pub(super) fn encode_list_cursor(id: &str) -> String {
    let payload = format!("merge-list1:{id}");
    cursor_checksum(&payload)
}

pub(super) fn decode_list_cursor(cursor: &str) -> QueryResult<String> {
    let (payload, checksum) = cursor
        .rsplit_once(':')
        .ok_or_else(|| QueryError::invalid_argument("invalid merge.list cursor"))?;
    if checksum != checksum_text(payload) || !payload.starts_with("merge-list1:") {
        return Err(QueryError::invalid_argument("invalid merge.list cursor"));
    }
    Ok(payload.trim_start_matches("merge-list1:").to_owned())
}

pub(super) fn encode_conflict_cursor(session: &str, revision: i64, offset: usize) -> String {
    cursor_checksum(&format!("merge-conflict1:{session}:{revision}:{offset}"))
}

pub(super) fn decode_conflict_cursor(
    cursor: &str,
    session: &str,
    revision: i64,
) -> QueryResult<usize> {
    let (payload, checksum) = cursor
        .rsplit_once(':')
        .ok_or_else(|| QueryError::invalid_argument("invalid merge conflict cursor"))?;
    if checksum != checksum_text(payload) {
        return Err(QueryError::invalid_argument(
            "invalid merge conflict cursor",
        ));
    }
    let prefix = format!("merge-conflict1:{session}:");
    let Some(rest) = payload.strip_prefix(&prefix) else {
        return Err(QueryError::new(
            QueryErrorKind::MergeSessionChanged,
            "merge conflict cursor belongs to a different Session",
        ));
    };
    let Some((cursor_revision, offset)) = rest.split_once(':') else {
        return Err(QueryError::invalid_argument(
            "invalid merge conflict cursor",
        ));
    };
    let cursor_revision = cursor_revision
        .parse::<i64>()
        .map_err(|_| QueryError::invalid_argument("invalid merge conflict cursor"))?;
    if cursor_revision != revision {
        return Err(session_changed(session));
    }
    offset
        .parse::<usize>()
        .map_err(|_| QueryError::invalid_argument("invalid merge conflict cursor"))
}

pub(super) fn cursor_checksum(payload: &str) -> String {
    format!("{payload}:{}", checksum_text(payload))
}

pub(super) fn checksum_text(payload: &str) -> String {
    let hash = blake3::hash(payload.as_bytes()).to_hex();
    hash.as_str()[..16].to_owned()
}

pub(super) fn commit_identity(commit: HashId) -> String {
    format!("commit/{}", commit.to_hex())
}

pub(super) fn logical_state_identity(slots: &BTreeMap<String, Value>) -> QueryResult<String> {
    let payload = cypher::encode_json(&Value::Map(slots.clone()));
    let bytes = serde_json::to_vec(&payload).map_err(|error| {
        QueryError::internal(format!("failed to encode logical state: {error}"))
    })?;
    Ok(format!("state/{}", blake3::hash(&bytes).to_hex()))
}

pub(super) fn conflict_id(
    base_identity: &str,
    ours_identity: &str,
    theirs_identity: &str,
    slot: &str,
    base: Option<&Value>,
    ours: Option<&Value>,
    theirs: Option<&Value>,
) -> HashId {
    let payload = Value::List(vec![
        Value::String(base_identity.to_owned()),
        Value::String(ours_identity.to_owned()),
        Value::String(theirs_identity.to_owned()),
        Value::String(slot.to_owned()),
        optional_hash_value(base),
        optional_hash_value(ours),
        optional_hash_value(theirs),
    ]);
    let bytes = serde_json::to_vec(&cypher::encode_json(&payload)).unwrap_or_default();
    HashId::from_bytes(*blake3::hash(&bytes).as_bytes())
}

fn optional_hash_value(value: Option<&Value>) -> Value {
    Value::Map(BTreeMap::from([
        ("present".to_owned(), Value::Boolean(value.is_some())),
        ("value".to_owned(), value.cloned().unwrap_or(Value::Null)),
    ]))
}
