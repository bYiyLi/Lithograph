use super::*;

#[test]
fn open_merge_session_is_a_gc_root_until_abort() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:N {v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base");
    let base = branch_head(&connection, "main").expect("base");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('ephemeral', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:N) SET n.v=1 FINISH",
        options(r#"{"branch":"ephemeral"}"#),
    )
    .expect("ephemeral write");
    let ephemeral = branch_head(&connection, "ephemeral").expect("ephemeral head");
    let started = call(
        &connection,
        "CALL lithograph.merge.start('branch/ephemeral') YIELD session, revision, status RETURN session, revision, status",
    );
    assert_eq!(started[0][2], Value::String("fast_forward".to_owned()));
    let session = string(&started[0][0]).to_owned();
    let revision = integer(&started[0][1]);
    call(
        &connection,
        "CALL lithograph.branch.delete('ephemeral') YIELD name RETURN name",
    );
    call(
        &connection,
        "CALL lithograph.gc() YIELD commits RETURN commits",
    );
    assert!(commit_exists(&connection, ephemeral).expect("session-protected Commit exists"));
    call(
        &connection,
        &format!(
            "CALL lithograph.merge.abort('{session}', {revision}) YIELD session RETURN session"
        ),
    );
    call(
        &connection,
        "CALL lithograph.gc() YIELD commits RETURN commits",
    );
    assert!(!commit_exists(&connection, ephemeral).expect("unreachable Commit collected"));
}

