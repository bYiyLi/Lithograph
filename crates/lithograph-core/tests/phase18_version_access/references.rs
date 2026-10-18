use super::*;

#[test]
fn reference_and_commit_metadata_operations_are_not_graph_property_writes() {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {secret:'value'}) FINISH");
    let base = branch_head(&connection, "main").expect("base");
    let options = r#"{"readAccess":{"query":{"denyProperties":["secret"]},"result":{"denyProperties":["secret"]}},"writeAccess":{"denyProperties":["secret"],"allowCreate":false,"allowDelete":false,"allowLabelChanges":false,"allowSchemaChanges":false}}"#;
    let descriptor = descriptor(base);
    for query in [
        format!("CALL lithograph.branch.create('ref', '{descriptor}') YIELD name RETURN name"),
        "CALL lithograph.branch.checkout('ref') YIELD name RETURN name".into(),
        "CALL lithograph.branch.checkout('main') YIELD name RETURN name".into(),
        "CALL lithograph.branch.delete('ref') YIELD name RETURN name".into(),
        format!("CALL lithograph.tag.create('tag', '{descriptor}') YIELD name RETURN name"),
        format!("CALL lithograph.tag.move('tag', '{descriptor}') YIELD name RETURN name"),
        "CALL lithograph.tag.delete('tag') YIELD name RETURN name".into(),
        format!(
            "CALL lithograph.commit.data.set('{descriptor}', {{secret:'metadata'}}) YIELD data RETURN data"
        ),
    ] {
        execute(&connection, &query, options).unwrap_or_else(|error| panic!("{query}: {error}"));
    }
    let data = execute(
        &connection,
        &format!("CALL lithograph.commit.get('{descriptor}') YIELD data RETURN data"),
        options,
    )
    .expect("metadata keys are not graph properties")
    .0;
    assert_eq!(
        data,
        vec![vec![Value::Map(BTreeMap::from([(
            "secret".into(),
            Value::String("metadata".into())
        )]))]]
    );
    execute(
        &connection,
        &format!("CALL lithograph.commit.data.clear('{descriptor}') YIELD commit RETURN commit"),
        options,
    )
    .expect("clear metadata");
    let before = load_snapshot_state(&connection, base).expect("before state");
    execute(
        &connection,
        "CALL lithograph.commit.create({secret:'metadata'}) YIELD commit RETURN commit",
        options,
    )
    .expect("empty version Commit is not a Node creation");
    let head = branch_head(&connection, "main").expect("new Commit");
    assert_ne!(head, base);
    assert_eq!(
        load_snapshot_state(&connection, head).expect("after state"),
        before
    );
}

#[test]
fn version_boolean_rejects_reference_metadata_and_session_writes_without_side_effects() {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {secret:0}) FINISH");
    let base = branch_head(&connection, "main").expect("base");
    branch(&connection, "source", base);
    branch(&connection, "delete-me", base);
    call(
        &connection,
        "CALL lithograph.tag.create('existing', 'branch/main') YIELD name RETURN name",
    );
    let version = descriptor(base);
    for query in [
        "CALL lithograph.branch.create('forbidden') YIELD name RETURN name".into(),
        "CALL lithograph.branch.checkout('source') YIELD name RETURN name".into(),
        "CALL lithograph.branch.delete('delete-me') YIELD name RETURN name".into(),
        "CALL lithograph.tag.create('forbidden', 'branch/main') YIELD name RETURN name".into(),
        "CALL lithograph.tag.move('existing', 'branch/source') YIELD name RETURN name".into(),
        "CALL lithograph.tag.delete('existing') YIELD name RETURN name".into(),
        "CALL lithograph.commit.create() YIELD commit RETURN commit".into(),
        format!(
            "CALL lithograph.commit.data.set('{version}', {{secret:'meta'}}) YIELD commit RETURN commit"
        ),
        format!("CALL lithograph.commit.data.clear('{version}') YIELD commit RETURN commit"),
        "CALL lithograph.merge.start('branch/source') YIELD session RETURN session".into(),
    ] {
        assert_denied_unchanged(
            &connection,
            &query,
            r#"{"writeAccess":{"allowVersionChanges":false,"allowCreate":true,"allowDelete":true,"allowMaintenance":true}}"#,
            "main",
        );
    }
    let branches = call(
        &connection,
        "CALL lithograph.branch.list() YIELD name, active RETURN name, active",
    );
    assert!(branches.contains(&vec![Value::String("main".into()), Value::Boolean(true)]));
    assert!(
        branches
            .iter()
            .all(|row| row[0] != Value::String("forbidden".into()))
    );
    assert!(
        branches
            .iter()
            .any(|row| row[0] == Value::String("delete-me".into()))
    );
    assert_eq!(
        call(
            &connection,
            "CALL lithograph.tag.list() YIELD name RETURN name"
        ),
        vec![vec![Value::String("existing".into())]]
    );
    assert_eq!(
        call(
            &connection,
            &format!(
                "CALL lithograph.commit.get('{version}') YIELD hasData, data RETURN hasData, data"
            )
        ),
        vec![vec![Value::Boolean(false), Value::Null]]
    );
    let sessions: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_merge_sessions",
            [],
            |row| row.get(0),
        )
        .expect("session count");
    assert_eq!(sessions, 0);
}

#[test]
fn maintenance_permission_is_independent_from_version_and_graph_property_permissions() {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {secret:1}) FINISH");
    call(
        &connection,
        "CREATE RANGE INDEX item_secret FOR (n:Item) ON (n.secret)",
    );
    let query =
        "CALL lithograph.index.rebuild('item_secret', 'branch/main') YIELD name RETURN name";
    for operation in [query, "CALL lithograph.gc() YIELD commits RETURN commits"] {
        assert_denied_unchanged(
            &connection,
            operation,
            r#"{"writeAccess":{"allowMaintenance":false,"allowVersionChanges":true}}"#,
            "main",
        );
        execute(
            &connection,
            operation,
            r#"{"readAccess":{"query":{"denyProperties":["secret"]}},"writeAccess":{"allowMaintenance":true,"allowVersionChanges":false,"denyProperties":["secret"]}}"#,
        )
        .expect("derived rebuild and GC do not modify graph properties or Branch refs");
    }
}
