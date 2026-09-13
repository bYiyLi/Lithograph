use super::*;

#[test]
fn diff_patch_round_trip_is_atomic_and_ignores_commit_data() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc {name:'a', count:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("state A");
    let a = branch_head(&connection, "main").expect("A");
    execute(
        &connection,
        "MATCH (n:Doc) SET n.name='b', n.count=2 SET n:Published FINISH",
        ExecutionOptions::default(),
    )
    .expect("state B");
    let b = branch_head(&connection, "main").expect("B");

    let patch_before_data = call(
        &connection,
        &format!(
            "CALL lithograph.diff('{}', '{}') YIELD patch RETURN patch",
            descriptor(a),
            descriptor(b)
        ),
    )[0][0]
        .clone();
    assert_patch_operations_have_before_after(&patch_before_data);
    call(
        &connection,
        &format!(
            "CALL lithograph.commit.data.set('{}', 'annotation') YIELD commit RETURN commit",
            descriptor(b)
        ),
    );
    let patch_after_data = call(
        &connection,
        &format!(
            "CALL lithograph.diff('{}', '{}') YIELD patch RETURN patch",
            descriptor(a),
            descriptor(b)
        ),
    )[0][0]
        .clone();
    assert_eq!(patch_before_data, patch_after_data);
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('diff-a', '{}') YIELD name RETURN name",
            descriptor(a)
        ),
    );
    call(
        &connection,
        &format!(
            "CALL lithograph.tag.create('diff-b', '{}') YIELD name RETURN name",
            descriptor(b)
        ),
    );
    let descriptor_patch = call(
        &connection,
        "CALL lithograph.diff('branch/diff-a', 'tag/diff-b') YIELD patch RETURN patch",
    )[0][0]
        .clone();
    assert_eq!(descriptor_patch, patch_before_data);

    call(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(a)
        ),
    );
    let mut params = BTreeMap::new();
    params.insert("patch".to_owned(), patch_before_data.clone());
    let prepared = prepare(
        &connection,
        "CALL lithograph.patch.apply($patch) YIELD commit RETURN commit",
        params,
        ExecutionOptions::default(),
    )
    .expect("prepare patch");
    let mut cursor = QueryCursor::new(prepared);
    while !cursor
        .next_batch(&connection, 64)
        .expect("patch batch")
        .done
    {}
    let applied = branch_head(&connection, "main").expect("applied head");
    assert_eq!(snapshot(&connection, applied), snapshot(&connection, b));

    assert_foreign_patch_rejected(&connection, a, &patch_before_data);
    assert_malformed_patch_rejected(&connection, a, &patch_before_data);
}

fn assert_foreign_patch_rejected(connection: &Connection, base: HashId, patch: &Value) {
    call(
        connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(base)
        ),
    );
    let before = branch_head(connection, "main").expect("before bad patch");
    let mut foreign = map(patch).clone();
    foreign.insert(
        "databaseId".to_owned(),
        Value::String("00000000-0000-4000-8000-000000000099".to_owned()),
    );
    let mut params = BTreeMap::new();
    params.insert("patch".to_owned(), Value::Map(foreign));
    let error = prepare(
        connection,
        "CALL lithograph.patch.apply($patch) YIELD commit RETURN commit",
        params,
        ExecutionOptions::default(),
    )
    .and_then(|prepared| {
        let mut cursor = QueryCursor::new(prepared);
        cursor.next_batch(connection, 64).map(|_| ())
    })
    .expect_err("foreign database patch must fail");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    assert_eq!(
        branch_head(connection, "main").expect("after bad patch"),
        before
    );
}

fn assert_malformed_patch_rejected(connection: &Connection, base: HashId, patch: &Value) {
    let mut missing_from = map(patch).clone();
    missing_from.remove("from");
    assert_patch_rejected_atomically(connection, base, Value::Map(missing_from));

    let mut mismatched_slot = map(patch).clone();
    let Value::List(operations) = mismatched_slot
        .get_mut("operations")
        .expect("patch operations")
    else {
        panic!("patch operations must be a List");
    };
    let first = operations.first_mut().expect("non-empty patch");
    let Value::Map(operation) = first else {
        panic!("patch operation must be a Map");
    };
    operation.insert("slot".to_owned(), Value::String("node/999".to_owned()));
    assert_patch_rejected_atomically(connection, base, Value::Map(mismatched_slot));

    let mut duplicate_slot = map(patch).clone();
    let Value::List(operations) = duplicate_slot
        .get_mut("operations")
        .expect("patch operations")
    else {
        panic!("patch operations must be a List");
    };
    let mut reverse = operations
        .iter()
        .find_map(|value| match value {
            Value::Map(operation)
                if operation.get("op") == Some(&Value::String("AddLabel".to_owned())) =>
            {
                Some(operation.clone())
            }
            _ => None,
        })
        .expect("round-trip Patch includes AddLabel");
    reverse.insert("op".to_owned(), Value::String("RemoveLabel".to_owned()));
    reverse.insert("before".to_owned(), Value::Boolean(true));
    operations.push(Value::Map(reverse));
    assert_patch_rejected_atomically(connection, base, Value::Map(duplicate_slot));
}

