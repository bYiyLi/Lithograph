use super::*;

const PROTECT_EXISTING_STATE: &str = r#"{"writeAccess":{"denyProperties":["secret"],"allowCreate":false,"allowDelete":false,"allowLabelChanges":false,"allowSchemaChanges":false}}"#;

const EFFECTS: [(&str, &str); 4] = [
    ("CREATE () FINISH", "allowCreate"),
    ("MATCH (n:Victim) DELETE n FINISH", "allowDelete"),
    (
        "MATCH (n:Victim) SET n:Published FINISH",
        "allowLabelChanges",
    ),
    (
        "CREATE RANGE INDEX victim_title FOR (n:Victim) ON (n.title)",
        "allowSchemaChanges",
    ),
];

fn effect_fixture() -> Connection {
    let connection = fresh_storage();
    call(
        &connection,
        "CREATE (:Anchor {title:0, secret:'internal'}), (:Victim {title:'base', secret:'private'}) FINISH",
    );
    connection
}

fn canonical_counts(connection: &Connection) -> (i64, i64, i64, i64, i64) {
    connection
        .query_row(
            "SELECT (SELECT count(*) FROM main._lithograph_commits), \
                    (SELECT count(*) FROM main._lithograph_layers), \
                    (SELECT count(*) FROM main._lithograph_schema_objects), \
                    (SELECT count(*) FROM main._lithograph_merge_sessions), \
                    (SELECT count(*) FROM main._lithograph_merge_resolutions)",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("canonical object counts")
}

fn assert_effect_present(connection: &Connection, capability: &str, branch_name: &str) {
    let (query, expected) = match capability {
        "allowCreate" => ("MATCH (n) RETURN count(n)", 3),
        "allowDelete" => ("MATCH (n:Victim) RETURN count(n)", 0),
        "allowLabelChanges" => ("MATCH (n:Published) RETURN count(n)", 1),
        "allowSchemaChanges" => ("SHOW INDEXES YIELD name RETURN count(name)", 1),
        _ => panic!("unexpected capability {capability}"),
    };
    assert_eq!(
        execute(
            connection,
            query,
            &format!(r#"{{"at":"branch/{branch_name}"}}"#),
        )
        .expect("effective state")
        .0,
        vec![vec![Value::Integer(expected)]],
        "{capability}"
    );
}

fn rebase_query() -> &'static str {
    "CALL lithograph.rebase('branch/main') YIELD status, rewritten RETURN status, rewritten"
}

fn assert_replayed(rows: &[Vec<Value>], count: usize) {
    assert_eq!(rows[0][0], Value::String("rebased".into()));
    let Value::List(rewritten) = &rows[0][1] else {
        panic!("rewritten Commit mapping");
    };
    assert_eq!(rewritten.len(), count);
}

#[test]
fn non_fast_forward_merge_rechecks_all_effect_permissions_without_consuming_ready_session() {
    for (change, capability) in EFFECTS {
        let connection = effect_fixture();
        let base = branch_head(&connection, "main").expect("base");
        branch(&connection, "source", base);
        call(&connection, "MATCH (n:Anchor) SET n.title=1 FINISH");
        execute(&connection, change, r#"{"branch":"source"}"#).expect("source change");
        let source = branch_head(&connection, "source").expect("source head");
        let started = call(
            &connection,
            "CALL lithograph.merge.start('branch/source') YIELD session, revision, status RETURN session, revision, status",
        );
        assert_eq!(started[0][2], Value::String("ready".into()));
        let session = string(&started[0][0]);
        let revision = integer(&started[0][1]);
        let inspect = format!(
            "CALL lithograph.merge.get('{session}') YIELD revision, status RETURN revision, status"
        );
        let session_before = call(&connection, &inspect);
        let counts = canonical_counts(&connection);
        let query = format!(
            "CALL lithograph.merge.finalize('{session}', {revision}) YIELD status RETURN status"
        );
        assert_denied_unchanged(
            &connection,
            &query,
            &format!(r#"{{"writeAccess":{{"{capability}":false}}}}"#),
            "main",
        );
        assert_eq!(canonical_counts(&connection), counts);
        assert_eq!(call(&connection, &inspect), session_before);
        execute(
            &connection,
            &query,
            &format!(r#"{{"writeAccess":{{"{capability}":true,"denyProperties":["secret"]}}}}"#),
        )
        .expect("permitted non-fast-forward Merge");
        let merged = branch_head(&connection, "main").expect("merged head");
        assert_eq!(
            lithograph_core::storage::load_commit(&connection, merged)
                .expect("Merge Commit")
                .parent2,
            Some(source)
        );
        assert_eq!(branch_head(&connection, "source").expect("source"), source);
        assert_eq!(
            call(&connection, "MATCH (n:Anchor) RETURN n.title, n.secret"),
            vec![vec![Value::Integer(1), Value::String("internal".into())]]
        );
        assert_effect_present(&connection, capability, "main");
        assert_eq!(canonical_counts(&connection).3, 0);
    }
}

#[test]
fn schema_only_patch_is_independent_of_graph_property_and_entity_permissions() {
    for change in [
        "CREATE RANGE INDEX victim_title FOR (n:Victim) ON (n.title)",
        "CREATE CONSTRAINT victim_title_unique FOR (n:Victim) REQUIRE n.title IS UNIQUE",
    ] {
        let connection = effect_fixture();
        let base = branch_head(&connection, "main").expect("base");
        call(&connection, change);
        let changed = branch_head(&connection, "main").expect("Schema Commit");
        let expected = load_snapshot_state(&connection, changed).expect("changed state");
        let patch = super::patch::diff(&connection, base, changed);
        call(
            &connection,
            &format!(
                "CALL lithograph.reset('{}') YIELD to RETURN to",
                descriptor(base)
            ),
        );
        let before = load_snapshot_state(&connection, base).expect("pre-Patch state");
        let counts = canonical_counts(&connection);
        let error = super::patch::apply(&connection, patch.clone(), PROTECT_EXISTING_STATE)
            .expect_err("Schema permission is independently enforced");
        assert_eq!(error.kind, QueryErrorKind::AccessDenied);
        assert_eq!(branch_head(&connection, "main").expect("head"), base);
        assert_eq!(canonical_counts(&connection), counts);
        assert_eq!(
            load_snapshot_state(
                &connection,
                branch_head(&connection, "main").expect("preserved head")
            )
            .expect("preserved state"),
            before
        );
        super::patch::apply(
            &connection,
            patch,
            r#"{"writeAccess":{"denyPropertyPatterns":[".*"],"allowCreate":false,"allowDelete":false,"allowLabelChanges":false,"allowSchemaChanges":true}}"#,
        )
        .expect("Schema Patch has no graph-property or entity mutation");
        assert_eq!(
            load_snapshot_state(
                &connection,
                branch_head(&connection, "main").expect("applied head")
            )
            .expect("applied state"),
            expected
        );
    }
}

#[test]
fn rebase_replays_entity_label_and_schema_history_when_final_restricted_effects_are_unchanged() {
    for (change, capability) in EFFECTS {
        let connection = effect_fixture();
        let base = branch_head(&connection, "main").expect("base");
        let base_state = load_snapshot_state(&connection, base).expect("base state");
        branch(&connection, "topic", base);
        execute(&connection, change, r#"{"branch":"topic"}"#).expect("topic effect");
        execute(
            &connection,
            "MATCH (n:Anchor) SET n.note=2 FINISH",
            r#"{"branch":"topic"}"#,
        )
        .expect("second replay Commit");
        let old_head = branch_head(&connection, "topic").expect("old topic head");
        let mut expected = load_snapshot_state(&connection, old_head).expect("topic state");
        call(&connection, "MATCH (n:Anchor) SET n.title=1 FINISH");
        let main_head = branch_head(&connection, "main").expect("main head");
        let main_state = load_snapshot_state(&connection, main_head).expect("main state");
        for (slot, value) in main_state.properties {
            if base_state.properties.get(&slot) != Some(&value) {
                expected.properties.insert(slot, value);
            }
        }
        let options = PROTECT_EXISTING_STATE.replacen('{', r#"{"branch":"topic","#, 1);
        let rows = execute(&connection, rebase_query(), &options)
            .expect("historical replay is not a new effective restricted action")
            .0;
        assert_replayed(&rows, 2);
        let rebased = branch_head(&connection, "topic").expect("new topic head");
        assert_ne!(rebased, old_head);
        assert_eq!(branch_head(&connection, "main").expect("main"), main_head);
        assert_eq!(
            load_snapshot_state(&connection, rebased).expect("rebased state"),
            expected,
            "{capability}"
        );
        assert_effect_present(&connection, capability, "topic");
    }
}

#[test]
fn rebase_final_effect_denial_rolls_back_all_replayed_commits_layers_and_schema() {
    for (change, capability) in EFFECTS {
        let connection = effect_fixture();
        let base = branch_head(&connection, "main").expect("base");
        branch(&connection, "topic", base);
        for value in [1, 2] {
            execute(
                &connection,
                &format!("MATCH (n:Anchor) SET n.note={value} FINISH"),
                r#"{"branch":"topic"}"#,
            )
            .expect("topic replay history");
        }
        call(&connection, change);
        let main_head = branch_head(&connection, "main").expect("onto head");
        let counts = canonical_counts(&connection);
        assert_denied_unchanged(
            &connection,
            rebase_query(),
            &format!(r#"{{"branch":"topic","writeAccess":{{"{capability}":false}}}}"#),
            "topic",
        );
        assert_eq!(canonical_counts(&connection), counts, "{capability}");
        assert_eq!(branch_head(&connection, "main").expect("main"), main_head);
        let rows = execute(
            &connection,
            rebase_query(),
            r#"{"branch":"topic","writeAccess":{"denyProperties":["secret"]}}"#,
        )
        .expect("retry with the effective capability permitted")
        .0;
        assert_replayed(&rows, 2);
        assert_effect_present(&connection, capability, "topic");
        assert_eq!(
            execute(
                &connection,
                "MATCH (n:Anchor) RETURN n.note, n.secret",
                r#"{"at":"branch/topic"}"#,
            )
            .expect("replayed public state")
            .0,
            vec![vec![Value::Integer(2), Value::String("internal".into())]]
        );
    }
}

#[test]
fn squash_preserves_delete_and_schema_history_without_treating_replay_as_new_effects() {
    let connection = effect_fixture();
    let base = branch_head(&connection, "main").expect("base");
    call(&connection, "MATCH (n:Victim) DELETE n FINISH");
    call(
        &connection,
        "CREATE RANGE INDEX anchor_title FOR (n:Anchor) ON (n.title)",
    );
    let old_head = branch_head(&connection, "main").expect("old head");
    let expected = load_snapshot_state(&connection, old_head).expect("effective state");
    execute(
        &connection,
        &format!(
            "CALL lithograph.squash('{}') YIELD commit RETURN commit",
            descriptor(base)
        ),
        PROTECT_EXISTING_STATE,
    )
    .expect("history-only Squash permits old deletion and Schema changes");
    let squashed = branch_head(&connection, "main").expect("squashed head");
    assert_ne!(squashed, old_head);
    assert_eq!(
        load_snapshot_state(&connection, squashed).expect("squashed state"),
        expected
    );
}
