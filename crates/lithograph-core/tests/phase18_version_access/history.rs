use super::*;

const DENY_SECRET: &str = r#"{"writeAccess":{"denyProperties":["secret"]}}"#;

#[test]
fn reset_and_revert_check_real_target_changes_and_leave_history_intact_on_denial() {
    let connection = fresh_storage();
    call(
        &connection,
        "CREATE (:Item {secret:'base', title:'base'}) FINISH",
    );
    let base = branch_head(&connection, "main").expect("base");
    call(&connection, "MATCH (n:Item) SET n.secret='later' FINISH");
    let secret_change = branch_head(&connection, "main").expect("secret change");
    for query in [
        format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(base)
        ),
        format!(
            "CALL lithograph.revert('{}') YIELD commit RETURN commit",
            descriptor(secret_change)
        ),
    ] {
        assert_denied_unchanged(&connection, &query, DENY_SECRET, "main");
    }
    call(&connection, "MATCH (n:Item) SET n.title='public' FINISH");
    let public_change = branch_head(&connection, "main").expect("public change");
    execute(
        &connection,
        &format!(
            "CALL lithograph.revert('{}') YIELD commit RETURN commit",
            descriptor(public_change)
        ),
        DENY_SECRET,
    )
    .expect("unprotected Revert is permitted");
    assert_eq!(
        call(&connection, "MATCH (n:Item) RETURN n.secret, n.title"),
        vec![vec![
            Value::String("later".into()),
            Value::String("base".into())
        ]]
    );
    execute(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(secret_change)
        ),
        DENY_SECRET,
    )
    .expect("Reset with unchanged protected fields is permitted");
}

#[test]
fn squash_allows_protected_history_replay_when_final_effective_state_is_identical() {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {secret:0}) FINISH");
    let base = branch_head(&connection, "main").expect("base");
    call(
        &connection,
        "MATCH (n:Item) SET n.secret=1 SET n:Published FINISH",
    );
    call(&connection, "CREATE (:Extra {secret:'internal'}) FINISH");
    let old_head = branch_head(&connection, "main").expect("old head");
    let old_state = load_snapshot_state(&connection, old_head).expect("old state");
    let query = format!(
        "CALL lithograph.squash('{}') YIELD commit RETURN commit",
        descriptor(base)
    );
    let options = r#"{"writeAccess":{"denyProperties":["secret"],"allowCreate":false,"allowDelete":false,"allowLabelChanges":false,"allowSchemaChanges":false}}"#;
    execute(&connection, &query, options).expect("Squash only changes history");
    let new_head = branch_head(&connection, "main").expect("new head");
    assert_ne!(new_head, old_head);
    assert_eq!(
        load_snapshot_state(&connection, new_head).expect("new state"),
        old_state
    );
    assert_denied_unchanged(
        &connection,
        &query,
        r#"{"writeAccess":{"allowVersionChanges":false}}"#,
        "main",
    );
}

#[test]
fn rebase_allows_replaying_old_protected_writes_when_final_value_does_not_change() {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {secret:0, title:0}) FINISH");
    let base = branch_head(&connection, "main").expect("base");
    branch(&connection, "topic", base);
    call(&connection, "MATCH (n:Item) SET n.title=2 FINISH");
    execute(
        &connection,
        "MATCH (n:Item) SET n.secret=1 FINISH",
        r#"{"branch":"topic"}"#,
    )
    .expect("trusted topic history");
    let old_head = branch_head(&connection, "topic").expect("old topic head");
    execute(
        &connection,
        "CALL lithograph.rebase('branch/main') YIELD status RETURN status",
        r#"{"branch":"topic","writeAccess":{"denyProperties":["secret"],"allowCreate":false,"allowDelete":false,"allowLabelChanges":false,"allowSchemaChanges":false}}"#,
    )
    .expect("protected final property is unchanged");
    assert_ne!(
        branch_head(&connection, "topic").expect("new topic head"),
        old_head
    );
    assert_eq!(
        execute(
            &connection,
            "MATCH (n:Item) RETURN n.secret, n.title",
            r#"{"at":"branch/topic"}"#,
        )
        .expect("rebased state")
        .0,
        vec![vec![Value::Integer(1), Value::Integer(2)]]
    );
}

#[test]
fn rebase_rolls_back_replayed_commits_when_final_protected_value_would_change() {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {secret:0, title:0}) FINISH");
    let base = branch_head(&connection, "main").expect("base");
    branch(&connection, "topic", base);
    branch(&connection, "fast-forward", base);
    call(&connection, "MATCH (n:Item) SET n.secret=2 FINISH");
    for title in [1, 2] {
        execute(
            &connection,
            &format!("MATCH (n:Item) SET n.title={title} FINISH"),
            r#"{"branch":"topic"}"#,
        )
        .expect("topic history");
    }
    for name in ["topic", "fast-forward"] {
        assert_denied_unchanged(
            &connection,
            "CALL lithograph.rebase('branch/main') YIELD status RETURN status",
            &format!(r#"{{"branch":"{name}","writeAccess":{{"denyProperties":["secret"]}}}}"#),
            name,
        );
    }
}

