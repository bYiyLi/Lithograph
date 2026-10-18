use super::*;

pub(in crate::query::version) fn start(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
    pinned_ours: HashId,
) -> QueryResult<Vec<ProcedureRow>> {
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
    Ok(vec![session_status_row(&session, &initial)])
}

pub(in crate::query::version) fn get(
    connection: &Connection,
    args: Vec<Value>,
) -> QueryResult<Vec<ProcedureRow>> {
    super::super::with_read_snapshot(connection, |connection| get_snapshot(connection, &args))
}

fn get_snapshot(connection: &Connection, args: &[Value]) -> QueryResult<Vec<ProcedureRow>> {
    let id = string_arg(args, 0, "session")?;
    let session = require_session(connection, id)?;
    let resolutions = load_resolutions(connection, id)?;
    let computation = compute_summary(connection, session.ours, session.theirs, &resolutions)?;
    Ok(vec![session_status_row(&session, &computation)])
}

pub(in crate::query::version) fn list(
    connection: &Connection,
    args: Vec<Value>,
) -> QueryResult<Vec<ProcedureRow>> {
    let limit = positive_limit(args.first(), 100, "merge.list limit")?;
    let after = args
        .get(1)
        .map(|value| string_value(value, "cursor"))
        .transpose()?
        .map(decode_list_cursor)
        .transpose()?;
    let mut sessions =
        storage::list_merge_sessions_after(connection, after.as_deref(), limit.saturating_add(1))?;
    let has_more = sessions.len() > limit;
    if has_more {
        sessions.truncate(limit);
    }
    let last = sessions.last().map(|session| session.id.clone());
    sessions
        .into_iter()
        .map(|session| {
            let cursor = if has_more && Some(&session.id) == last.as_ref() {
                Value::String(encode_list_cursor(&session.id))
            } else {
                Value::Null
            };
            Ok(row([
                ("session", Value::String(session.id)),
                ("targetBranch", Value::String(session.target_branch)),
                ("ours", commit_value(session.ours)),
                ("theirs", commit_value(session.theirs)),
                ("revision", Value::Integer(session.revision)),
                ("createdAt", Value::Integer(session.created_at)),
                ("cursor", cursor),
            ]))
        })
        .collect()
}

pub(in crate::query::version) fn conflicts(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
) -> QueryResult<Vec<ProcedureRow>> {
    super::super::with_read_snapshot(connection, |connection| {
        conflicts_snapshot(connection, &args, options)
    })
}

fn conflicts_snapshot(
    connection: &Connection,
    args: &[Value],
    options: &ExecutionOptions,
) -> QueryResult<Vec<ProcedureRow>> {
    let id = string_arg(args, 0, "session")?;
    let session = require_session(connection, id)?;
    let limit = positive_limit(args.get(1), 100, "merge.conflicts limit")?;
    let offset = args
        .get(2)
        .map(|value| string_value(value, "cursor"))
        .transpose()?
        .map(|cursor| decode_conflict_cursor(cursor, id, session.revision))
        .transpose()?
        .unwrap_or(0);
    let resolutions = load_resolutions(connection, id)?;
    let page = conflict_page(
        connection,
        session.ours,
        session.theirs,
        &resolutions,
        offset,
        limit,
    )?;
    page.conflicts
        .iter()
        .enumerate()
        .map(|(index, conflict)| {
            super::super::access::check_slot_read(&conflict.slot, &options.access.read.query)?;
            let next = offset + index + 1;
            let cursor = if index + 1 < page.conflicts.len() || page.has_more {
                Value::String(encode_conflict_cursor(id, session.revision, next))
            } else {
                Value::Null
            };
            Ok(row([
                ("session", Value::String(id.to_owned())),
                ("revision", Value::Integer(session.revision)),
                ("conflictId", Value::String(conflict.id.to_hex())),
                ("slot", Value::String(conflict.slot.clone())),
                ("base", conflict.base.clone().unwrap_or(Value::Null)),
                ("ours", conflict.ours.clone().unwrap_or(Value::Null)),
                ("theirs", conflict.theirs.clone().unwrap_or(Value::Null)),
                (
                    "resolution",
                    conflict.resolution.clone().unwrap_or(Value::Null),
                ),
                ("cursor", cursor),
            ]))
        })
        .collect()
}

