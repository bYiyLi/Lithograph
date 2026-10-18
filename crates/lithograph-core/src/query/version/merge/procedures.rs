use super::*;

pub(in crate::query::version) fn start(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
    pinned_ours: HashId,
) -> QueryResult<super::super::VersionRows> {
    let _session_memory = connection.reserve(1024)?;
    let source = args
        .first()
        .ok_or_else(|| QueryError::invalid_argument("missing source"))?;
    let theirs = resolve_descriptor(connection, source, "source")?;
    let branch = target_branch(connection, options)?;
    let ours = pinned_ours;
    let expected = args
        .get(1)
        .map(|value| expected_commit(connection, value))
        .transpose()?;
    if expected.is_some_and(|expected| expected != ours) {
        return Err(QueryError::new(
            QueryErrorKind::BranchHeadMoved,
            "target Branch head does not match expectedHead",
        ));
    }
    let initial = compute_summary(connection, ours, theirs, &BTreeMap::new())?;
    let session =
        storage::create_merge_session(connection, &branch, ours, theirs, expected, now_micros()?)
            .map_err(map_start_storage_error)?;
    output_status_row(connection, &session, &initial)
}

pub(in crate::query::version) fn get(
    connection: &Connection,
    args: Vec<Value>,
) -> QueryResult<super::super::VersionRows> {
    super::super::with_read_snapshot(connection, |connection| get_snapshot(connection, &args))
}

fn get_snapshot(connection: &Connection, args: &[Value]) -> QueryResult<super::super::VersionRows> {
    let id = string_arg(args, 0, "session")?;
    let session = output_session(connection, id)?;
    let resolutions = load_resolutions(connection, id)?;
    let computation = compute_summary(connection, session.ours, session.theirs, &resolutions)?;
    output_status_row(connection, &session, &computation)
}

pub(in crate::query::version) fn list(
    connection: &Connection,
    args: Vec<Value>,
) -> QueryResult<super::super::VersionRows> {
    super::super::with_read_snapshot(connection, |connection| {
        output_list_snapshot(connection, &args)
    })
}

pub(in crate::query::version) fn conflicts(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
) -> QueryResult<super::super::VersionRows> {
    super::super::with_read_snapshot(connection, |connection| {
        conflicts_snapshot(connection, &args, options)
    })
}

fn conflicts_snapshot(
    connection: &Connection,
    args: &[Value],
    options: &ExecutionOptions,
) -> QueryResult<super::super::VersionRows> {
    let id = string_arg(args, 0, "session")?;
    let session = output_session(connection, id)?;
    let limit = positive_limit(args.get(1), 100, "merge.conflicts limit")?;
    let offset = args
        .get(2)
        .map(|value| string_value(value, "cursor"))
        .transpose()?
        .map(|cursor| decode_conflict_cursor(cursor, id, session.revision))
        .transpose()?
        .unwrap_or(0);
    let resolutions = load_resolutions(connection, id)?;
    let (page, memory) = conflict_page(
        connection,
        session.ours,
        session.theirs,
        &resolutions,
        offset,
        limit,
    )?;
    output_conflict_rows(
        connection,
        page,
        memory,
        id,
        session.revision,
        offset,
        options,
    )
}

pub(in crate::query::version) fn resolve(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
) -> QueryResult<super::super::VersionRows> {
    let id = string_arg(&args, 0, "session")?;
    let expected_revision = integer_arg(&args, 1, "expectedRevision")?;
    let session = output_session(connection, id)?;
    ensure_revision(&session, expected_revision)?;
    let current = load_resolutions(connection, id)?;
    let requested = resolution_ids(connection, args.get(2))?;
    let known = conflicts_for_ids(
        connection,
        session.ours,
        session.theirs,
        &current,
        &requested,
    )?;
    let incoming = resolution_items(connection, args.get(2), &known)?;
    check_resolution_access(connection, &known, &incoming, options)?;
    let (proposed, changed) = proposed_resolutions(connection, current, &incoming)?;
    let proposed_computation =
        compute_summary(connection, session.ours, session.theirs, &proposed)?;
    let encoded = encode_incoming_resolutions(connection, &incoming)?;
    let resulting_revision =
        storage::update_merge_resolutions(connection, id, expected_revision, &encoded, changed)
            .map_err(|error| map_session_cas_error(connection, id, error))?;
    resolution_status_row(connection, id, resulting_revision, &proposed_computation)
}