fn assert_patch_rejected_atomically(connection: &Connection, base: HashId, patch: Value) {
    call(
        connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(base)
        ),
    );
    let before = branch_head(connection, "main").expect("before malformed patch");
    let count_before = commit_count(connection);
    let mut params = BTreeMap::new();
    params.insert("patch".to_owned(), patch);
    let error = prepare(
        connection,
        "CALL lithograph.patch.apply($patch) YIELD commit RETURN commit",
        params,
        ExecutionOptions::default(),
    )
    .and_then(|prepared| {
        let mut cursor = QueryCursor::new(prepared);
        cursor.next_batch(connection, 64).map(|_| ())
    })
    .expect_err("malformed Patch must fail");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    assert_eq!(
        branch_head(connection, "main").expect("after malformed patch"),
        before
    );
    assert_eq!(commit_count(connection), count_before);
}

#[test]
fn relationship_patch_round_trip_preserves_identity_and_properties() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:A {name:'a'}), (:B {name:'b'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base nodes");
    let before = branch_head(&connection, "main").expect("before relationship");
    execute(
        &connection,
        "MATCH (a:A), (b:B) CREATE (a)-[:LINK {weight:3}]->(b) FINISH",
        ExecutionOptions::default(),
    )
    .expect("relationship state");
    let after = branch_head(&connection, "main").expect("after relationship");

    let forward = call(
        &connection,
        &format!(
            "CALL lithograph.diff('{}', '{}') YIELD patch RETURN patch",
            descriptor(before),
            descriptor(after)
        ),
    )[0][0]
        .clone();
    assert_patch_operations_have_before_after(&forward);
    call(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(before)
        ),
    );
    apply_patch(&connection, forward);
    assert_eq!(
        snapshot(
            &connection,
            branch_head(&connection, "main").expect("forward head")
        ),
        snapshot(&connection, after)
    );

    let inverse = call(
        &connection,
        &format!(
            "CALL lithograph.diff('{}', '{}') YIELD patch RETURN patch",
            descriptor(after),
            descriptor(before)
        ),
    )[0][0]
        .clone();
    apply_patch(&connection, inverse);
    assert_eq!(
        snapshot(
            &connection,
            branch_head(&connection, "main").expect("inverse head")
        ),
        snapshot(&connection, before)
    );
}

#[test]
fn same_name_index_replacement_round_trip_uses_one_logical_slot() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE RANGE INDEX stable_idx FOR (n:Person) ON (n.age)",
        ExecutionOptions::default(),
    )
    .expect("range index");
    let before = branch_head(&connection, "main").expect("before index replacement");
    execute(
        &connection,
        "DROP INDEX stable_idx",
        ExecutionOptions::default(),
    )
    .expect("drop range index");
    execute(
        &connection,
        "CREATE TEXT INDEX stable_idx FOR (n:Person) ON (n.name)",
        ExecutionOptions::default(),
    )
    .expect("text index");
    let after = branch_head(&connection, "main").expect("after index replacement");

    let forward = call(
        &connection,
        &format!(
            "CALL lithograph.diff('{}', '{}') YIELD patch RETURN patch",
            descriptor(before),
            descriptor(after)
        ),
    )[0][0]
        .clone();
    let Value::List(operations) = map(&forward).get("operations").expect("Patch operations") else {
        panic!("Patch operations must be a List");
    };
    let index_operations = operations
        .iter()
        .filter_map(|value| match value {
            Value::Map(operation)
                if operation.get("slot") == Some(&Value::String("index/stable_idx".to_owned())) =>
            {
                Some(operation)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(index_operations.len(), 1);
    assert_eq!(
        index_operations[0].get("op"),
        Some(&Value::String("SetIndex".to_owned()))
    );

    call(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(before)
        ),
    );
    apply_patch(&connection, forward);
    assert_eq!(
        snapshot(
            &connection,
            branch_head(&connection, "main").expect("forward replacement head")
        ),
        snapshot(&connection, after)
    );

    let inverse = call(
        &connection,
        &format!(
            "CALL lithograph.diff('{}', '{}') YIELD patch RETURN patch",
            descriptor(after),
            descriptor(before)
        ),
    )[0][0]
        .clone();
    apply_patch(&connection, inverse);
    assert_eq!(
        snapshot(
            &connection,
            branch_head(&connection, "main").expect("inverse replacement head")
        ),
        snapshot(&connection, before)
    );
}

fn assert_patch_operations_have_before_after(patch: &Value) {
    let Value::List(operations) = &map(patch)["operations"] else {
        panic!("patch operations must be a List");
    };
    for operation in operations {
        let operation = map(operation);
        assert!(operation.contains_key("before"));
        assert!(operation.contains_key("after"));
    }
}

fn apply_patch(connection: &Connection, patch: Value) {
    let mut params = BTreeMap::new();
    params.insert("patch".to_owned(), patch);
    let prepared = prepare(
        connection,
        "CALL lithograph.patch.apply($patch) YIELD commit RETURN commit",
        params,
        ExecutionOptions::default(),
    )
    .expect("prepare Patch");
    let mut cursor = QueryCursor::new(prepared);
    while !cursor.next_batch(connection, 64).expect("Patch batch").done {}
}