#[test]
fn fast_forward_session_defers_ref_move_and_pins_source_commit() {
    let connection = fresh_storage();
    let base = branch_head(&connection, "main").expect("root");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('target', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('source', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "CREATE (:FF {v:1}) FINISH",
        options(r#"{"branch":"source"}"#),
    )
    .expect("source write");
    let pinned_source = branch_head(&connection, "source").expect("pinned source");
    let before = commit_count(&connection);
    let started = execute(
        &connection,
        "CALL lithograph.merge.start('branch/source') YIELD session, revision, status, ours, theirs RETURN session, revision, status, ours, theirs",
        options(r#"{"branch":"target"}"#),
    )
    .expect("fast-forward start")
    .0;
    assert_eq!(started[0][2], Value::String("fast_forward".to_owned()));
    assert_eq!(
        branch_head(&connection, "target").expect("target unchanged"),
        base
    );
    assert_eq!(commit_count(&connection), before);
    assert_eq!(string(&started[0][4]), descriptor(pinned_source));
    let session = string(&started[0][0]).to_owned();
    let revision = integer(&started[0][1]);
    let candidate = execute(
        &connection,
        "MATCH (n:FF) RETURN n.v",
        options(&format!(
            r#"{{"mergeSession":{{"id":"{session}","revision":{revision}}}}}"#
        )),
    )
    .expect("fast-forward candidate")
    .0;
    assert_eq!(candidate, vec![vec![Value::Integer(1)]]);
    execute(
        &connection,
        "CREATE (:FF {v:2}) FINISH",
        options(r#"{"branch":"source"}"#),
    )
    .expect("source moves after start");
    assert_ne!(
        branch_head(&connection, "source").expect("moved source"),
        pinned_source
    );
    let finalized = call(
        &connection,
        &format!(
            "CALL lithograph.merge.finalize('{session}', {revision}) YIELD status, commit RETURN status, commit"
        ),
    );
    assert_eq!(finalized[0][0], Value::String("fast_forward".to_owned()));
    assert_eq!(string(&finalized[0][1]), descriptor(pinned_source));
    assert_eq!(
        branch_head(&connection, "target").expect("target advanced"),
        pinned_source
    );
    assert_eq!(commit_count(&connection), before + 1); // only the post-start source write
    let removed = execute(
        &connection,
        &format!("CALL lithograph.merge.get('{session}') YIELD session RETURN session"),
        ExecutionOptions::default(),
    )
    .expect_err("finalized Session must be deleted");
    assert_eq!(removed.kind, QueryErrorKind::MergeSessionNotFound);
}

#[test]
fn merge_start_expected_head_and_finalize_head_cas_are_enforced() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Cas {v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base");
    let base = branch_head(&connection, "main").expect("base");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('cas-source', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Cas) SET n.v=1 FINISH",
        options(r#"{"branch":"cas-source"}"#),
    )
    .expect("source change");
    let stale_existing = load_commit(&connection, base)
        .expect("base record")
        .parent1
        .expect("base parent");
    let session_count_before: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_merge_sessions",
            [],
            |row| row.get(0),
        )
        .expect("session count");
    let mismatch = execute(
        &connection,
        &format!(
            "CALL lithograph.merge.start('branch/cas-source', '{}') YIELD session RETURN session",
            descriptor(stale_existing)
        ),
        ExecutionOptions::default(),
    )
    .expect_err("expectedHead mismatch");
    assert_eq!(mismatch.kind, QueryErrorKind::BranchHeadMoved);
    let session_count_after: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_merge_sessions",
            [],
            |row| row.get(0),
        )
        .expect("session count after mismatch");
    assert_eq!(session_count_after, session_count_before);

    let started = call(
        &connection,
        &format!(
            "CALL lithograph.merge.start('branch/cas-source', '{}') YIELD session, revision RETURN session, revision",
            descriptor(base)
        ),
    );
    let session = string(&started[0][0]).to_owned();
    execute(
        &connection,
        "CREATE (:CasMoved) FINISH",
        ExecutionOptions::default(),
    )
    .expect("move target head");
    let moved = execute(
        &connection,
        &format!("CALL lithograph.merge.finalize('{session}', 1) YIELD status RETURN status"),
        ExecutionOptions::default(),
    )
    .expect_err("finalize after head move");
    assert_eq!(moved.kind, QueryErrorKind::BranchHeadMoved);
    let recovered = call(
        &connection,
        &format!("CALL lithograph.merge.get('{session}') YIELD revision RETURN revision"),
    );
    assert_eq!(recovered[0][0], Value::Integer(1));
}

#[test]
fn merge_list_is_bounded_and_up_to_date_finalize_creates_no_commit() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:UpToDate {v:7}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("up-to-date base");
    let head = branch_head(&connection, "main").expect("head");
    for index in 0..3 {
        call(
            &connection,
            &format!(
                "CALL lithograph.branch.create('list-{index}', '{}') YIELD name RETURN name",
                descriptor(head)
            ),
        );
        execute(
            &connection,
            &format!(
                "CALL lithograph.merge.start('branch/list-{index}') YIELD session RETURN session"
            ),
            ExecutionOptions::default(),
        )
        .expect("start up-to-date session");
    }
    let first = call(
        &connection,
        "CALL lithograph.merge.list(2) YIELD session, cursor RETURN session, cursor",
    );
    assert_eq!(first.len(), 2);
    let cursor = string(&first[1][1]).to_owned();
    let second = call(
        &connection,
        &format!(
            "CALL lithograph.merge.list(2, '{cursor}') YIELD session, cursor RETURN session, cursor"
        ),
    );
    assert_eq!(second.len(), 1);
    assert_eq!(second[0][1], Value::Null);
    let before = commit_count(&connection);
    let session = string(&first[0][0]).to_owned();
    let candidate = execute(
        &connection,
        "MATCH (n:UpToDate) RETURN n.v",
        options(&format!(
            r#"{{"mergeSession":{{"id":"{session}","revision":1}}}}"#
        )),
    )
    .expect("up-to-date candidate")
    .0;
    assert_eq!(candidate, vec![vec![Value::Integer(7)]]);
    let finalized = call(
        &connection,
        &format!(
            "CALL lithograph.merge.finalize('{session}', 1) YIELD status, commit RETURN status, commit"
        ),
    );
    assert_eq!(finalized[0][0], Value::String("up_to_date".to_owned()));
    assert_eq!(string(&finalized[0][1]), descriptor(head));
    assert_eq!(commit_count(&connection), before);
}

#[test]
fn finalize_missing_target_branch_preserves_merge_session() {
    let connection = fresh_storage();
    let base = branch_head(&connection, "main").expect("base");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('gone-target', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('gone-source', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "CREATE (:GoneSource) FINISH",
        options(r#"{"branch":"gone-source"}"#),
    )
    .expect("source write");
    let started = execute(
        &connection,
        "CALL lithograph.merge.start('branch/gone-source') YIELD session, revision RETURN session, revision",
        options(r#"{"branch":"gone-target"}"#),
    )
    .expect("merge start")
    .0;
    let session = string(&started[0][0]).to_owned();
    call(
        &connection,
        "CALL lithograph.branch.delete('gone-target') YIELD name RETURN name",
    );
    let error = execute(
        &connection,
        &format!("CALL lithograph.merge.finalize('{session}', 1) YIELD status RETURN status"),
        ExecutionOptions::default(),
    )
    .expect_err("deleted target must fail finalize");
    assert_eq!(error.kind, QueryErrorKind::BranchNotFound);
    let recovered = call(
        &connection,
        &format!("CALL lithograph.merge.get('{session}') YIELD revision RETURN revision"),
    );
    assert_eq!(recovered[0][0], Value::Integer(1));
}

#[test]
fn revert_merge_commit_requires_mainline_and_inverts_selected_parent() {
    let (connection, _, session, revision, _) = ready_merge_fixture();
    call(
        &connection,
        &format!(
            "CALL lithograph.merge.finalize('{session}', {revision}) YIELD commit RETURN commit"
        ),
    );
    let merged = branch_head(&connection, "main").expect("merge head");
    let record = load_commit(&connection, merged).expect("merge record");
    let first_parent = record.parent1.expect("first parent");
    let second_parent = record.parent2.expect("second parent");
    let missing = execute(
        &connection,
        &format!(
            "CALL lithograph.revert('{}') YIELD commit RETURN commit",
            descriptor(merged)
        ),
        ExecutionOptions::default(),
    )
    .expect_err("merge revert needs mainline");
    assert_eq!(missing.kind, QueryErrorKind::InvalidArgument);
    let reverted = call(
        &connection,
        &format!(
            "CALL lithograph.revert('{}', {{mainline:1}}) YIELD commit RETURN commit",
            descriptor(merged)
        ),
    );
    let revert_commit = HashId::from_hex(&string(&reverted[0][0])[7..]).expect("revert id");
    assert_eq!(
        snapshot(&connection, revert_commit),
        snapshot(&connection, first_parent)
    );
    call(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(merged)
        ),
    );
    let reverted_second = call(
        &connection,
        &format!(
            "CALL lithograph.revert('{}', {{mainline:2}}) YIELD commit RETURN commit",
            descriptor(merged)
        ),
    );
    let second_revert_commit =
        HashId::from_hex(&string(&reverted_second[0][0])[7..]).expect("second revert id");
    assert_eq!(
        snapshot(&connection, second_revert_commit),
        snapshot(&connection, second_parent)
    );
}

#[test]
fn merge_option_boundaries_and_error_precedence_are_stable() {
    let (connection, session, conflict_id, _) = property_conflict_fixture();
    let missing = execute(
        &connection,
        "CALL lithograph.merge.get('merge-session/00000000-0000-4000-8000-000000000000') YIELD session RETURN session",
        ExecutionOptions::default(),
    )
    .expect_err("missing Session");
    assert_eq!(missing.kind, QueryErrorKind::MergeSessionNotFound);
    let branch_option = execute(
        &connection,
        &format!("CALL lithograph.merge.get('{session}') YIELD session RETURN session"),
        options(r#"{"branch":"main"}"#),
    )
    .expect_err("merge.get branch option");
    assert_eq!(branch_option.kind, QueryErrorKind::InvalidArgument);
    let metadata_option = execute(
        &connection,
        "CALL lithograph.merge.start('branch/conflict') YIELD session RETURN session",
        options(r#"{"author":"not-allowed"}"#),
    )
    .expect_err("merge.start metadata option");
    assert_eq!(metadata_option.kind, QueryErrorKind::InvalidArgument);
    let unresolved = execute(
        &connection,
        &format!("CALL lithograph.merge.finalize('{session}', 1) YIELD status RETURN status"),
        ExecutionOptions::default(),
    )
    .expect_err("unresolved finalize");
    assert_eq!(unresolved.kind, QueryErrorKind::MergeConflict);
    let resolved = call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{conflict_id}', choice:'ours'}}]) YIELD revision RETURN revision"
        ),
    );
    let revision = integer(&resolved[0][0]);
    let stale = execute(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{conflict_id}', choice:'ours'}}]) YIELD revision RETURN revision"
        ),
        ExecutionOptions::default(),
    )
    .expect_err("stale resolve");
    assert_eq!(stale.kind, QueryErrorKind::MergeSessionChanged);
    let stale_abort = execute(
        &connection,
        &format!("CALL lithograph.merge.abort('{session}', 1) YIELD session RETURN session"),
        ExecutionOptions::default(),
    )
    .expect_err("stale abort");
    assert_eq!(stale_abort.kind, QueryErrorKind::MergeSessionChanged);
    let still_open = call(
        &connection,
        &format!("CALL lithograph.merge.get('{session}') YIELD revision RETURN revision"),
    );
    assert_eq!(still_open[0][0], Value::Integer(revision));
    call(
        &connection,
        &format!(
            "CALL lithograph.merge.abort('{session}', {revision}) YIELD session RETURN session"
        ),
    );
}

#[test]
fn relationship_endpoint_dependency_conflict_is_resolvable() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (a:A {name:'a'}), (b:B {name:'b'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base nodes");
    let base = branch_head(&connection, "main").expect("base");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('dep-source', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (a:A) DETACH DELETE a FINISH",
        ExecutionOptions::default(),
    )
    .expect("ours delete endpoint");
    execute(
        &connection,
        "MATCH (a:A), (b:B) CREATE (a)-[:LINK {v:1}]->(b) FINISH",
        options(r#"{"branch":"dep-source"}"#),
    )
    .expect("theirs relationship");
    let started = call(
        &connection,
        "CALL lithograph.merge.start('branch/dep-source') YIELD session, status, unresolved RETURN session, status, unresolved",
    );
    assert_eq!(started[0][1], Value::String("conflicted".to_owned()));
    let session = string(&started[0][0]).to_owned();
    let conflicts = call(
        &connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 20) YIELD conflictId, slot RETURN conflictId, slot"
        ),
    );
    assert!(
        conflicts
            .iter()
            .any(|row| string(&row[1]).starts_with("relationship/"))
    );
    assert!(conflicts.iter().any(|row| string(&row[1]) == "node/1"));
    let resolutions = conflicts
        .iter()
        .map(|row| format!("{{conflictId:'{}', choice:'theirs'}}", string(&row[0])))
        .collect::<Vec<_>>()
        .join(",");
    let resolved = call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{resolutions}]) YIELD revision, unresolved RETURN revision, unresolved"
        ),
    );
    assert_eq!(resolved[0][1], Value::Integer(0));
    let revision = integer(&resolved[0][0]);
    let candidate = execute(
        &connection,
        "MATCH (a:A)-[r:LINK]->(b:B) RETURN a.name, b.name, r.v",
        options(&format!(
            r#"{{"mergeSession":{{"id":"{session}","revision":{revision}}}}}"#
        )),
    )
    .expect("relationship dependency candidate")
    .0;
    assert_eq!(
        candidate,
        vec![vec![
            Value::String("a".to_owned()),
            Value::String("b".to_owned()),
            Value::Integer(1),
        ]]
    );
}

#[test]
fn schema_slots_surface_merge_conflicts() {
    let connection = fresh_storage();
    let schema_base = branch_head(&connection, "main").expect("schema base");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('schema-source', '{}') YIELD name RETURN name",
            descriptor(schema_base)
        ),
    );
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Typed => {value :: INTEGER}) }",
        ExecutionOptions::default(),
    )
    .expect("ours graph type");
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Typed => {value :: STRING}) }",
        options(r#"{"branch":"schema-source"}"#),
    )
    .expect("theirs graph type");
    let schema = call(
        &connection,
        "CALL lithograph.merge.start('branch/schema-source') YIELD session, status RETURN session, status",
    );
    assert_eq!(schema[0][1], Value::String("conflicted".to_owned()));
    let schema_session = string(&schema[0][0]);
    let schema_conflicts = call(
        &connection,
        &format!("CALL lithograph.merge.conflicts('{schema_session}', 20) YIELD slot RETURN slot"),
    );
    assert!(schema_conflicts.contains(&vec![Value::String("graph/node/Typed".to_owned())]));
}
