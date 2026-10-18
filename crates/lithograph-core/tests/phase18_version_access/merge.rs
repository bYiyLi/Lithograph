use super::*;

fn start(connection: &Connection) -> (String, i64, String) {
    let rows = call(
        connection,
        "CALL lithograph.merge.start('branch/source') YIELD session, revision, status RETURN session, revision, status",
    );
    (
        string(&rows[0][0]).to_owned(),
        integer(&rows[0][1]),
        string(&rows[0][2]).to_owned(),
    )
}

fn finalize(session: &str, revision: i64) -> String {
    format!("CALL lithograph.merge.finalize('{session}', {revision}) YIELD status RETURN status")
}

fn revision(connection: &Connection, session: &str) -> i64 {
    integer(
        &call(
            connection,
            &format!("CALL lithograph.merge.get('{session}') YIELD revision RETURN revision"),
        )[0][0],
    )
}

fn conflict_fixture() -> (Connection, String, String) {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {secret:'base'}) FINISH");
    let base = branch_head(&connection, "main").expect("base");
    branch(&connection, "source", base);
    call(&connection, "MATCH (n:Item) SET n.secret='ours' FINISH");
    execute(
        &connection,
        "MATCH (n:Item) SET n.secret='theirs' FINISH",
        r#"{"branch":"source"}"#,
    )
    .expect("source conflict");
    let (session, _, status) = start(&connection);
    assert_eq!(status, "conflicted");
    let conflicts = call(
        &connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 10) YIELD conflictId RETURN conflictId"
        ),
    );
    (connection, session, string(&conflicts[0][0]).to_owned())
}

#[test]
fn fast_forward_finalize_rechecks_this_invocations_policy_and_preserves_session_on_denial() {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {secret:0, title:0}) FINISH");
    let base = branch_head(&connection, "main").expect("base");
    branch(&connection, "source", base);
    execute(
        &connection,
        "MATCH (n:Item) SET n.secret=1 FINISH",
        r#"{"branch":"source"}"#,
    )
    .expect("trusted source write");
    let source = branch_head(&connection, "source").expect("source");
    let (session, session_revision, status) = start(&connection);
    assert_eq!(status, "fast_forward");
    let query = finalize(&session, session_revision);
    assert_denied_unchanged(
        &connection,
        &query,
        r#"{"writeAccess":{"denyPropertyPatterns":["secret"]}}"#,
        "main",
    );
    assert_eq!(revision(&connection, &session), session_revision);
    execute(
        &connection,
        &query,
        r#"{"writeAccess":{"denyProperties":["title"],"allowCreate":false,"allowDelete":false}}"#,
    )
    .expect("permitted fast-forward is not blanket denied");
    assert_eq!(
        branch_head(&connection, "main").expect("final head"),
        source
    );
}

#[test]
fn conflict_resolution_protects_explicit_same_value_requests_and_keeps_revision_on_error() {
    let (connection, session, conflict) = conflict_fixture();
    for choice in ["ours", "theirs"] {
        let query = format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{conflict}', choice:'{choice}'}}]) YIELD revision RETURN revision"
        );
        assert_denied_unchanged(
            &connection,
            &query,
            r#"{"writeAccess":{"denyProperties":["secret"]}}"#,
            "main",
        );
        assert_eq!(revision(&connection, &session), 1);
    }
    call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{conflict}', choice:'ours'}}]) YIELD revision RETURN revision"
        ),
    );
    execute(
        &connection,
        &finalize(&session, 2),
        r#"{"writeAccess":{"denyProperties":["secret"]}}"#,
    )
    .expect("resolved candidate leaves protected effective value unchanged");
    assert_eq!(
        call(&connection, "MATCH (n:Item) RETURN n.secret"),
        vec![vec![Value::String("ours".into())]]
    );
}

#[test]
fn ready_merge_finalize_rechecks_protected_effective_changes_after_earlier_resolution() {
    let (connection, session, conflict) = conflict_fixture();
    call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{conflict}', choice:'theirs'}}]) YIELD revision RETURN revision"
        ),
    );
    assert_denied_unchanged(
        &connection,
        &finalize(&session, 2),
        r#"{"writeAccess":{"denyProperties":["secret"]}}"#,
        "main",
    );
    assert_eq!(revision(&connection, &session), 2);
    call(&connection, &finalize(&session, 2));
    assert_eq!(
        call(&connection, "MATCH (n:Item) RETURN n.secret"),
        vec![vec![Value::String("theirs".into())]]
    );
}

