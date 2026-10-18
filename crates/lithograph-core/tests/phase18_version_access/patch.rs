use super::*;

pub(super) fn diff(connection: &Connection, before: HashId, after: HashId) -> Value {
    call(
        connection,
        &format!(
            "CALL lithograph.diff('{}', '{}') YIELD patch RETURN patch",
            descriptor(before),
            descriptor(after)
        ),
    )[0][0]
        .clone()
}

pub(super) fn apply(
    connection: &Connection,
    patch: Value,
    options: &str,
) -> Result<QuerySummary, QueryError> {
    execute_params(
        connection,
        "CALL lithograph.patch.apply($patch) YIELD commit RETURN commit",
        BTreeMap::from([("patch".to_owned(), patch)]),
        options,
    )
    .map(|result| result.1)
}

#[test]
fn patch_is_not_blanket_denied_and_protected_changes_rollback_before_publishing() {
    let connection = fresh_storage();
    call(
        &connection,
        "CREATE (:Item {secret:'base', title:'base'}) FINISH",
    );
    let base = branch_head(&connection, "main").expect("base");
    call(&connection, "MATCH (n:Item) SET n.title='public' FINISH");
    let public = branch_head(&connection, "main").expect("public");
    let public_patch = diff(&connection, base, public);
    call(&connection, "MATCH (n:Item) SET n.secret='private' FINISH");
    let private = branch_head(&connection, "main").expect("private");
    let private_patch = diff(&connection, public, private);
    call(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(base)
        ),
    );
    let options = r#"{"writeAccess":{"denyPropertyPrefixes":["sec"],"allowCreate":false,"allowDelete":false}}"#;
    apply(&connection, public_patch, options).expect("Patch with permitted effective changes");
    let before = branch_head(&connection, "main").expect("before protected Patch");
    let commits = commit_count(&connection);
    let error = apply(&connection, private_patch, options).expect_err("protected Patch denied");
    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    assert_eq!(
        branch_head(&connection, "main").expect("unchanged head"),
        before
    );
    assert_eq!(commit_count(&connection), commits);
    assert_eq!(
        call(&connection, "MATCH (n:Item) RETURN n.secret, n.title"),
        vec![vec![
            Value::String("base".into()),
            Value::String("public".into())
        ]]
    );
}

#[test]
fn patch_explicit_same_value_property_write_is_denied_even_without_effective_delta() {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {secret:'base'}) FINISH");
    let base = branch_head(&connection, "main").expect("base");
    call(&connection, "MATCH (n:Item) SET n.secret='private' FINISH");
    let private = branch_head(&connection, "main").expect("private");
    let mut patch = diff(&connection, base, private);
    let Value::Map(patch_map) = &mut patch else {
        panic!("Patch map");
    };
    let Value::List(operations) = patch_map.get_mut("operations").expect("operations") else {
        panic!("operations list");
    };
    let Value::Map(operation) = &mut operations[0] else {
        panic!("property operation");
    };
    operation.insert("after".to_owned(), operation["before"].clone());
    call(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(base)
        ),
    );
    let commits = commit_count(&connection);
    let error = apply(
        &connection,
        patch,
        r#"{"writeAccess":{"denyProperties":["secret"]}}"#,
    )
    .expect_err("an explicitly targeted protected property is forbidden");
    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    assert_eq!(
        branch_head(&connection, "main").expect("unchanged head"),
        base
    );
    assert_eq!(commit_count(&connection), commits);
}

#[test]
fn opaque_diff_rejects_hidden_query_properties_but_result_rules_leave_patch_complete() {
    let connection = fresh_storage();
    call(
        &connection,
        "CREATE (:Item {secret:'base', title:'base'}) FINISH",
    );
    let base = branch_head(&connection, "main").expect("base");
    call(&connection, "MATCH (n:Item) SET n.secret='private' FINISH");
    let changed = branch_head(&connection, "main").expect("changed");
    let query = format!(
        "CALL lithograph.diff('{}', '{}') YIELD patch RETURN patch",
        descriptor(base),
        descriptor(changed)
    );
    let full = execute(&connection, &query, "{}").expect("full Patch").0;
    let returned = execute(
        &connection,
        &query,
        r#"{"readAccess":{"result":{"denyProperties":["secret","operations"]}}}"#,
    )
    .expect("result filtering does not forge a partial Patch")
    .0;
    assert_eq!(returned, full);
    assert_denied_unchanged(
        &connection,
        &query,
        r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#,
        "main",
    );
    call(&connection, "MATCH (n:Item) SET n.title='public' FINISH");
    let public = branch_head(&connection, "main").expect("public");
    execute(
        &connection,
        &format!(
            "CALL lithograph.diff('{}', '{}') YIELD patch RETURN patch",
            descriptor(changed),
            descriptor(public)
        ),
        r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#,
    )
    .expect("unrelated complete Patch can be read");
}

#[test]
fn authorized_patch_entity_deletion_does_not_treat_cleanup_as_protected_property_writes() {
    let connection = fresh_storage();
    call(
        &connection,
        "CREATE (a:Item {secret:'a'})-[r:LINK {secret:'r'}]->(b:Item {secret:'b'}) FINISH",
    );
    let base = branch_head(&connection, "main").expect("base");
    call(&connection, "MATCH (n:Item) DETACH DELETE n FINISH");
    let deleted = branch_head(&connection, "main").expect("deleted");
    let patch = diff(&connection, base, deleted);
    call(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(base)
        ),
    );
    apply(
        &connection,
        patch,
        r#"{"writeAccess":{"denyProperties":["secret"],"allowDelete":true,"allowLabelChanges":false}}"#,
    )
    .expect("authorized entity deletion includes cleanup");
    assert_eq!(
        call(&connection, "MATCH (n) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
}
