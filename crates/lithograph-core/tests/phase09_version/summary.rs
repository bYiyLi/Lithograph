use super::*;

#[test]
fn version_summary_reports_the_mutated_target_branch_snapshot() {
    let connection = fresh_storage();
    let main = branch_head(&connection, "main").expect("main head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('feature', '{}') YIELD name RETURN name",
            descriptor(main)
        ),
    );
    let (rows, summary) = execute(
        &connection,
        "CALL lithograph.commit.create() YIELD commit RETURN commit",
        options(r#"{"branch":"feature"}"#),
    )
    .expect("explicit Commit on feature");
    let feature_commit = string(&rows[0][0]).to_owned();
    assert_eq!(summary.commit.as_deref(), Some(feature_commit.as_str()));
    assert_eq!(
        branch_head(&connection, "main").expect("main remains active"),
        main
    );
}

#[test]
fn ref_only_version_summary_reports_the_active_branch_snapshot() {
    let connection = fresh_storage();
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('feature', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "CALL lithograph.commit.create() YIELD commit RETURN commit",
        options(r#"{"branch":"feature"}"#),
    )
    .expect("advance feature");
    execute(
        &connection,
        "CREATE (:MainOnly) FINISH",
        ExecutionOptions::default(),
    )
    .expect("advance active main");
    let main = branch_head(&connection, "main").expect("active main head");
    let (_, summary) = execute(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(base)
        ),
        options(r#"{"branch":"feature"}"#),
    )
    .expect("reset non-active feature");
    assert_eq!(summary.commit.as_deref(), Some(descriptor(main).as_str()));
    assert_eq!(
        branch_head(&connection, "feature").expect("reset feature head"),
        base
    );
}

#[test]
fn fast_forward_merge_summary_reports_the_active_branch_snapshot() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base");
    let base = branch_head(&connection, "main").expect("base head");
    for name in ["feature", "source"] {
        call(
            &connection,
            &format!(
                "CALL lithograph.branch.create('{name}', '{}') YIELD name RETURN name",
                descriptor(base)
            ),
        );
    }
    execute(
        &connection,
        "MATCH (n:Item) SET n.v=1 FINISH",
        options(r#"{"branch":"source"}"#),
    )
    .expect("advance merge source");
    let source = branch_head(&connection, "source").expect("source head");
    execute(
        &connection,
        "CREATE (:MainOnly) FINISH",
        ExecutionOptions::default(),
    )
    .expect("advance active main independently");
    let main = branch_head(&connection, "main").expect("active main head");
    assert_ne!(main, source);

    let started = execute(
        &connection,
        "CALL lithograph.merge.start('branch/source') YIELD session, revision, status RETURN session, revision, status",
        options(r#"{"branch":"feature"}"#),
    )
    .expect("start fast-forward merge")
    .0;
    assert_eq!(started[0][2], Value::String("fast_forward".to_owned()));
    let session = string(&started[0][0]).to_owned();
    let revision = integer(&started[0][1]);
    let (finalized, summary) = execute(
        &connection,
        &format!(
            "CALL lithograph.merge.finalize('{session}', {revision}) YIELD status, commit RETURN status, commit"
        ),
        ExecutionOptions::default(),
    )
    .expect("finalize fast-forward merge");
    assert_eq!(finalized[0][0], Value::String("fast_forward".to_owned()));
    assert_eq!(summary.commit.as_deref(), Some(descriptor(main).as_str()));
    assert_eq!(
        branch_head(&connection, "feature").expect("fast-forwarded feature"),
        source
    );
}

#[test]
fn merge_finalize_summary_reports_session_target_snapshot() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {left:0, right:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('feature', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.left=1 FINISH",
        ExecutionOptions::default(),
    )
    .expect("main change");
    let main = branch_head(&connection, "main").expect("main head");
    execute(
        &connection,
        "MATCH (n:Item) SET n.right=2 FINISH",
        options(r#"{"branch":"feature"}"#),
    )
    .expect("feature change");
    let started = execute(
        &connection,
        "CALL lithograph.merge.start('branch/main') YIELD session, revision, status RETURN session, revision, status",
        options(r#"{"branch":"feature"}"#),
    )
    .expect("start merge into non-active feature")
    .0;
    assert_eq!(started[0][2], Value::String("ready".to_owned()));
    let session = string(&started[0][0]).to_owned();
    let revision = integer(&started[0][1]);
    let (finalized, summary) = execute(
        &connection,
        &format!(
            "CALL lithograph.merge.finalize('{session}', {revision}) YIELD status, commit RETURN status, commit"
        ),
        ExecutionOptions::default(),
    )
    .expect("finalize feature merge");
    assert_eq!(finalized[0][0], Value::String("merged".to_owned()));
    let merged = string(&finalized[0][1]).to_owned();
    assert_eq!(summary.commit.as_deref(), Some(merged.as_str()));
    assert_eq!(
        branch_head(&connection, "main").expect("active main unchanged"),
        main
    );
    assert_eq!(
        format!(
            "commit/{}",
            branch_head(&connection, "feature")
                .expect("feature merged head")
                .to_hex()
        ),
        merged
    );
}

#[test]
fn trailing_read_only_version_call_keeps_the_mutation_summary() {
    let connection = fresh_storage();
    let (_, summary) = execute(
        &connection,
        "CALL lithograph.commit.create() YIELD commit NEXT CALL lithograph.branch.list() YIELD name RETURN name",
        ExecutionOptions::default(),
    )
    .expect("mutating call followed by read-only version call");
    let head = branch_head(&connection, "main").expect("main head");
    assert_eq!(summary.commit.as_deref(), Some(descriptor(head).as_str()));
}

#[test]
fn repeated_version_mutation_uses_the_prior_invocation_head() {
    let connection = fresh_storage();
    let base = branch_head(&connection, "main").expect("base head");
    let (rows, summary) = execute(
        &connection,
        "UNWIND [1, 2] AS ordinal CALL lithograph.commit.create() YIELD commit RETURN ordinal, commit ORDER BY ordinal",
        ExecutionOptions::default(),
    )
    .expect("repeated Commit creation");
    assert_eq!(rows.len(), 2);
    let first = HashId::from_hex(&string(&rows[0][1])[7..]).expect("first Commit id");
    let second = HashId::from_hex(&string(&rows[1][1])[7..]).expect("second Commit id");
    assert_ne!(first, base);
    assert_ne!(second, first);
    assert_eq!(
        load_commit(&connection, second)
            .expect("second Commit")
            .parent1,
        Some(first)
    );
    assert_eq!(branch_head(&connection, "main").expect("main head"), second);
    assert_eq!(summary.commit.as_deref(), Some(descriptor(second).as_str()));
}

#[test]
fn composed_ref_move_then_commit_uses_the_selected_target_branch_head() {
    let connection = fresh_storage();
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('feature', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "CREATE (:MainOnly) FINISH",
        ExecutionOptions::default(),
    )
    .expect("advance active main");
    let main = branch_head(&connection, "main").expect("main head");

    let (rows, summary) = execute(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to NEXT CALL lithograph.commit.create() YIELD commit RETURN commit",
            descriptor(base)
        ),
        options(r#"{"branch":"feature"}"#),
    )
    .expect("reset then commit selected feature branch");
    let created = HashId::from_hex(&string(&rows[0][0])[7..]).expect("created Commit id");
    assert_eq!(
        load_commit(&connection, created)
            .expect("created Commit")
            .parent1,
        Some(base)
    );
    assert_eq!(
        branch_head(&connection, "feature").expect("feature head"),
        created
    );
    assert_eq!(branch_head(&connection, "main").expect("main head"), main);
    assert_eq!(
        summary.commit.as_deref(),
        Some(descriptor(created).as_str())
    );
}
