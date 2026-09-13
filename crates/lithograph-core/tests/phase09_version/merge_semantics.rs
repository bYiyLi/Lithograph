use super::*;

fn start_merge(
    connection: &Connection,
    source: &str,
    execution_options: ExecutionOptions,
) -> Vec<Vec<Value>> {
    execute(
        connection,
        &format!(
            "CALL lithograph.merge.start('{source}') YIELD session, revision, status, unresolved RETURN session, revision, status, unresolved"
        ),
        execution_options,
    )
    .expect("merge start")
    .0
}

fn single_conflict(connection: &Connection, session: &str) -> Vec<Value> {
    let conflicts = call(
        connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 20) YIELD conflictId, slot, base, ours, theirs RETURN conflictId, slot, base, ours, theirs"
        ),
    );
    assert_eq!(conflicts.len(), 1);
    conflicts.into_iter().next().expect("one conflict")
}

fn resolve_choice(
    connection: &Connection,
    session: &str,
    revision: i64,
    conflict: &str,
    choice: &str,
) -> i64 {
    let rows = call(
        connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', {revision}, [{{conflictId:'{conflict}', choice:'{choice}'}}]) YIELD revision RETURN revision"
        ),
    );
    integer(&rows[0][0])
}

fn finalize_session(connection: &Connection, session: &str, revision: i64) -> HashId {
    let rows = call(
        connection,
        &format!(
            "CALL lithograph.merge.finalize('{session}', {revision}) YIELD commit RETURN commit"
        ),
    );
    let commit = string(&rows[0][0]);
    HashId::from_hex(&commit[7..]).expect("finalized commit")
}

#[test]
fn same_value_changes_auto_merge_without_conflict() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {name:'base'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('same-value', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.name='same' FINISH",
        ExecutionOptions::default(),
    )
    .expect("ours same value");
    execute(
        &connection,
        "MATCH (n:Item) SET n.name='same' FINISH",
        options(r#"{"branch":"same-value"}"#),
    )
    .expect("theirs same value");

    let started = start_merge(
        &connection,
        "branch/same-value",
        ExecutionOptions::default(),
    );
    assert_eq!(started[0][2], Value::String("ready".to_owned()));
    assert_eq!(started[0][3], Value::Integer(0));
    let session = string(&started[0][0]);
    let revision = integer(&started[0][1]);
    let candidate = execute(
        &connection,
        "MATCH (n:Item) RETURN n.name",
        options(&format!(
            r#"{{"mergeSession":{{"id":"{session}","revision":{revision}}}}}"#
        )),
    )
    .expect("same-value candidate")
    .0;
    assert_eq!(candidate, vec![vec![Value::String("same".to_owned())]]);
}

#[test]
fn criss_cross_virtual_base_keeps_ambiguous_slot_conflicted() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {name:'root'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("root value");
    let root = branch_head(&connection, "main").expect("root head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('criss', '{}') YIELD name RETURN name",
            descriptor(root)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.name='A' FINISH",
        ExecutionOptions::default(),
    )
    .expect("A1");
    let a1 = branch_head(&connection, "main").expect("A1 head");
    execute(
        &connection,
        "MATCH (n:Item) SET n.name='B' FINISH",
        options(r#"{"branch":"criss"}"#),
    )
    .expect("B1");
    let b1 = branch_head(&connection, "criss").expect("B1 head");

    let first = start_merge(&connection, &descriptor(b1), ExecutionOptions::default());
    let first_session = string(&first[0][0]).to_owned();
    let first_conflict = single_conflict(&connection, &first_session);
    let first_revision = resolve_choice(
        &connection,
        &first_session,
        integer(&first[0][1]),
        string(&first_conflict[0]),
        "ours",
    );
    finalize_session(&connection, &first_session, first_revision);

    let second = start_merge(
        &connection,
        &descriptor(a1),
        options(r#"{"branch":"criss"}"#),
    );
    let second_session = string(&second[0][0]).to_owned();
    let second_conflict = single_conflict(&connection, &second_session);
    let second_revision = resolve_choice(
        &connection,
        &second_session,
        integer(&second[0][1]),
        string(&second_conflict[0]),
        "ours",
    );
    finalize_session(&connection, &second_session, second_revision);

    let final_merge = start_merge(&connection, "branch/criss", ExecutionOptions::default());
    assert_eq!(final_merge[0][2], Value::String("conflicted".to_owned()));
    assert_eq!(final_merge[0][3], Value::Integer(1));
    let final_session = string(&final_merge[0][0]);
    let conflict = single_conflict(&connection, final_session);
    assert!(string(&conflict[1]).ends_with("/property/name"));
    assert_eq!(conflict[2], Value::Null);
    assert_eq!(conflict[3], Value::String("A".to_owned()));
    assert_eq!(conflict[4], Value::String("B".to_owned()));
}

#[test]
fn delete_vs_modify_is_a_resolvable_conflict() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {name:'base'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base item");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('modify-source', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) DETACH DELETE n FINISH",
        ExecutionOptions::default(),
    )
    .expect("ours delete");
    execute(
        &connection,
        "MATCH (n:Item) SET n.name='theirs' FINISH",
        options(r#"{"branch":"modify-source"}"#),
    )
    .expect("theirs modify");

    let started = start_merge(
        &connection,
        "branch/modify-source",
        ExecutionOptions::default(),
    );
    assert_eq!(started[0][2], Value::String("conflicted".to_owned()));
    let session = string(&started[0][0]).to_owned();
    let conflicts = call(
        &connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 20) YIELD conflictId, slot RETURN conflictId, slot"
        ),
    );
    assert_eq!(conflicts.len(), 1);
    assert_eq!(string(&conflicts[0][1]), "node/1");
    let resolutions = conflicts
        .iter()
        .map(|row| format!("{{conflictId:'{}', choice:'theirs'}}", string(&row[0])))
        .collect::<Vec<_>>()
        .join(",");
    let resolved = call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', {}, [{resolutions}]) YIELD revision, unresolved RETURN revision, unresolved",
            integer(&started[0][1])
        ),
    );
    assert_eq!(resolved[0][1], Value::Integer(0));
    let revision = integer(&resolved[0][0]);
    let candidate = execute(
        &connection,
        "MATCH (n:Item) RETURN n.name",
        options(&format!(
            r#"{{"mergeSession":{{"id":"{session}","revision":{revision}}}}}"#
        )),
    )
    .expect("delete-vs-modify candidate")
    .0;
    assert_eq!(candidate, vec![vec![Value::String("theirs".to_owned())]]);
}