#[test]
fn merge_conflict_records_are_rejected_if_they_expose_query_hidden_property_values() {
    let (connection, session, _) = conflict_fixture();
    let query = format!(
        "CALL lithograph.merge.conflicts('{session}', 10) YIELD conflictId, base, ours, theirs RETURN conflictId, base, ours, theirs"
    );
    let full = call(&connection, &query);
    assert_denied_unchanged(
        &connection,
        &query,
        r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#,
        "main",
    );
    let returned = execute(
        &connection,
        &query,
        r#"{"readAccess":{"result":{"denyProperties":["secret","base"]}}}"#,
    )
    .expect("opaque records remain complete under result-only rules")
    .0;
    assert_eq!(returned, full);
    assert_eq!(revision(&connection, &session), 1);
}

#[test]
fn merge_finalize_create_delete_label_and_schema_permissions_use_actual_graph_effects() {
    for (change, capability) in [
        ("CREATE (:Extra) FINISH", "allowCreate"),
        ("MATCH (n:Item) DELETE n FINISH", "allowDelete"),
        ("MATCH (n:Item) SET n:Published FINISH", "allowLabelChanges"),
        (
            "CREATE RANGE INDEX item_title FOR (n:Item) ON (n.title)",
            "allowSchemaChanges",
        ),
    ] {
        let connection = fresh_storage();
        call(&connection, "CREATE (:Item {title:'base'}) FINISH");
        let base = branch_head(&connection, "main").expect("base");
        branch(&connection, "source", base);
        execute(&connection, change, r#"{"branch":"source"}"#).expect("source change");
        let (session, session_revision, _) = start(&connection);
        let query = finalize(&session, session_revision);
        assert_denied_unchanged(
            &connection,
            &query,
            &format!(r#"{{"writeAccess":{{"{capability}":false}}}}"#),
            "main",
        );
        assert_eq!(revision(&connection, &session), session_revision);
        call(&connection, &query);
        assert_eq!(
            load_snapshot_state(&connection, branch_head(&connection, "main").expect("main"))
                .expect("main state"),
            load_snapshot_state(
                &connection,
                branch_head(&connection, "source").expect("source")
            )
            .expect("source state")
        );
    }
}

#[test]
fn session_abort_and_resolve_require_version_permission_without_consuming_session() {
    let (connection, session, conflict) = conflict_fixture();
    let resolve = format!(
        "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{conflict}', choice:'ours'}}]) YIELD revision RETURN revision"
    );
    let abort = format!("CALL lithograph.merge.abort('{session}', 1) YIELD session RETURN session");
    for query in [&resolve, &abort] {
        assert_denied_unchanged(
            &connection,
            query,
            r#"{"writeAccess":{"allowVersionChanges":false}}"#,
            "main",
        );
        assert_eq!(revision(&connection, &session), 1);
    }
    call(&connection, &resolve);
    assert_denied_unchanged(
        &connection,
        &finalize(&session, 2),
        r#"{"writeAccess":{"allowVersionChanges":false}}"#,
        "main",
    );
    execute(
        &connection,
        &format!("CALL lithograph.merge.abort('{session}', 2) YIELD session RETURN session"),
        r#"{"writeAccess":{"denyProperties":["secret"],"allowDelete":false}}"#,
    )
    .expect("aborting a Session does not delete graph entities");
}

#[test]
fn relationship_property_with_label_like_name_is_not_a_label_change() {
    let connection = fresh_storage();
    call(
        &connection,
        "CREATE (:Item)-[:LINK {`foo/label/bar`:'base'}]->(:Item) FINISH",
    );
    let base = branch_head(&connection, "main").expect("base");
    branch(&connection, "source", base);
    call(
        &connection,
        "MATCH ()-[r:LINK]->() SET r.`foo/label/bar`='ours' FINISH",
    );
    execute(
        &connection,
        "MATCH ()-[r:LINK]->() SET r.`foo/label/bar`='theirs' FINISH",
        r#"{"branch":"source"}"#,
    )
    .expect("source relationship conflict");
    let (session, _, status) = start(&connection);
    assert_eq!(status, "conflicted");
    let conflicts = call(
        &connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 10) YIELD conflictId RETURN conflictId"
        ),
    );
    let conflict = string(&conflicts[0][0]);
    let options = r#"{"writeAccess":{"allowLabelChanges":false}}"#;
    execute(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{conflict}', choice:'theirs'}}]) YIELD revision RETURN revision"
        ),
        options,
    )
    .expect("an attribute whose name contains label syntax is still a property");
    execute(&connection, &finalize(&session, 2), options).expect("permitted attribute merge");
    assert_eq!(
        call(
            &connection,
            "MATCH ()-[r:LINK]->() RETURN r.`foo/label/bar`"
        ),
        vec![vec![Value::String("theirs".into())]]
    );
}
