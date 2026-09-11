use super::*;

#[test]
fn write_options_target_the_selected_branch_and_persist_commit_metadata() {
    let connection = fresh_storage();
    let main = branch_head(&connection, "main").expect("main head");
    lithograph_core::storage::create_branch(&connection, "team/feature", main)
        .expect("create hierarchical branch");
    let options = ExecutionOptions::parse_text(
        r#"{"branch":"team/feature","author":"Ada","message":"Phase 05 branch write"}"#,
    )
    .expect("branch write options");

    let (_, summary) = execute(
        &connection,
        "CREATE (:BranchOnly {value:1}) FINISH",
        options,
    )
    .expect("write selected branch");
    let branch = branch_head(&connection, "team/feature").expect("feature branch head");

    assert_ne!(branch, main);
    assert_eq!(
        branch_head(&connection, "main").expect("unchanged main"),
        main
    );
    assert_eq!(summary.commit, format!("commit/{}", branch.to_hex()));
    let metadata: (Option<String>, Option<String>) = connection
        .query_row(
            "SELECT author, message FROM main._lithograph_commits WHERE id = ?1",
            [branch.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read commit metadata");
    assert_eq!(
        metadata,
        (
            Some("Ada".to_owned()),
            Some("Phase 05 branch write".to_owned())
        )
    );
    assert_eq!(
        read_rows(&connection, "MATCH (n:BranchOnly) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
    let historical = ExecutionOptions::parse_text(r#"{"at":"branch/team/feature"}"#)
        .expect("hierarchical branch snapshot");
    assert_eq!(
        execute(
            &connection,
            "MATCH (n:BranchOnly) RETURN count(n)",
            historical.clone(),
        )
        .expect("read selected branch")
        .0,
        vec![vec![Value::Integer(1)]]
    );
    let error = execute(
        &connection,
        "CREATE (:RejectedHistoricalWrite) FINISH",
        historical,
    )
    .expect_err("options.at is read-only even when it names a Branch");
    assert_eq!(error.kind, QueryErrorKind::ReadOnlySnapshot);
}