fn dormant_resolution_fixture() -> (Connection, String, String, String, i64) {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE CONSTRAINT user_email_unique FOR (n:User) REQUIRE n.email IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("unique constraint");
    execute(
        &connection,
        "CREATE (:User {email:'base', role:'anchor'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base user");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('dormant-source', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:User) WHERE n.role='anchor' SET n.email='dup' FINISH",
        ExecutionOptions::default(),
    )
    .expect("ours direct-conflict value");
    execute(
        &connection,
        "MATCH (n:User) WHERE n.role='anchor' SET n.email='safe' CREATE (:User {email:'dup', role:'second'}) FINISH",
        options(r#"{"branch":"dormant-source"}"#),
    )
    .expect("theirs direct + disjoint value");

    let started = start_merge(
        &connection,
        "branch/dormant-source",
        ExecutionOptions::default(),
    );
    let session = string(&started[0][0]).to_owned();
    let direct = single_conflict(&connection, &session);
    assert!(string(&direct[1]).ends_with("/property/email"));
    let direct_id = string(&direct[0]).to_owned();
    let revision_two = resolve_choice(
        &connection,
        &session,
        integer(&started[0][1]),
        &direct_id,
        "ours",
    );

    let current = call(
        &connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 20) YIELD conflictId, slot, resolution RETURN conflictId, slot, resolution"
        ),
    );
    let derived = current
        .iter()
        .find(|row| string(&row[1]) == "constraint/user_email_unique")
        .expect("derived unique conflict");
    let derived_id = string(&derived[0]).to_owned();
    let resolved_derived = call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', {revision_two}, [{{conflictId:'{derived_id}', choice:'value', value:null}}]) YIELD revision, unresolved RETURN revision, unresolved"
        ),
    );
    let revision_three = integer(&resolved_derived[0][0]);
    assert_eq!(resolved_derived[0][1], Value::Integer(0));
    (connection, session, direct_id, derived_id, revision_three)
}