#[test]
fn reset_and_revert_intersect_create_delete_label_and_schema_permissions() {
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
        call(&connection, change);
        let changed = branch_head(&connection, "main").expect("changed");
        call(
            &connection,
            &format!(
                "CALL lithograph.reset('{}') YIELD to RETURN to",
                descriptor(base)
            ),
        );
        assert_denied_unchanged(
            &connection,
            &format!(
                "CALL lithograph.reset('{}') YIELD to RETURN to",
                descriptor(changed)
            ),
            &format!(r#"{{"writeAccess":{{"{capability}":false}}}}"#),
            "main",
        );
        call(
            &connection,
            &format!(
                "CALL lithograph.reset('{}') YIELD to RETURN to",
                descriptor(changed)
            ),
        );
        let inverse_capability = match capability {
            "allowCreate" => "allowDelete",
            "allowDelete" => "allowCreate",
            other => other,
        };
        assert_denied_unchanged(
            &connection,
            &format!(
                "CALL lithograph.revert('{}') YIELD commit RETURN commit",
                descriptor(changed)
            ),
            &format!(r#"{{"writeAccess":{{"{inverse_capability}":false}}}}"#),
            "main",
        );
    }
}

#[test]
fn rebase_cannot_return_conflict_values_of_query_hidden_properties() {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {secret:0}) FINISH");
    let base = branch_head(&connection, "main").expect("base");
    branch(&connection, "topic", base);
    call(&connection, "MATCH (n:Item) SET n.secret=1 FINISH");
    execute(
        &connection,
        "MATCH (n:Item) SET n.secret=2 FINISH",
        r#"{"branch":"topic"}"#,
    )
    .expect("topic conflict");
    assert_denied_unchanged(
        &connection,
        "CALL lithograph.rebase('branch/main') YIELD status, conflicts RETURN status, conflicts",
        r#"{"branch":"topic","readAccess":{"query":{"denyProperties":["secret"]}}}"#,
        "topic",
    );
    let full = execute(
        &connection,
        "CALL lithograph.rebase('branch/main') YIELD status, conflicts RETURN status, conflicts",
        r#"{"branch":"topic","readAccess":{"result":{"denyProperties":["secret"]}}}"#,
    )
    .expect("result-only rules keep opaque conflict payload complete")
    .0;
    assert_eq!(full[0][0], Value::String("conflicted".into()));
    assert!(matches!(&full[0][1], Value::List(conflicts) if !conflicts.is_empty()));
}

#[test]
fn reset_compares_protected_nan_by_canonical_value_instead_of_float_equality() {
    let connection = fresh_storage();
    execute_params(
        &connection,
        "CREATE (:Item {secret:$secret, title:'base'}) FINISH",
        BTreeMap::from([("secret".into(), Value::Float(f64::NAN))]),
        "{}",
    )
    .expect("canonical NaN property");
    let base = branch_head(&connection, "main").expect("NaN base");
    for secret in [1.0, f64::NAN] {
        execute_params(
            &connection,
            "MATCH (n:Item) SET n.secret=$secret FINISH",
            BTreeMap::from([("secret".into(), Value::Float(secret))]),
            "{}",
        )
        .expect("protected slot is touched before returning to canonical NaN");
    }
    call(&connection, "MATCH (n:Item) SET n.title='public' FINISH");
    execute(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(base)
        ),
        DENY_SECRET,
    )
    .expect("an unchanged canonical NaN is not a protected property change");
    assert_eq!(branch_head(&connection, "main").expect("reset head"), base);
    let rows = call(&connection, "MATCH (n:Item) RETURN n.secret, n.title");
    assert!(matches!(rows[0][0], Value::Float(value) if value.is_nan()));
    assert_eq!(rows[0][1], Value::String("base".into()));
}

#[test]
fn reset_rejects_protected_signed_zero_change_despite_numeric_equality() {
    let connection = fresh_storage();
    execute_params(
        &connection,
        "CREATE (:Item {secret:$secret}) FINISH",
        BTreeMap::from([("secret".into(), Value::Float(0.0))]),
        "{}",
    )
    .expect("positive zero base");
    let base = branch_head(&connection, "main").expect("positive zero head");
    execute_params(
        &connection,
        "MATCH (n:Item) SET n.secret=$secret FINISH",
        BTreeMap::from([("secret".into(), Value::Float(-0.0))]),
        "{}",
    )
    .expect("negative zero is a distinct canonical property state");
    let negative = branch_head(&connection, "main").expect("negative zero head");
    assert_ne!(base, negative);
    assert_denied_unchanged(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(base)
        ),
        DENY_SECRET,
        "main",
    );
    let rows = call(&connection, "MATCH (n:Item) RETURN n.secret");
    assert!(matches!(rows[0][0], Value::Float(value) if value.to_bits() == (-0.0_f64).to_bits()));
}
