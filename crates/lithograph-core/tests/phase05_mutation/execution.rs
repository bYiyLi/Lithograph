use super::*;

#[test]
fn explain_of_a_mutating_query_is_read_only_and_completable() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before EXPLAIN");
    let prepared = prepare(
        &connection,
        "EXPLAIN CREATE (:Explained) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare mutating EXPLAIN");
    let mut cursor = QueryCursor::new(prepared);

    let batch = cursor
        .next_batch(&connection, 1)
        .expect("execute mutating EXPLAIN");
    assert!(batch.done);
    let summary = cursor.complete(&connection).expect("complete EXPLAIN");

    assert_eq!(summary.query_type, QueryType::Read);
    assert_eq!(
        branch_head(&connection, "main").expect("head after EXPLAIN"),
        before
    );
    assert!(
        lithograph_core::storage::find_label(&connection, "Explained")
            .expect("find explained label")
            .is_none()
    );
}

#[test]
fn explain_does_not_hide_unsupported_match_semantics_in_a_write_plan() {
    let connection = fresh_storage();
    let error = prepare(
        &connection,
        "EXPLAIN MATCH DIFFERENT RELATIONSHIPS (a)-->(b) CREATE (:NeverWritten) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect_err("EXPLAIN must validate the MATCH operators in a mutating plan");

    assert_eq!(error.kind, QueryErrorKind::Semantic);
    assert!(
        lithograph_core::storage::find_label(&connection, "NeverWritten")
            .expect("find label from rejected EXPLAIN")
            .is_none()
    );
}

#[test]
fn mutating_explain_reports_read_projection_and_barrier_operators() {
    let connection = fresh_storage();
    let prepared = prepare(
        &connection,
        "EXPLAIN MATCH (source) WHERE source.name = 'A' CREATE (:Copy) RETURN DISTINCT source.name AS name ORDER BY name SKIP 1 LIMIT 2",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare complete mutating plan");
    let plan = prepared.physical.explain();

    for operator in [
        "NodeScan",
        "Filter",
        "Mutation { kind: Create }",
        "Eager",
        "Distinct",
        "Project",
        "ExternalSort",
        "Skip",
        "Limit",
        "Commit",
    ] {
        assert!(plan.contains(operator), "missing {operator} in:\n{plan}");
    }
}

#[test]
fn mutating_explain_rejects_the_same_unsupported_grouping_as_execution() {
    let connection = fresh_storage();
    let error = prepare(
        &connection,
        "EXPLAIN CREATE (n) RETURN count(n) AS total, n",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect_err("EXPLAIN must validate Phase 05 projection execution scope");

    assert_eq!(error.kind, QueryErrorKind::Semantic);
}

#[test]
fn mutating_explain_preserves_optional_match_semantics() {
    let connection = fresh_storage();
    let prepared = prepare(
        &connection,
        "EXPLAIN OPTIONAL MATCH (source:Missing) CREATE (:Copy) RETURN source",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare mutating OPTIONAL MATCH plan");
    let plan = prepared.physical.explain();

    assert!(plan.contains("Optional"), "missing Optional in:\n{plan}");
    assert!(
        plan.contains("NodeScan"),
        "missing OPTIONAL MATCH scan in:\n{plan}"
    );
}

#[test]
fn mutating_explain_does_not_share_anonymous_pattern_bindings() {
    let connection = fresh_storage();
    let prepared = prepare(
        &connection,
        "EXPLAIN MATCH (), () CREATE (:Copy) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare mutating plan with anonymous patterns");
    let plan = prepared.physical.explain();

    assert_eq!(
        plan.matches("NodeScan").count(),
        2,
        "each anonymous pattern needs its own scan:\n{plan}"
    );
    assert!(plan.contains("Cartesian"), "missing Cartesian in:\n{plan}");
}

#[test]
fn write_cursor_cannot_complete_before_its_terminal_batch() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Source), (:Source) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed source nodes");
    let before = branch_head(&connection, "main").expect("head before write");
    let prepared = prepare(
        &connection,
        "MATCH (:Source) CREATE (:Pending) RETURN 1 AS value",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare multi-batch write");
    let mut cursor = QueryCursor::new(prepared);

    let batch = cursor
        .next_batch(&connection, 1)
        .expect("read the first write batch");
    assert!(!batch.done);
    let error = cursor
        .complete(&connection)
        .expect_err("premature completion must not release the write savepoint");
    assert_eq!(error.kind, QueryErrorKind::Internal);

    cursor.cancel(&connection).expect("cancel pending write");
    assert_eq!(
        branch_head(&connection, "main").expect("head after cancel"),
        before
    );
    assert_eq!(
        read_rows(&connection, "MATCH (n:Pending) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn multi_row_write_and_read_after_write_use_clause_barriers_and_one_commit() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Seed {name:'A'}), (:Seed {name:'B'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed rows");
    let commit_count_before: i64 = connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count before");
    let (_, summary) = execute(
        &connection,
        "MATCH (n:Seed) CREATE (:Made {source:n.name}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("multi-row write");
    let commit_count_after: i64 = connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count after");
    assert_eq!(commit_count_after, commit_count_before + 1);
    assert_eq!(summary.counters.nodes_created, 2);
    assert_eq!(
        read_rows(&connection, "MATCH (n:Made) RETURN count(n)"),
        vec![vec![Value::Integer(2)]]
    );

    let (rows, _) = execute(
        &connection,
        "CREATE (n:VisibleNow) MATCH (m:VisibleNow) RETURN count(m)",
        ExecutionOptions::default(),
    )
    .expect("read after write");
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn read_before_write_is_materialized_and_does_not_chase_new_rows() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Seed), (:Seed), (:Seed) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed");
    let (_, summary) = execute(
        &connection,
        "MATCH (n:Seed) CREATE (:Seed) FINISH",
        ExecutionOptions::default(),
    )
    .expect("write after materialized read");
    assert_eq!(summary.counters.nodes_created, 3);
    assert_eq!(
        read_rows(&connection, "MATCH (n:Seed) RETURN count(n)"),
        vec![vec![Value::Integer(6)]]
    );
}

#[test]
fn stale_prepared_writer_reports_branch_head_moved_and_rolls_back_allocations() {
    let connection = fresh_storage();
    let first = prepare(
        &connection,
        "CREATE (:FirstWriter) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare first");
    let second = prepare(
        &connection,
        "CREATE (:SecondWriter) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare second");

    let mut first = QueryCursor::new(first);
    assert!(first.next_batch(&connection, 1).expect("first batch").done);
    first.complete(&connection).expect("first complete");
    let head = branch_head(&connection, "main").expect("first head");

    let mut second = QueryCursor::new(second);
    let error = second
        .next_batch(&connection, 1)
        .expect_err("stale writer must fail CAS");
    assert_eq!(error.kind, QueryErrorKind::BranchHeadMoved);
    assert_eq!(
        branch_head(&connection, "main").expect("head unchanged"),
        head
    );
    assert!(
        lithograph_core::storage::find_label(&connection, "SecondWriter")
            .expect("find rolled-back label")
            .is_none()
    );
}

#[test]
fn stale_writer_on_second_connection_reports_branch_head_moved() {
    let (first_connection, path) = fresh_file_storage();
    let second_connection = Connection::open(&path).expect("second file connection");

    let first = prepare(
        &first_connection,
        "CREATE (:FirstConnectionWriter) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare first connection writer");
    let second = prepare(
        &second_connection,
        "CREATE (:SecondConnectionWriter) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare second connection writer");

    let mut first = QueryCursor::new(first);
    assert!(
        first
            .next_batch(&first_connection, 1)
            .expect("first connection batch")
            .done
    );
    first
        .complete(&first_connection)
        .expect("first connection complete");
    let head = branch_head(&first_connection, "main").expect("first connection head");

    let mut second = QueryCursor::new(second);
    let error = second
        .next_batch(&second_connection, 1)
        .expect_err("second connection stale writer must fail CAS");
    assert_eq!(error.kind, QueryErrorKind::BranchHeadMoved);
    assert_eq!(
        branch_head(&second_connection, "main").expect("second connection head"),
        head
    );
    assert!(
        lithograph_core::storage::find_label(&second_connection, "SecondConnectionWriter")
            .expect("find rolled-back second-connection label")
            .is_none()
    );

    drop(second_connection);
    drop(first_connection);
    std::fs::remove_file(path).expect("remove file database");
}

#[test]
fn mutation_failure_rolls_back_identity_dictionary_layer_commit_and_branch() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("before");
    let commit_count_before: i64 = connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count");
    let error = execute(
        &connection,
        "CREATE (n:WillRollback) SET n.bad = {nested: 1} FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("Map property cannot persist");
    assert_eq!(error.kind, QueryErrorKind::Type);
    assert_eq!(branch_head(&connection, "main").expect("same head"), before);
    let commit_count_after: i64 = connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count after");
    assert_eq!(commit_count_after, commit_count_before);
    assert!(
        lithograph_core::storage::find_label(&connection, "WillRollback")
            .expect("rolled label")
            .is_none()
    );
    assert!(
        lithograph_core::storage::find_property_key(&connection, "bad")
            .expect("rolled key")
            .is_none()
    );
}

#[test]
fn unsupported_collection_projections_fail_instead_of_mutating_as_empty_values() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Source {copied:1}), (:Target {kept:2}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed projection targets");
    let before = branch_head(&connection, "main").expect("head before unsupported expressions");

    let error = execute(
        &connection,
        "MATCH (source:Source), (target:Target) SET target = source{.*} FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("unsupported map projection must not compile as an empty map");
    assert_eq!(error.kind, QueryErrorKind::Semantic);
    assert_eq!(
        branch_head(&connection, "main").expect("head after map projection"),
        before
    );
    assert_eq!(
        read_rows(&connection, "MATCH (target:Target) RETURN target.kept"),
        vec![vec![Value::Integer(2)]]
    );

    let error = execute(
        &connection,
        "CREATE (:ListProbe {items:[x IN [1, 2] | x]}) FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("unsupported list comprehension must not compile as an empty list");
    assert_eq!(error.kind, QueryErrorKind::Semantic);
    assert_eq!(
        branch_head(&connection, "main").expect("head after list comprehension"),
        before
    );
    assert!(
        lithograph_core::storage::find_label(&connection, "ListProbe")
            .expect("find rejected list-comprehension label")
            .is_none()
    );
}

#[test]
fn nested_map_return_preserves_only_the_declared_top_level_entries() {
    let connection = fresh_storage();
    let (rows, _) = execute(
        &connection,
        "CREATE (:MapResult) RETURN {outer:{nested:1}}",
        ExecutionOptions::default(),
    )
    .expect("nested map result");

    assert_eq!(
        rows,
        vec![vec![Value::Map(BTreeMap::from([(
            "outer".to_owned(),
            Value::Map(BTreeMap::from([("nested".to_owned(), Value::Integer(1))])),
        )]))]]
    );
}

#[test]
fn historical_snapshot_is_read_only_but_prior_versions_remain_queryable() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (n:Person {name:'Alice', age:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("create");
    let created = branch_head(&connection, "main").expect("created head");
    execute(
        &connection,
        "MATCH (n:Person) SET n.age = 2 FINISH",
        ExecutionOptions::default(),
    )
    .expect("update");
    assert_eq!(
        read_rows(&connection, "MATCH (n:Person) RETURN n.age"),
        vec![vec![Value::Integer(2)]]
    );
    let historical =
        ExecutionOptions::parse_text(&format!(r#"{{"at":"commit/{}"}}"#, created.to_hex()))
            .expect("historical options");
    let historical_rows = execute(
        &connection,
        "MATCH (n:Person) RETURN n.age",
        historical.clone(),
    )
    .expect("historical read")
    .0;
    assert_eq!(historical_rows, vec![vec![Value::Integer(1)]]);
    let error = execute(
        &connection,
        "MATCH (n:Person) SET n.age = 3 FINISH",
        historical,
    )
    .expect_err("historical mutation");
    assert_eq!(error.kind, QueryErrorKind::ReadOnlySnapshot);

    let mut manually_selected = ExecutionOptions::default();
    manually_selected.snapshot = lithograph_core::query::SnapshotSelector::Commit(created.to_hex());
    let error = execute(
        &connection,
        "MATCH (n:Person) SET n.age = 4 FINISH",
        manually_selected,
    )
    .expect_err("manually selected historical mutation");
    assert_eq!(error.kind, QueryErrorKind::ReadOnlySnapshot);
}

#[test]
fn set_remove_delete_and_detach_preserve_historical_snapshots() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (a:Person {name:'A', temp:'x'})-[:R]->(b:Person {name:'B'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed history");
    let created = branch_head(&connection, "main").expect("created head");

    execute(
        &connection,
        "MATCH (a:Person) WHERE a.name = 'A' SET a.name = 'A2' REMOVE a.temp FINISH",
        ExecutionOptions::default(),
    )
    .expect("set and remove");
    let updated = branch_head(&connection, "main").expect("updated head");

    execute(
        &connection,
        "MATCH (a:Person) WHERE a.name = 'A2' DETACH DELETE a FINISH",
        ExecutionOptions::default(),
    )
    .expect("detach delete");
    let detached = branch_head(&connection, "main").expect("detached head");

    execute(
        &connection,
        "MATCH (b:Person) WHERE b.name = 'B' DELETE b FINISH",
        ExecutionOptions::default(),
    )
    .expect("plain delete");

    let at_created =
        ExecutionOptions::parse_text(&format!(r#"{{"at":"commit/{}"}}"#, created.to_hex()))
            .expect("created snapshot options");
    assert_eq!(
        execute(
            &connection,
            "MATCH (a:Person) WHERE a.name = 'A' RETURN a.temp",
            at_created.clone(),
        )
        .expect("created snapshot property")
        .0,
        vec![vec![Value::String("x".to_owned())]]
    );
    assert_eq!(
        execute(
            &connection,
            "MATCH ()-[r:R]->() RETURN count(r)",
            at_created,
        )
        .expect("created snapshot relationship")
        .0,
        vec![vec![Value::Integer(1)]]
    );

    let at_updated =
        ExecutionOptions::parse_text(&format!(r#"{{"at":"commit/{}"}}"#, updated.to_hex()))
            .expect("updated snapshot options");
    assert_eq!(
        execute(
            &connection,
            "MATCH (a:Person) WHERE a.name = 'A2' RETURN a.temp",
            at_updated,
        )
        .expect("updated snapshot")
        .0,
        vec![vec![Value::Null]]
    );

    let at_detached =
        ExecutionOptions::parse_text(&format!(r#"{{"at":"commit/{}"}}"#, detached.to_hex()))
            .expect("detached snapshot options");
    assert_eq!(
        execute(&connection, "MATCH (n:Person) RETURN n.name", at_detached,)
            .expect("detached snapshot")
            .0,
        vec![vec![Value::String("B".to_owned())]]
    );
    assert_eq!(
        read_rows(&connection, "MATCH (n:Person) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
}