#[test]
fn dormant_resolution_reappears_for_the_same_conflict_id() {
    let (connection, session, direct_id, derived_id, revision_three) = dormant_resolution_fixture();
    let switched = call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', {revision_three}, [{{conflictId:'{direct_id}', choice:'theirs'}}]) YIELD revision, unresolved RETURN revision, unresolved"
        ),
    );
    let revision_four = integer(&switched[0][0]);
    assert_eq!(switched[0][1], Value::Integer(0));
    let dormant_inventory = call(
        &connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 20) YIELD conflictId RETURN conflictId"
        ),
    );
    assert!(
        !dormant_inventory
            .iter()
            .any(|row| string(&row[0]) == derived_id)
    );

    let restored = call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', {revision_four}, [{{conflictId:'{direct_id}', choice:'ours'}}]) YIELD revision, status, unresolved RETURN revision, status, unresolved"
        ),
    );
    let revision_five = integer(&restored[0][0]);
    assert_eq!(restored[0][1], Value::String("ready".to_owned()));
    assert_eq!(restored[0][2], Value::Integer(0));
    let restored_inventory = call(
        &connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 20) YIELD conflictId, resolution RETURN conflictId, resolution"
        ),
    );
    let restored_derived = restored_inventory
        .iter()
        .find(|row| string(&row[0]) == derived_id)
        .expect("dormant conflict restored");
    assert_ne!(restored_derived[1], Value::Null);
    let constraints = execute(
        &connection,
        "SHOW CONSTRAINTS YIELD name RETURN name",
        options(&format!(
            r#"{{"mergeSession":{{"id":"{session}","revision":{revision_five}}}}}"#
        )),
    )
    .expect("restored dormant candidate")
    .0;
    assert!(!constraints.contains(&vec![Value::String("user_email_unique".to_owned())]));
}