pub(in crate::query::version) fn resolve(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
) -> QueryResult<Vec<ProcedureRow>> {
    let id = string_arg(&args, 0, "session")?.to_owned();
    let expected_revision = integer_arg(&args, 1, "expectedRevision")?;
    let session = require_session(connection, &id)?;
    ensure_revision(&session, expected_revision)?;
    let current = load_resolutions(connection, &id)?;
    let requested = resolution_ids(args.get(2))?;
    let known = conflicts_for_ids(
        connection,
        session.ours,
        session.theirs,
        &current,
        &requested,
    )?;
    let incoming = resolution_items(args.get(2), &known)?;
    check_resolution_access(&known, &incoming, options)?;
    let (proposed, changed) = proposed_resolutions(&current, &incoming);
    let proposed_computation =
        compute_summary(connection, session.ours, session.theirs, &proposed)?;
    let encoded = incoming
        .iter()
        .map(|(id, value)| Ok((*id, encode_resolution(value)?)))
        .collect::<QueryResult<BTreeMap<_, _>>>()?;
    let resulting_revision =
        storage::update_merge_resolutions(connection, &id, expected_revision, &encoded, changed)
            .map_err(|error| map_session_cas_error(connection, &id, error))?;
    Ok(vec![row([
        ("session", Value::String(id)),
        ("revision", Value::Integer(resulting_revision)),
        (
            "status",
            Value::String(proposed_computation.status.to_owned()),
        ),
        (
            "unresolved",
            Value::Integer(i64::try_from(proposed_computation.unresolved).unwrap_or(i64::MAX)),
        ),
    ])])
}

fn check_resolution_access(
    known: &BTreeMap<HashId, MergeConflict>,
    incoming: &BTreeMap<HashId, Value>,
    options: &ExecutionOptions,
) -> QueryResult<()> {
    for conflict in known.values() {
        let next = resolved_value(conflict, incoming)?;
        super::super::access::check_resolution_slot(
            &conflict.slot,
            conflict.ours.as_ref(),
            next.as_ref(),
            &options.access,
        )?;
    }
    Ok(())
}

fn proposed_resolutions(
    current: &BTreeMap<HashId, Value>,
    incoming: &BTreeMap<HashId, Value>,
) -> (BTreeMap<HashId, Value>, bool) {
    let mut proposed = current.clone();
    let mut changed = false;
    for (conflict_id, resolution) in incoming {
        if !same_optional_value(proposed.get(conflict_id), Some(resolution)) {
            changed = true;
            proposed.insert(*conflict_id, resolution.clone());
        }
    }
    (proposed, changed)
}

pub(in crate::query::version) fn finalize(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
) -> QueryResult<Vec<ProcedureRow>> {
    #[cfg(feature = "test-support")]
    let prepare_started = std::time::Instant::now();
    let (id, expected_revision, session, action) = prepare_finalize(connection, &args)?;
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
    Ok(vec![row([
        ("status", Value::String(status.to_owned())),
        ("commit", commit_value(commit)),
    ])])
}

type PreparedFinalize = (String, i64, MergeSessionRecord, FinalizeAction);
fn prepare_finalize(connection: &Connection, args: &[Value]) -> QueryResult<PreparedFinalize> {
    let id = string_arg(args, 0, "session")?.to_owned();
    let expected_revision = integer_arg(args, 1, "expectedRevision")?;
    let session = require_session(connection, &id)?;
    ensure_revision(&session, expected_revision)?;
    let resolutions = load_resolutions(connection, &id)?;
    let computation = compute_summary(connection, session.ours, session.theirs, &resolutions)?;
    if computation.unresolved != 0 {
        return Err(merge_conflict());
    }
    let action = finalize_action_for_computation(connection, &session, computation, &resolutions)?;
    Ok((id, expected_revision, session, action))
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
    candidate: Option<SnapshotState>,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<FinalizeAction> {
    if let Some(candidate) = candidate {
        let ours = storage::load_snapshot_state(connection, session.ours)?;
        return Ok(FinalizeAction::Merge {
            layer: storage::layer_between(&ours, &candidate)?,
            schema_blob: candidate.schema.canonical_blob()?,
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
        schema_blob: schema.canonical_blob()?,
    })
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
            let previous = storage::SchemaState::load(connection, session.ours)?;
            let schema = storage::SchemaState::from_canonical_blob(&schema_blob)?;
            super::super::access::check_layer(
                connection,
                session.ours,
                &layer,
                &schema,
                &options.access,
            )?;
            super::super::access::check_schema_validation(&previous, &schema, &options.access)?;
            let schema_hash = qs::persist_validated_blob(connection, session.ours, &schema_blob)?;
            let metadata = CommitMetadata {
                author: options.author.clone(),
                message: options.message.clone(),
                committed_at: now_micros()?,
            };
            let commit = storage::commit_layer_with_schema(
                connection,
                &session.target_branch,
                session.ours,
                Some(session.theirs),
                &layer,
                schema_hash,
                &metadata,
            )?;
            Ok(("merged", commit))
        }
    }
}

pub(in crate::query::version) fn abort(
    connection: &Connection,
    args: Vec<Value>,
) -> QueryResult<Vec<ProcedureRow>> {
    let id = string_arg(&args, 0, "session")?.to_owned();
    let expected_revision = integer_arg(&args, 1, "expectedRevision")?;
    match storage::delete_merge_session(connection, &id, expected_revision) {
        Ok(true) => Ok(vec![row([("session", Value::String(id))])]),
        Ok(false) => Err(session_not_found(&id)),
        Err(error) => Err(map_session_cas_error(connection, &id, error)),
    }
}