fn check_resolution_access(
    connection: &Connection,
    known: &BTreeMap<HashId, MergeConflict>,
    incoming: &BTreeMap<HashId, Value>,
    options: &ExecutionOptions,
) -> QueryResult<()> {
    for conflict in known.values() {
        connection.checkpoint()?;
        let next = budget::resolve_value(connection, conflict, incoming)?;
        super::super::access::check_resolution_slot(
            connection,
            &conflict.slot,
            conflict.ours.as_ref(),
            next.as_ref(),
            &options.access,
        )?;
    }
    Ok(())
}

fn proposed_resolutions(
    connection: &Connection,
    mut current: VersionOwned<BTreeMap<HashId, Value>>,
    incoming: &BTreeMap<HashId, Value>,
) -> QueryResult<(VersionOwned<BTreeMap<HashId, Value>>, bool)> {
    let mut changed = false;
    for (conflict_id, resolution) in incoming {
        connection.checkpoint()?;
        if !super::super::canonical::same_optional_value_controlled(
            connection,
            current.get(conflict_id),
            Some(resolution),
        )? {
            let (resolution, memory) = connection.copy_value(resolution)?;
            current.memory.grow(map_entry_bytes::<Value>()?)?;
            current.memory.merge(memory)?;
            current.insert(*conflict_id, resolution);
            changed = true;
        }
    }
    Ok((current, changed))
}

fn encode_incoming_resolutions(
    connection: &Connection,
    incoming: &BTreeMap<HashId, Value>,
) -> QueryResult<VersionOwned<BTreeMap<HashId, String>>> {
    let mut encoded = VersionOwned {
        value: BTreeMap::new(),
        memory: connection.reserve(0)?,
    };
    for (id, value) in incoming {
        connection.checkpoint()?;
        encoded.memory.grow(map_entry_bytes::<String>()?)?;
        let (value, memory) = encode_resolution(connection, value)?.into_parts();
        encoded.memory.merge(memory)?;
        encoded.insert(*id, value);
    }
    Ok(encoded)
}

fn resolution_status_row(
    connection: &Connection,
    id: &str,
    revision: i64,
    computation: &MergeComputation,
) -> QueryResult<super::super::VersionRows> {
    let memory = connection.reserve(checked_add(
        output_row_bytes(4)?,
        checked_add(id.len() as u64, computation.status.len() as u64)?,
    )?)?;
    let id = crate::query::expression::clone_string_controlled(id, Some(connection.control()))?;
    let mut output = row([
        ("session", Value::String(id)),
        ("revision", Value::Integer(revision)),
    ]);
    append_computation_status(&mut output, computation);
    Ok(super::super::VersionRows {
        value: vec![output],
        memory,
    })
}

pub(super) fn append_computation_status(output: &mut ProcedureRow, computation: &MergeComputation) {
    output.insert(
        "status".to_owned(),
        Value::String(computation.status.to_owned()),
    );
    output.insert(
        "unresolved".to_owned(),
        Value::Integer(i64::try_from(computation.unresolved).unwrap_or(i64::MAX)),
    );
}

pub(in crate::query::version) fn finalize(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
) -> QueryResult<super::super::VersionRows> {
    #[cfg(feature = "test-support")]
    let prepare_started = std::time::Instant::now();
    let PreparedFinalize {
        id,
        expected_revision,
        session,
        action,
    } = prepare_finalize(connection, &args)?;
    #[cfg(feature = "test-support")]
    crate::performance::record_merge_finalize_prepare(prepare_started.elapsed().as_micros());
    #[cfg(feature = "test-support")]
    let wait_started = std::time::Instant::now();
    acquire_finalize_writer(connection, &id, expected_revision)?;
    #[cfg(feature = "test-support")]
    crate::performance::record_merge_finalize_writer_wait(wait_started.elapsed().as_micros());
    #[cfg(feature = "test-support")]
    let hold_started = std::time::Instant::now();
    ensure_finalize_branch_head(connection, &session)?;
    let (status, commit) = apply_finalize_action(connection, &session, action, options)?;
    if !storage::delete_merge_session(connection, &id, expected_revision)
        .map_err(|error| map_session_cas_error(connection, &id, error))?
    {
        return Err(session_not_found(&id));
    }
    #[cfg(feature = "test-support")]
    crate::performance::record_merge_finalize_writer_hold(hold_started.elapsed().as_micros());
    let memory = connection.reserve(checked_add(output_row_bytes(2)?, 128)?)?;
    Ok(super::super::VersionRows {
        value: vec![row([
            ("status", Value::String(status.to_owned())),
            ("commit", commit_value(commit)),
        ])],
        memory,
    })
}