#[test]
fn rebase_flattens_old_merge_commit_against_first_parent() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {a:0, b:0, c:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base item");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('side', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.a=1 FINISH",
        ExecutionOptions::default(),
    )
    .expect("main first-parent change");
    execute(
        &connection,
        "MATCH (n:Item) SET n.b=2 FINISH",
        options(r#"{"branch":"side"}"#),
    )
    .expect("side change");
    let merge = start_merge(&connection, "branch/side", ExecutionOptions::default());
    assert_eq!(merge[0][2], Value::String("ready".to_owned()));
    let old_merge = finalize_session(&connection, string(&merge[0][0]), integer(&merge[0][1]));
    assert!(
        load_commit(&connection, old_merge)
            .expect("old merge")
            .parent2
            .is_some()
    );

    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('onto', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.c=3 FINISH",
        options(r#"{"branch":"onto"}"#),
    )
    .expect("onto change");
    let rebased = call(
        &connection,
        "CALL lithograph.rebase('branch/onto') YIELD status, commit, rewritten RETURN status, commit, rewritten",
    );
    assert_eq!(rebased[0][0], Value::String("rebased".to_owned()));
    let new_head = branch_head(&connection, "main").expect("rebased head");
    assert_ne!(new_head, old_merge);
    assert!(
        load_commit(&connection, new_head)
            .expect("rebased final")
            .parent2
            .is_none()
    );
    let values = call(&connection, "MATCH (n:Item) RETURN n.a, n.b, n.c");
    assert_eq!(
        values,
        vec![vec![
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(3)
        ]]
    );
    let rewritten = match &rebased[0][2] {
        Value::List(values) => values,
        other => panic!("expected rewritten mapping, got {other:?}"),
    };
    assert_eq!(rewritten.len(), 2);
}

fn rebase_on_branch(
    connection: &Connection,
    branch: &str,
    onto: &str,
) -> Result<Vec<Vec<Value>>, QueryError> {
    execute(
        connection,
        &format!(
            "CALL lithograph.rebase('{onto}') YIELD status, commit, conflicts RETURN status, commit, conflicts"
        ),
        options(&format!(r#"{{"branch":"{branch}"}}"#)),
    )
    .map(|result| result.0)
}

fn conflict_slots(value: &Value) -> Vec<String> {
    let Value::List(conflicts) = value else {
        panic!("expected conflict list, got {value:?}");
    };
    conflicts
        .iter()
        .map(|conflict| string(&map(conflict)["slot"]).to_owned())
        .collect()
}

#[test]
fn rebase_surfaces_relationship_endpoint_dependency_as_conflict() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:A {name:'a'}), (:B {name:'b'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base nodes");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('rebase-endpoint', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (a:A) DETACH DELETE a FINISH",
        ExecutionOptions::default(),
    )
    .expect("onto deletes endpoint");
    execute(
        &connection,
        "MATCH (a:A), (b:B) CREATE (a)-[:LINK]->(b) FINISH",
        options(r#"{"branch":"rebase-endpoint"}"#),
    )
    .expect("source adds dependent relationship");
    let old_head = branch_head(&connection, "rebase-endpoint").expect("source head");

    let rows = rebase_on_branch(&connection, "rebase-endpoint", "branch/main")
        .expect("dependency conflict must be returned, not raised as integrity failure");
    assert_eq!(rows[0][0], Value::String("conflicted".to_owned()));
    assert_eq!(rows[0][1], Value::Null);
    assert!(conflict_slots(&rows[0][2]).contains(&"node/1".to_owned()));
    let Value::List(conflicts) = &rows[0][2] else {
        panic!("expected dependency conflict list");
    };
    let node_conflict = conflicts
        .iter()
        .find(|conflict| string(&map(conflict)["slot"]) == "node/1")
        .expect("node existence conflict");
    let node_conflict_id = string(&map(node_conflict)["conflictId"]);
    let invalid = execute(
        &connection,
        &format!(
            "CALL lithograph.rebase('branch/main', {{resolutions:[{{conflictId:'{node_conflict_id}', choice:'value', value:'invalid'}}]}}) YIELD status RETURN status"
        ),
        options(r#"{"branch":"rebase-endpoint"}"#),
    )
    .expect_err("Node existence resolution must preserve slot type");
    assert_eq!(invalid.kind, QueryErrorKind::InvalidArgument);
    assert_eq!(
        branch_head(&connection, "rebase-endpoint").expect("branch unchanged"),
        old_head
    );
}

#[test]
fn rebase_surfaces_derived_unique_violation_as_conflict() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:User {email:'a'}), (:User {email:'b'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base users");
    execute(
        &connection,
        "CREATE CONSTRAINT rebase_email_unique FOR (n:User) REQUIRE n.email IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("unique constraint");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('rebase-unique', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:User) WHERE n.email='a' SET n.email='same' FINISH",
        ExecutionOptions::default(),
    )
    .expect("onto changes first user");
    execute(
        &connection,
        "MATCH (n:User) WHERE n.email='b' SET n.email='same' FINISH",
        options(r#"{"branch":"rebase-unique"}"#),
    )
    .expect("source changes second user");
    let old_head = branch_head(&connection, "rebase-unique").expect("source head");

    let rows = rebase_on_branch(&connection, "rebase-unique", "branch/main")
        .expect("derived constraint conflict must be returned as rebase conflict");
    assert_eq!(rows[0][0], Value::String("conflicted".to_owned()));
    assert_eq!(rows[0][1], Value::Null);
    assert!(conflict_slots(&rows[0][2]).contains(&"constraint/rebase_email_unique".to_owned()));
    assert_eq!(
        branch_head(&connection, "rebase-unique").expect("branch unchanged"),
        old_head
    );
}

fn simple_rebase_conflict_fixture(branch: &str) -> (Connection, String, HashId) {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base item");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('{branch}', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.v=2 FINISH",
        ExecutionOptions::default(),
    )
    .expect("onto change");
    execute(
        &connection,
        "MATCH (n:Item) SET n.v=1 FINISH",
        options(&format!(r#"{{"branch":"{branch}"}}"#)),
    )
    .expect("source change");
    let old_head = branch_head(&connection, branch).expect("source head");
    let rows = rebase_on_branch(&connection, branch, "branch/main").expect("initial conflict");
    let Value::List(conflicts) = &rows[0][2] else {
        panic!("expected rebase conflict list");
    };
    let id = string(&map(&conflicts[0])["conflictId"]).to_owned();
    (connection, id, old_head)
}

#[test]
fn rebase_rejects_unknown_and_malformed_resolutions() {
    let (connection, conflict_id, old_head) = simple_rebase_conflict_fixture("resolution-shape");
    let unknown = "0000000000000000000000000000000000000000000000000000000000000000";
    let error = execute(
        &connection,
        &format!(
            "CALL lithograph.rebase('branch/main', {{resolutions:[{{conflictId:'{unknown}', choice:'ours'}}]}}) YIELD status RETURN status"
        ),
        options(r#"{"branch":"resolution-shape"}"#),
    )
    .expect_err("unknown rebase conflictId");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);

    let error = execute(
        &connection,
        &format!(
            "CALL lithograph.rebase('branch/main', {{resolutions:[{{conflictId:'{conflict_id}', choice:'ours', value:1}}]}}) YIELD status RETURN status"
        ),
        options(r#"{"branch":"resolution-shape"}"#),
    )
    .expect_err("ours/theirs resolution cannot carry value");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    assert_eq!(
        branch_head(&connection, "resolution-shape").expect("branch unchanged"),
        old_head
    );
}

#[test]
fn rebase_fast_forward_rejects_unknown_resolution_before_moving_branch() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base item");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('fast-forward-resolution', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.v=1 FINISH",
        ExecutionOptions::default(),
    )
    .expect("advance main");
    let main_head = branch_head(&connection, "main").expect("advanced main");
    let unknown = "0000000000000000000000000000000000000000000000000000000000000000";

    let error = execute(
        &connection,
        &format!(
            "CALL lithograph.rebase('branch/main', {{resolutions:[{{conflictId:'{unknown}', choice:'ours'}}]}}) YIELD status RETURN status"
        ),
        options(r#"{"branch":"fast-forward-resolution"}"#),
    )
    .expect_err("fast-forward rebase must reject unknown resolution");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    assert_eq!(
        branch_head(&connection, "fast-forward-resolution").expect("branch unchanged"),
        base
    );
    assert_eq!(
        branch_head(&connection, "main").expect("main unchanged"),
        main_head
    );
}

#[test]
fn rebase_unknown_resolution_rolls_back_completed_replay() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {a:0, b:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base item");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('unknown-after-replay', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.a=1 FINISH",
        ExecutionOptions::default(),
    )
    .expect("onto change");
    execute(
        &connection,
        "MATCH (n:Item) SET n.b=2 FINISH",
        options(r#"{"branch":"unknown-after-replay"}"#),
    )
    .expect("source change");
    let old_head = branch_head(&connection, "unknown-after-replay").expect("old head");
    let commit_count_before = commit_count(&connection);
    let unknown = "0000000000000000000000000000000000000000000000000000000000000000";

    let error = execute(
        &connection,
        &format!(
            "CALL lithograph.rebase('branch/main', {{resolutions:[{{conflictId:'{unknown}', choice:'ours'}}]}}) YIELD status RETURN status"
        ),
        options(r#"{"branch":"unknown-after-replay"}"#),
    )
    .expect_err("unknown resolution must rollback completed replay");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    assert_eq!(
        branch_head(&connection, "unknown-after-replay").expect("branch rolled back"),
        old_head
    );
    assert_eq!(commit_count(&connection), commit_count_before);
}

#[test]
fn rebase_conflict_ids_stay_stable_across_multi_commit_resolution_retries() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {a:0, b:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base item");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('multi-rebase-conflict', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.a=2, n.b=2 FINISH",
        ExecutionOptions::default(),
    )
    .expect("onto changes both slots");
    execute(
        &connection,
        "MATCH (n:Item) SET n.a=1 FINISH",
        options(r#"{"branch":"multi-rebase-conflict"}"#),
    )
    .expect("source commit one");
    execute(
        &connection,
        "MATCH (n:Item) SET n.b=1 FINISH",
        options(r#"{"branch":"multi-rebase-conflict"}"#),
    )
    .expect("source commit two");
    let old_head = branch_head(&connection, "multi-rebase-conflict").expect("source head");

    let first = rebase_on_branch(&connection, "multi-rebase-conflict", "branch/main")
        .expect("first conflict");
    let Value::List(first_conflicts) = &first[0][2] else {
        panic!("expected first rebase conflict");
    };
    let first_id = string(&map(&first_conflicts[0])["conflictId"]).to_owned();

    let second = execute(
        &connection,
        &format!(
            "CALL lithograph.rebase('branch/main', {{resolutions:[{{conflictId:'{first_id}', choice:'ours'}}]}}) YIELD status, conflicts RETURN status, conflicts"
        ),
        options(r#"{"branch":"multi-rebase-conflict"}"#),
    )
    .expect("resolution of first conflict reaches second conflict")
    .0;
    assert_eq!(second[0][0], Value::String("conflicted".to_owned()));
    let Value::List(second_conflicts) = &second[0][1] else {
        panic!("expected second rebase conflict");
    };
    let second_id = string(&map(&second_conflicts[0])["conflictId"]).to_owned();

    let resolved = execute(
        &connection,
        &format!(
            "CALL lithograph.rebase('branch/main', {{resolutions:[{{conflictId:'{first_id}', choice:'ours'}}, {{conflictId:'{second_id}', choice:'ours'}}]}}) YIELD status, commit RETURN status, commit"
        ),
        options(r#"{"branch":"multi-rebase-conflict"}"#),
    )
    .expect("known conflict resolutions must remain valid across retry")
    .0;
    assert_eq!(resolved[0][0], Value::String("rebased".to_owned()));
    assert_ne!(resolved[0][1], Value::Null);
    assert_ne!(
        branch_head(&connection, "multi-rebase-conflict").expect("rebased head"),
        old_head
    );
    let values = execute(
        &connection,
        "MATCH (n:Item) RETURN n.a, n.b",
        options(r#"{"branch":"multi-rebase-conflict"}"#),
    )
    .expect("rebased values")
    .0;
    assert_eq!(values, vec![vec![Value::Integer(2), Value::Integer(2)]]);
}

fn criss_cross_rebase_fixture() -> (Connection, HashId, HashId, HashId, HashId) {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {a:0, b:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("root");
    let root = branch_head(&connection, "main").expect("root head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('criss-rebase', '{}') YIELD name RETURN name",
            descriptor(root)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.a=1 FINISH",
        ExecutionOptions::default(),
    )
    .expect("main side A1");
    let a1 = branch_head(&connection, "main").expect("A1");
    execute(
        &connection,
        "MATCH (n:Item) SET n.b=1 FINISH",
        options(r#"{"branch":"criss-rebase"}"#),
    )
    .expect("side B1");
    let b1 = branch_head(&connection, "criss-rebase").expect("B1");
    let main_merge = start_merge(&connection, &descriptor(b1), ExecutionOptions::default());
    let a2 = finalize_session(
        &connection,
        string(&main_merge[0][0]),
        integer(&main_merge[0][1]),
    );
    let side_merge = start_merge(
        &connection,
        &descriptor(a1),
        options(r#"{"branch":"criss-rebase"}"#),
    );
    let b2 = finalize_session(
        &connection,
        string(&side_merge[0][0]),
        integer(&side_merge[0][1]),
    );
    (connection, a1, b1, a2, b2)
}

#[test]
fn rebase_criss_cross_selects_a_first_parent_replay_boundary() {
    let (connection, a1, b1, a2, b2) = criss_cross_rebase_fixture();
    let (branch, onto, old_head) = if a1 < b1 {
        ("criss-rebase", "branch/main", b2)
    } else {
        ("main", "branch/criss-rebase", a2)
    };
    let onto_head = if branch == "main" { b2 } else { a2 };
    let rows = rebase_on_branch(&connection, branch, onto)
        .expect("criss-cross rebase must not depend on Commit-ID ordering");
    assert_eq!(rows[0][0], Value::String("rebased".to_owned()));
    let new_head = branch_head(&connection, branch).expect("rebased head");
    assert_ne!(new_head, old_head);
    let record = load_commit(&connection, new_head).expect("rebased record");
    assert_eq!(record.parent1, Some(onto_head));
    assert!(record.parent2.is_none());
    assert_eq!(
        snapshot(&connection, new_head),
        snapshot(&connection, old_head)
    );
}