struct PreparedFinalize {
    id: VersionOwned<String>,
    expected_revision: i64,
    session: VersionOwned<MergeSessionRecord>,
    action: FinalizeAction,
}

fn prepare_finalize(connection: &Connection, args: &[Value]) -> QueryResult<PreparedFinalize> {
    let id = string_arg(args, 0, "session")?;
    let expected_revision = integer_arg(args, 1, "expectedRevision")?;
    let session = output_session(connection, id)?;
    ensure_revision(&session, expected_revision)?;
    let resolutions = load_resolutions(connection, id)?;
    let computation = compute_summary(connection, session.ours, session.theirs, &resolutions)?;
    if computation.unresolved != 0 {
        return Err(merge_conflict());
    }
    let action = finalize_action_for_computation(connection, &session, computation, &resolutions)?;
    let memory = connection.reserve(id.len() as u64)?;
    let id = crate::query::expression::clone_string_controlled(id, Some(connection.control()))?;
    Ok(PreparedFinalize {
        id: VersionOwned { value: id, memory },
        expected_revision,
        session,
        action,
    })
}

fn finalize_action_for_computation(
    connection: &Connection,
    session: &MergeSessionRecord,
    computation: MergeComputation,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<FinalizeAction> {
    Ok(match computation.status {
        "up_to_date" => FinalizeAction::UpToDate,
        "fast_forward" => FinalizeAction::FastForward,
        "ready" => ready_finalize_action(connection, session, computation.candidate, resolutions)?,
        "conflicted" => return Err(merge_conflict()),
        status => {
            return Err(QueryError::internal(format!(
                "unexpected merge status {status}"
            )));
        }
    })
}

fn ready_finalize_action(
    connection: &Connection,
    session: &MergeSessionRecord,
    candidate: Option<VersionState>,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<FinalizeAction> {
    if let Some(candidate) = candidate {
        let ours = connection.load_state(session.ours)?;
        return Ok(FinalizeAction::Merge {
            layer: connection.layer_between(&ours, &candidate)?,
            schema_blob: finalize_schema_blob(connection, &candidate.schema)?,
        });
    }
    let Some((layer, schema, unresolved)) =
        sparse_candidate_layer(connection, session.ours, session.theirs, resolutions)?
    else {
        return Err(QueryError::internal(
            "ready merge is missing its candidate state",
        ));
    };
    if unresolved != 0 {
        return Err(merge_conflict());
    }
    Ok(FinalizeAction::Merge {
        layer,
        schema_blob: finalize_schema_blob(connection, &schema)?,
    })
}

fn finalize_schema_blob(
    connection: &Connection,
    schema: &storage::SchemaState,
) -> QueryResult<VersionOwned<Vec<u8>>> {
    let check = || {
        connection
            .control()
            .check()
            .map_err(super::super::execution::storage_error)
    };
    let workspace = checked_mul(storage::schema_state_memory_bytes(schema, &check)?, 8)?;
    let mut memory = connection.reserve(workspace)?;
    let value = schema.canonical_blob_checked(&check)?;
    // This private action retains the exact source Schema's decode allowance.
    // The blob is generated above, so later decoding cannot introduce a larger
    // object shape from external input. Its bytes and decoder work coexist.
    memory.resize(checked_add(workspace, value.capacity() as u64)?)?;
    Ok(VersionOwned { value, memory })
}

fn acquire_finalize_writer(
    connection: &Connection,
    id: &str,
    expected_revision: i64,
) -> QueryResult<()> {
    storage::update_merge_resolutions(connection, id, expected_revision, &BTreeMap::new(), false)
        .map_err(|error| map_session_cas_error(connection, id, error))?;
    Ok(())
}

fn ensure_finalize_branch_head(
    connection: &Connection,
    session: &MergeSessionRecord,
) -> QueryResult<()> {
    let branch_head =
        storage::branch_head(connection, &session.target_branch).map_err(|error| match error {
            storage::StorageError::NotFound(_) => branch_not_found(&session.target_branch),
            error => error.into(),
        })?;
    if branch_head != session.ours {
        return Err(QueryError::new(
            QueryErrorKind::BranchHeadMoved,
            "target Branch moved after Merge Session start",
        ));
    }
    Ok(())
}

fn apply_finalize_action(
    connection: &Connection,
    session: &MergeSessionRecord,
    action: FinalizeAction,
    options: &ExecutionOptions,
) -> QueryResult<(&'static str, HashId)> {
    match action {
        FinalizeAction::UpToDate => Ok(("up_to_date", session.ours)),
        FinalizeAction::FastForward => {
            super::super::access::check_commits(
                connection,
                session.ours,
                session.theirs,
                &options.access,
            )?;
            storage::move_branch_ref(
                connection,
                &session.target_branch,
                Some(session.ours),
                session.theirs,
            )?;
            Ok(("fast_forward", session.theirs))
        }
        FinalizeAction::Merge { layer, schema_blob } => {
            apply_merge_action(connection, session, layer, schema_blob, options)
        }
    }
}

fn apply_merge_action(
    connection: &Connection,
    session: &MergeSessionRecord,
    layer: VersionLayer,
    schema_blob: VersionOwned<Vec<u8>>,
    options: &ExecutionOptions,
) -> QueryResult<(&'static str, HashId)> {
    let previous = connection.load_schema(session.ours)?;
    let check = || {
        connection
            .control()
            .check()
            .map_err(super::super::execution::storage_error)
    };
    let schema = storage::SchemaState::from_canonical_blob_checked(&schema_blob, &check)?;
    super::super::access::check_layer(connection, session.ours, &layer, &schema, &options.access)?;
    super::super::access::check_schema_validation(connection, &previous, &schema, &options.access)?;
    validate_merge_transitions(connection, &previous, &schema)?;
    let schema_hash = storage::persist_schema_blob(connection, &schema_blob)?;
    let _write_memory = finalize_layer_workspace(connection, &layer)?;
    let metadata = super::super::operations::copy_commit_metadata(connection, options)?;
    connection.control().check()?;
    let commit = connection.commit_layer_with_schema(
        &session.target_branch,
        session.ours,
        Some(session.theirs),
        &layer,
        schema_hash,
        &metadata,
    )?;
    connection.control().check()?;
    Ok(("merged", commit))
}

fn validate_merge_transitions(
    connection: &Connection,
    previous: &storage::SchemaState,
    schema: &storage::SchemaState,
) -> QueryResult<()> {
    crate::query::semantic_index::validate_fulltext_transition_controlled(
        connection,
        previous,
        schema,
        &|| connection.control().check().is_err(),
    )?;
    crate::query::managed_semantic::validate_transition_controlled(
        connection,
        previous,
        schema,
        &|| connection.control().check().is_err(),
    )?;
    Ok(())
}

fn finalize_layer_workspace(
    connection: &Connection,
    layer: &storage::LayerBuilder,
) -> QueryResult<MemoryReservation> {
    let mut slots = layer
        .node_changes()
        .map(|_| ())
        .chain(layer.label_changes().map(|_| ()))
        .chain(layer.relationship_changes().map(|_| ()));
    let mut bytes = slots.try_fold(4096, |bytes, ()| {
        connection.checkpoint()?;
        checked_add(bytes, 256)
    })?;
    let check = || {
        connection
            .control()
            .check()
            .map_err(super::super::execution::storage_error)
    };
    for (_, _, _, value) in layer.property_changes() {
        connection.checkpoint()?;
        bytes = checked_add(bytes, 512)?;
        if let Some(value) = value {
            bytes = checked_add(
                bytes,
                checked_mul(storage::property_value_memory_bytes(value, &check)?, 8)?,
            )?;
        }
    }
    connection.reserve(bytes)
}

pub(in crate::query::version) fn abort(
    connection: &Connection,
    args: Vec<Value>,
) -> QueryResult<super::super::VersionRows> {
    let id = string_arg(&args, 0, "session")?;
    let expected_revision = integer_arg(&args, 1, "expectedRevision")?;
    let memory = connection.reserve(checked_add(output_row_bytes(1)?, id.len() as u64)?)?;
    let id = crate::query::expression::clone_string_controlled(id, Some(connection.control()))?;
    match storage::delete_merge_session(connection, &id, expected_revision) {
        Ok(true) => Ok(super::super::VersionRows {
            value: vec![row([("session", Value::String(id))])],
            memory,
        }),
        Ok(false) => Err(session_not_found(&id)),
        Err(error) => Err(map_session_cas_error(connection, &id, error)),
    }
}

fn output_row_bytes(fields: u64) -> QueryResult<u64> {
    checked_add(
        allocation_bytes::<ProcedureRow>(1)?,
        checked_mul(fields, checked_add(map_entry_bytes::<Value>()?, 128)?)?,
    )
}

fn output_status_row(
    connection: &Connection,
    session: &MergeSessionRecord,
    computation: &MergeComputation,
) -> QueryResult<super::super::VersionRows> {
    connection.checkpoint()?;
    let text = checked_add(session.id.len() as u64, session.target_branch.len() as u64)?;
    let text = checked_add(text, checked_add(computation.status.len() as u64, 128)?)?;
    let memory = connection.reserve(checked_add(output_row_bytes(7)?, text)?)?;
    Ok(super::super::VersionRows {
        value: vec![session_status_row(session, computation)],
        memory,
    })
}

fn output_session(
    connection: &Connection,
    id: &str,
) -> QueryResult<VersionOwned<MergeSessionRecord>> {
    let mut statement = connection.prepare(
        "SELECT length(CAST(target_branch AS BLOB)) FROM main._lithograph_merge_sessions WHERE id = ?1",
    )?;
    let mut rows = statement.query([id])?;
    let Some(row) = rows.next()? else {
        return Err(session_not_found(id));
    };
    let text = checked_add(id.len() as u64, output_length(row.get(0)?)?)?;
    let overhead = checked_add(std::mem::size_of::<MergeSessionRecord>() as u64, 64)?;
    let memory = connection.reserve(checked_add(text, overhead)?)?;
    connection.control().check()?;
    let value = output_load_session(connection, id)?;
    connection.control().check()?;
    Ok(VersionOwned { value, memory })
}

fn output_load_session(connection: &Connection, id: &str) -> QueryResult<MergeSessionRecord> {
    let mut statement = connection.prepare("SELECT id, target_branch, ours_commit, theirs_commit, revision, created_at FROM main._lithograph_merge_sessions WHERE id = ?1")?;
    let mut records = statement.query([id])?;
    let record = records.next()?.ok_or_else(|| session_not_found(id))?;
    output_read_session(connection, record)
}

fn output_list_snapshot(
    connection: &Connection,
    args: &[Value],
) -> QueryResult<super::super::VersionRows> {
    let limit = positive_limit(args.first(), 100, "merge.list limit")?;
    let cursor = args
        .get(1)
        .map(|value| string_value(value, "cursor"))
        .transpose()?;
    let _cursor_memory = connection.reserve(checked_add(
        cursor.map_or(0, |cursor| cursor.len() as u64),
        1024,
    )?)?;
    connection.control().check()?;
    let after = cursor.map(decode_list_cursor).transpose()?;
    connection.control().check()?;
    let fetched_limit = limit.saturating_add(1);
    let memory = output_list_memory(connection, after.as_deref(), fetched_limit)?;
    let mut sessions = output_load_sessions(connection, after.as_deref(), fetched_limit)?;
    let has_more = sessions.len() > limit;
    if has_more {
        sessions.truncate(limit);
    }
    let count = sessions.len();
    let mut rows = Vec::with_capacity(count);
    for (index, session) in sessions.into_iter().enumerate() {
        connection.checkpoint()?;
        rows.push(output_list_row(session, has_more && index + 1 == count));
    }
    Ok(super::super::VersionRows {
        value: rows,
        memory,
    })
}

fn output_load_sessions(
    connection: &Connection,
    after: Option<&str>,
    limit: usize,
) -> QueryResult<Vec<MergeSessionRecord>> {
    let mut statement = connection.prepare("SELECT id, target_branch, ours_commit, theirs_commit, revision, created_at FROM main._lithograph_merge_sessions WHERE (?1 IS NULL OR id > ?1) ORDER BY id COLLATE BINARY LIMIT ?2")?;
    let mut rows = statement.query(rusqlite::params![
        after,
        i64::try_from(limit).unwrap_or(i64::MAX)
    ])?;
    let mut output = Vec::new();
    while let Some(row) = rows.next()? {
        connection.checkpoint()?;
        output.push(output_read_session(connection, row)?);
    }
    Ok(output)
}

fn output_read_session(
    connection: &Connection,
    row: &rusqlite::Row<'_>,
) -> QueryResult<MergeSessionRecord> {
    connection.checkpoint()?;
    Ok(MergeSessionRecord {
        id: output_read_text(connection, row, 0)?,
        target_branch: output_read_text(connection, row, 1)?,
        ours: output_read_hash(row, 2)?,
        theirs: output_read_hash(row, 3)?,
        revision: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn output_read_text(
    connection: &Connection,
    row: &rusqlite::Row<'_>,
    index: usize,
) -> QueryResult<String> {
    let value = row.get_ref(index)?;
    let value = value.as_str().map_err(|error| {
        QueryError::new(
            QueryErrorKind::Storage,
            format!("stored merge text is invalid: {error}"),
        )
    })?;
    crate::query::expression::clone_string_controlled(value, Some(connection.control()))
}

fn output_read_hash(row: &rusqlite::Row<'_>, index: usize) -> QueryResult<HashId> {
    let value = row.get_ref(index)?;
    let value = value.as_blob().map_err(|error| {
        QueryError::new(
            QueryErrorKind::Storage,
            format!("stored merge hash is invalid: {error}"),
        )
    })?;
    HashId::from_slice(value).map_err(QueryError::from)
}

fn output_list_memory(
    connection: &Connection,
    after: Option<&str>,
    limit: usize,
) -> QueryResult<MemoryReservation> {
    let mut memory = connection.reserve(0)?;
    let mut statement = connection.prepare(
        "SELECT length(CAST(id AS BLOB)), length(CAST(target_branch AS BLOB)) FROM main._lithograph_merge_sessions WHERE (?1 IS NULL OR id > ?1) ORDER BY id COLLATE BINARY LIMIT ?2",
    )?;
    let mut rows = statement.query(rusqlite::params![
        after,
        i64::try_from(limit).unwrap_or(i64::MAX),
    ])?;
    while let Some(row) = rows.next()? {
        connection.checkpoint()?;
        let id = output_length(row.get(0)?)?;
        let branch = output_length(row.get(1)?)?;
        // The materialization Vec grows geometrically with a minimum capacity
        // of four. Keep its capacity alongside the output maps; the owned
        // names themselves move into those maps.
        memory.grow(output_list_bytes(id, branch)?)?;
    }
    Ok(memory)
}

fn output_list_bytes(id: u64, branch: u64) -> QueryResult<u64> {
    let storage = checked_add(allocation_bytes::<MergeSessionRecord>(4)?, 64)?;
    let cursor = checked_mul(checked_add(id, 256)?, 4)?;
    checked_add(
        checked_add(storage, output_row_bytes(7)?)?,
        checked_add(checked_add(id, branch)?, checked_add(128, cursor)?)?,
    )
}

fn output_list_row(session: MergeSessionRecord, has_cursor: bool) -> ProcedureRow {
    let cursor = if has_cursor {
        Value::String(encode_list_cursor(&session.id))
    } else {
        Value::Null
    };
    row([
        ("session", Value::String(session.id)),
        ("targetBranch", Value::String(session.target_branch)),
        ("ours", commit_value(session.ours)),
        ("theirs", commit_value(session.theirs)),
        ("revision", Value::Integer(session.revision)),
        ("createdAt", Value::Integer(session.created_at)),
        ("cursor", cursor),
    ])
}

fn output_conflict_row(
    connection: &Connection,
    memory: &mut MemoryReservation,
    id: &str,
    revision: i64,
    conflict: MergeConflict,
    next: Option<usize>,
) -> QueryResult<ProcedureRow> {
    let text = checked_add(id.len() as u64, 64)?;
    let cursor = next.map_or(Ok(0), |_| {
        checked_mul(checked_add(id.len() as u64, 256)?, 4)
    })?;
    memory.grow(checked_add(text, cursor)?)?;
    let id = crate::query::expression::clone_string_controlled(id, Some(connection.control()))?;
    let cursor = next.map_or(Value::Null, |next| {
        Value::String(encode_conflict_cursor(&id, revision, next))
    });
    Ok(row([
        ("session", Value::String(id)),
        ("revision", Value::Integer(revision)),
        ("conflictId", Value::String(conflict.id.to_hex())),
        ("slot", Value::String(conflict.slot)),
        ("base", conflict.base.unwrap_or(Value::Null)),
        ("ours", conflict.ours.unwrap_or(Value::Null)),
        ("theirs", conflict.theirs.unwrap_or(Value::Null)),
        ("resolution", conflict.resolution.unwrap_or(Value::Null)),
        ("cursor", cursor),
    ]))
}

fn output_conflict_rows(
    connection: &Connection,
    page: ConflictPage,
    mut memory: MemoryReservation,
    id: &str,
    revision: i64,
    offset: usize,
    options: &ExecutionOptions,
) -> QueryResult<super::super::VersionRows> {
    memory.grow(checked_mul(
        page.conflicts.len() as u64,
        output_row_bytes(9)?,
    )?)?;
    let count = page.conflicts.len();
    let mut rows = Vec::with_capacity(count);
    for (index, conflict) in page.conflicts.into_iter().enumerate() {
        connection.checkpoint()?;
        super::super::access::check_slot_read(&conflict.slot, &options.access.read.query)?;
        let next = offset
            .checked_add(index)
            .and_then(|next| next.checked_add(1))
            .ok_or_else(|| QueryError::invalid_argument("merge conflict cursor is out of range"))?;
        rows.push(output_conflict_row(
            connection,
            &mut memory,
            id,
            revision,
            conflict,
            (index + 1 < count || page.has_more).then_some(next),
        )?);
    }
    Ok(super::super::VersionRows {
        value: rows,
        memory,
    })
}

fn output_length(value: i64) -> QueryResult<u64> {
    u64::try_from(value).map_err(|_| {
        QueryError::new(
            QueryErrorKind::Storage,
            "stored merge text length is invalid",
        )
    })
}

#[cfg(test)]
mod output_tests {
    use super::*;
    use crate::query::resources::{ExecutionControl, QueryResources};

    fn output_database() -> rusqlite::Connection {
        let connection = rusqlite::Connection::open_in_memory().expect("memory database");
        connection.execute_batch(
            "CREATE TABLE _lithograph_merge_sessions(id TEXT PRIMARY KEY, target_branch TEXT NOT NULL, ours_commit BLOB NOT NULL, theirs_commit BLOB NOT NULL, revision INTEGER NOT NULL, created_at INTEGER NOT NULL); INSERT INTO _lithograph_merge_sessions VALUES('session', 'main', zeroblob(32), zeroblob(32), 1, 1)",
        ).expect("synthetic merge session");
        connection
    }

    #[test]
    fn output_list_reserves_actual_rows_instead_of_requested_limit() {
        let database = output_database();
        let resources = QueryResources::new(Some(64 * 1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let connection = Connection::new(&database, &control);
        let rows = list(&connection, vec![Value::Integer(i64::MAX)]).expect("one actual row");
        assert_eq!(rows.len(), 1);
        assert!(resources.used() > 0);
        drop(rows);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn output_session_rejects_owned_record_before_loading_long_text() {
        let database = output_database();
        database.execute(
            "UPDATE _lithograph_merge_sessions SET target_branch = replace(hex(zeroblob(32768)), '00', 'xy')",
            [],
        ).expect("synthetic long stored text");
        let resources = QueryResources::new(Some(512));
        let control = ExecutionControl::new(&resources, &|| false);
        let connection = Connection::new(&database, &control);
        let error = output_session(&connection, "session").expect_err("preflight budget rejection");
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(resources.peak(), 0);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn output_conflicts_moves_owned_values_and_retains_their_reservation() {
        let database = output_database();
        let resources = QueryResources::new(Some(1024 * 1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let connection = Connection::new(&database, &control);
        let text = "x".repeat(64 * 1024);
        let allocation = text.as_ptr();
        let conflict = MergeConflict {
            id: HashId::from_bytes([0; 32]),
            slot: "node/1/property/title".to_owned(),
            base: Some(Value::String(text)),
            ours: None,
            theirs: None,
            resolution: None,
        };
        let memory = connection
            .reserve(budget::conflict_bytes(&connection, &conflict).unwrap())
            .unwrap();
        let page = ConflictPage {
            conflicts: vec![conflict],
            has_more: false,
        };
        let rows = output_conflict_rows(
            &connection,
            page,
            memory,
            "session",
            1,
            0,
            &ExecutionOptions::default(),
        )
        .expect("move conflict output");
        let Value::String(value) = &rows[0]["base"] else {
            panic!("base string")
        };
        assert_eq!(value.as_ptr(), allocation);
        assert!(resources.used() >= 64 * 1024);
        drop(rows);
        assert_eq!(resources.used(), 0);
    }
}
