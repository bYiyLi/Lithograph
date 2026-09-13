use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, QuerySummary, prepare,
};
use lithograph_core::storage::{
    HashId, SnapshotState, branch_head, commit_exists, create_storage_schema,
    initialize_connection_state, initialize_root, load_commit, load_snapshot_state,
};
use rusqlite::Connection;

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
                 id INTEGER PRIMARY KEY CHECK(id=1),\
                 magic TEXT NOT NULL,\
                 database_id TEXT NOT NULL,\
                 storage_format INTEGER NOT NULL\
             );\
             INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format)\
             VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000009', 2);",
        )
        .expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    initialize_connection_state(&connection).expect("connection state");
    connection
}

fn execute(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), QueryError> {
    let prepared = prepare(connection, query, BTreeMap::new(), options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 64)?;
        rows.extend(batch.rows);
        if batch.done {
            return Ok((rows, cursor.complete(connection)?));
        }
    }
}

fn call(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    execute(connection, query, ExecutionOptions::default())
        .unwrap_or_else(|error| panic!("{query}: {error}"))
        .0
}

fn options(text: &str) -> ExecutionOptions {
    ExecutionOptions::parse_text(text).expect("valid options")
}

fn descriptor(commit: HashId) -> String {
    format!("commit/{}", commit.to_hex())
}

fn commit_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count")
}

fn map(value: &Value) -> &BTreeMap<String, Value> {
    match value {
        Value::Map(value) => value,
        other => panic!("expected map, got {other:?}"),
    }
}

fn string(value: &Value) -> &str {
    match value {
        Value::String(value) => value,
        other => panic!("expected string, got {other:?}"),
    }
}

fn integer(value: &Value) -> i64 {
    match value {
        Value::Integer(value) => *value,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn snapshot(connection: &Connection, commit: HashId) -> SnapshotState {
    load_snapshot_state(connection, commit).expect("snapshot state")
}

fn paged_log(connection: &Connection, start: HashId) -> Vec<HashId> {
    let mut cursor: Option<String> = None;
    let mut commits = Vec::new();
    loop {
        let query = match cursor.as_deref() {
            Some(cursor) => format!(
                "CALL lithograph.log('{}', 1, '{cursor}') YIELD commit, cursor RETURN commit, cursor",
                descriptor(start)
            ),
            None => format!(
                "CALL lithograph.log('{}', 1) YIELD commit, cursor RETURN commit, cursor",
                descriptor(start)
            ),
        };
        let rows = call(connection, &query);
        if rows.is_empty() {
            break;
        }
        let commit = string(&rows[0][0]);
        commits.push(HashId::from_hex(&commit[7..]).expect("log commit id"));
        cursor = match &rows[0][1] {
            Value::Null => None,
            Value::String(value) => Some(value.clone()),
            other => panic!("unexpected log cursor {other:?}"),
        };
        if cursor.is_none() {
            break;
        }
    }
    commits
}

#[test]
fn branch_tag_and_historical_reads_follow_version_contract() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Person {name:'base'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base write");
    let base = branch_head(&connection, "main").expect("base head");
    let physical_before: (i64, i64, i64) = connection
        .query_row(
            "SELECT (SELECT count(*) FROM main._lithograph_commits), (SELECT count(*) FROM main._lithograph_layers), (SELECT count(*) FROM main._lithograph_checkpoints)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("physical counts before cheap branch");

    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('feature', '{}') YIELD name, commit RETURN name, commit",
            descriptor(base)
        ),
    );
    let physical_after: (i64, i64, i64) = connection
        .query_row(
            "SELECT (SELECT count(*) FROM main._lithograph_commits), (SELECT count(*) FROM main._lithograph_layers), (SELECT count(*) FROM main._lithograph_checkpoints)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("physical counts after cheap branch");
    assert_eq!(physical_after, physical_before);
    let feature_before_historical_write =
        branch_head(&connection, "feature").expect("feature before historical write");
    let historical_write = execute(
        &connection,
        "CALL lithograph.commit.create() YIELD commit RETURN commit",
        options(r#"{"at":"branch/feature"}"#),
    )
    .expect_err("options.at Branch must stay read-only for Version mutation");
    assert_eq!(historical_write.kind, QueryErrorKind::ReadOnlySnapshot);
    assert_eq!(
        branch_head(&connection, "feature").expect("feature after historical write"),
        feature_before_historical_write
    );
    call(
        &connection,
        "CALL lithograph.branch.checkout('feature') YIELD name RETURN name",
    );
    let active_delete = execute(
        &connection,
        "CALL lithograph.branch.delete('feature') YIELD name RETURN name",
        ExecutionOptions::default(),
    )
    .expect_err("active Branch cannot be deleted");
    assert_eq!(active_delete.kind, QueryErrorKind::InvalidArgument);
    execute(
        &connection,
        "CREATE (:Person {name:'feature'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("feature write");
    let feature = branch_head(&connection, "feature").expect("feature head");

    call(
        &connection,
        "CALL lithograph.branch.checkout('main') YIELD name RETURN name",
    );
    execute(
        &connection,
        "CREATE (:Person {name:'main'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("main write");
    let main = branch_head(&connection, "main").expect("main head");
    assert_ne!(feature, main);

    let (main_rows, _) = execute(
        &connection,
        "MATCH (n:Person) RETURN n.name ORDER BY n.name",
        ExecutionOptions::default(),
    )
    .expect("main read");
    assert_eq!(
        main_rows,
        vec![
            vec![Value::String("base".to_owned())],
            vec![Value::String("main".to_owned())],
        ]
    );
    let (feature_rows, _) = execute(
        &connection,
        "MATCH (n:Person) RETURN n.name ORDER BY n.name",
        options(r#"{"at":"branch/feature"}"#),
    )
    .expect("feature historical read");
    assert_eq!(
        feature_rows,
        vec![
            vec![Value::String("base".to_owned())],
            vec![Value::String("feature".to_owned())],
        ]
    );
}

#[test]
fn tag_descriptors_are_pinned_and_can_share_branch_names() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Person {name:'tagged'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("tag base");
    let tagged = branch_head(&connection, "main").expect("tagged commit");
    call(
        &connection,
        &format!(
            "CALL lithograph.tag.create('snap', '{}') YIELD name RETURN name",
            descriptor(tagged)
        ),
    );
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('snap', '{}') YIELD name RETURN name",
            descriptor(tagged)
        ),
    );
    call(
        &connection,
        "CALL lithograph.branch.create('from-tag', 'tag/snap') YIELD name RETURN name",
    );
    assert_eq!(
        branch_head(&connection, "from-tag").expect("branch from Tag"),
        tagged
    );
    execute(
        &connection,
        "CREATE (:Person {name:'later'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("move main after Tag creation");
    let (tag_rows, _) = execute(
        &connection,
        "MATCH (n:Person) RETURN n.name ORDER BY n.name",
        options(r#"{"at":"tag/snap"}"#),
    )
    .expect("Tag remains pinned");
    assert_eq!(tag_rows, vec![vec![Value::String("tagged".to_owned())]]);
}

#[test]
fn commit_data_and_ref_boundaries_follow_version_contract() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Person {name:'feature'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("feature write");
    let feature = branch_head(&connection, "main").expect("feature commit");
    let set = call(
        &connection,
        &format!(
            "CALL lithograph.commit.data.set('{}', {{note:'kept', count:2}}) YIELD commit, data RETURN commit, data",
            descriptor(feature)
        ),
    );
    assert_eq!(string(&set[0][0]), descriptor(feature));
    assert_eq!(map(&set[0][1])["note"], Value::String("kept".to_owned()));
    let get = call(
        &connection,
        &format!(
            "CALL lithograph.commit.get('{}') YIELD commit, hasData, data RETURN commit, hasData, data",
            descriptor(feature)
        ),
    );
    assert_eq!(get[0][1], Value::Boolean(true));
    assert_eq!(map(&get[0][2])["count"], Value::Integer(2));
    assert_commit_data_json_shapes(&connection, feature);
    call(
        &connection,
        &format!(
            "CALL lithograph.commit.data.clear('{}') YIELD commit RETURN commit",
            descriptor(feature)
        ),
    );
    let get = call(
        &connection,
        &format!(
            "CALL lithograph.commit.get('{}') YIELD hasData, data RETURN hasData, data",
            descriptor(feature)
        ),
    );
    assert_eq!(get, vec![vec![Value::Boolean(false), Value::Null]]);

    let invalid = execute(
        &connection,
        "CALL lithograph.branch.create('bad//name') YIELD name RETURN name",
        ExecutionOptions::default(),
    )
    .expect_err("invalid Branch name");
    assert_eq!(invalid.kind, QueryErrorKind::InvalidArgument);
    let main_delete = execute(
        &connection,
        "CALL lithograph.branch.delete('main') YIELD name RETURN name",
        ExecutionOptions::default(),
    )
    .expect_err("main cannot be deleted");
    assert_eq!(main_delete.kind, QueryErrorKind::InvalidArgument);

    connection.execute_batch("BEGIN").expect("outer begin");
    let checkout = execute(
        &connection,
        "CALL lithograph.branch.checkout('feature') YIELD name RETURN name",
        ExecutionOptions::default(),
    )
    .expect_err("checkout in caller transaction");
    assert_eq!(checkout.kind, QueryErrorKind::TransactionBoundaryRequired);
    connection
        .execute_batch("ROLLBACK")
        .expect("outer rollback");
}

fn assert_commit_data_json_shapes(connection: &Connection, commit: HashId) {
    for (literal, expected) in [
        (
            "[1, 'x', true, null]",
            Value::List(vec![
                Value::Integer(1),
                Value::String("x".to_owned()),
                Value::Boolean(true),
                Value::Null,
            ]),
        ),
        ("'text'", Value::String("text".to_owned())),
        ("42", Value::Integer(42)),
        ("false", Value::Boolean(false)),
        ("null", Value::Null),
    ] {
        call(
            connection,
            &format!(
                "CALL lithograph.commit.data.set('{}', {literal}) YIELD commit RETURN commit",
                descriptor(commit)
            ),
        );
        let stored = call(
            connection,
            &format!(
                "CALL lithograph.commit.get('{}') YIELD hasData, data RETURN hasData, data",
                descriptor(commit)
            ),
        );
        assert_eq!(stored[0][0], Value::Boolean(true));
        assert_eq!(stored[0][1], expected);
        assert_eq!(
            branch_head(connection, "main").expect("stable head"),
            commit
        );
    }
}

#[test]
fn explicit_commit_and_log_cursor_pin_the_initial_dag() {
    let connection = fresh_storage();
    let root = branch_head(&connection, "main").expect("root");
    let created = execute(
        &connection,
        "CALL lithograph.commit.create({kind:'checkpoint'}) YIELD commit RETURN commit",
        options(r#"{"author":"alice","message":"empty state node"}"#),
    )
    .expect("explicit commit")
    .0;
    let first = HashId::from_hex(string(&created[0][0])[7..].as_ref()).expect("commit id");
    assert_ne!(first, root);
    assert_eq!(snapshot(&connection, first), snapshot(&connection, root));
    let record = load_commit(&connection, first).expect("commit record");
    assert_eq!(record.parent1, Some(root));
    assert_eq!(record.metadata.author.as_deref(), Some("alice"));
    assert_eq!(record.metadata.message.as_deref(), Some("empty state node"));
    let initial_data = call(
        &connection,
        &format!(
            "CALL lithograph.commit.get('{}') YIELD hasData, data RETURN hasData, data",
            descriptor(first)
        ),
    );
    assert_eq!(initial_data[0][0], Value::Boolean(true));
    assert_eq!(
        map(&initial_data[0][1])["kind"],
        Value::String("checkpoint".to_owned())
    );

    execute(
        &connection,
        "CREATE (:N {v:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("write 1");
    let second = branch_head(&connection, "main").expect("second");
    execute(
        &connection,
        "CREATE (:N {v:2}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("write 2");
    let third = branch_head(&connection, "main").expect("third");
    call(
        &connection,
        &format!(
            "CALL lithograph.tag.create('history', '{}') YIELD name RETURN name",
            descriptor(third)
        ),
    );

    let page1 = call(
        &connection,
        "CALL lithograph.log('tag/history', 1) YIELD commit, cursor RETURN commit, cursor",
    );
    assert_eq!(string(&page1[0][0]), descriptor(third));
    let cursor = string(&page1[0][1]).to_owned();
    call(
        &connection,
        &format!(
            "CALL lithograph.tag.move('history', '{}') YIELD name RETURN name",
            descriptor(root)
        ),
    );
    let page2 = call(
        &connection,
        &format!("CALL lithograph.log('tag/history', 1, '{cursor}') YIELD commit RETURN commit"),
    );
    assert_eq!(string(&page2[0][0]), descriptor(second));
}

#[path = "phase09_version/patch.rs"]
mod patch_tests;

fn ready_merge_fixture() -> (Connection, HashId, String, i64, i64) {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {name:'base', left:0, right:0, embedding: vector([1.0, 0.0], 2, FLOAT64)}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base");
    execute(
        &connection,
        "CREATE VECTOR INDEX item_embedding FOR (n:Item) ON (n.embedding) OPTIONS {indexConfig: {`vector.dimensions`: 2, `vector.similarity_function`: 'cosine'}}",
        ExecutionOptions::default(),
    )
    .expect("candidate search index");
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
    .expect("main disjoint change");
    execute(
        &connection,
        "MATCH (n:Item) SET n.right=2 FINISH",
        options(r#"{"branch":"feature"}"#),
    )
    .expect("feature disjoint change");
    let before_commits = commit_count(&connection);
    let started = call(
        &connection,
        "CALL lithograph.merge.start('branch/feature') YIELD session, revision, status, unresolved RETURN session, revision, status, unresolved",
    );
    assert_eq!(started[0][2], Value::String("ready".to_owned()));
    assert_eq!(started[0][3], Value::Integer(0));
    assert_eq!(commit_count(&connection), before_commits);
    (
        connection,
        base,
        string(&started[0][0]).to_owned(),
        integer(&started[0][1]),
        before_commits,
    )
}

#[test]
fn merge_candidate_supports_graph_search_schema_and_read_only() {
    let (connection, _, session, revision, _) = ready_merge_fixture();
    let candidate_options = options(&format!(
        r#"{{"mergeSession":{{"id":"{session}","revision":{revision}}}}}"#
    ));
    let (candidate, summary) = execute(
        &connection,
        "MATCH (n:Item) RETURN n.left, n.right",
        candidate_options.clone(),
    )
    .expect("candidate read");
    assert_eq!(candidate, vec![vec![Value::Integer(1), Value::Integer(2)]]);
    assert_eq!(summary.commit, None);
    assert_eq!(
        summary.merge_session.as_ref().expect("session summary").id,
        session
    );
    let (search, summary) = execute(
        &connection,
        "MATCH (n:Item) SEARCH n IN (VECTOR INDEX item_embedding FOR vector([1.0, 0.0], 2, FLOAT64) LIMIT 1) SCORE AS score RETURN n.left, n.right",
        candidate_options.clone(),
    )
    .expect("candidate vector search");
    assert_eq!(search, vec![vec![Value::Integer(1), Value::Integer(2)]]);
    assert!(summary.commit.is_none());
    let (indexes, summary) = execute(
        &connection,
        "SHOW INDEXES YIELD name RETURN name",
        candidate_options.clone(),
    )
    .expect("candidate schema introspection");
    assert!(indexes.contains(&vec![Value::String("item_embedding".to_owned())]));
    assert!(summary.commit.is_none());
    let error = execute(
        &connection,
        "CREATE (:CandidateMustStayReadOnly) FINISH",
        candidate_options,
    )
    .expect_err("candidate mutation must be rejected");
    assert_eq!(error.kind, QueryErrorKind::ReadOnlySnapshot);
    call(
        &connection,
        &format!(
            "CALL lithograph.merge.abort('{session}', {revision}) YIELD session RETURN session"
        ),
    );
}

#[test]
fn merge_finalize_creates_two_parent_commit_and_log_dag() {
    let (connection, base, session, revision, before_commits) = ready_merge_fixture();
    let finalized = execute(
        &connection,
        &format!(
            "CALL lithograph.merge.finalize('{session}', {revision}) YIELD status, commit RETURN status, commit"
        ),
        options(r#"{"author":"merge-author","message":"merge-message"}"#),
    )
    .expect("merge finalize with metadata")
    .0;
    assert_eq!(finalized[0][0], Value::String("merged".to_owned()));
    assert_eq!(commit_count(&connection), before_commits + 1);
    let merged = branch_head(&connection, "main").expect("merged head");
    let record = load_commit(&connection, merged).expect("merge record");
    assert_eq!(record.metadata.author.as_deref(), Some("merge-author"));
    assert_eq!(record.metadata.message.as_deref(), Some("merge-message"));
    let parent1 = record.parent1.expect("merge first parent");
    let parent2 = record.parent2.expect("merge second parent");
    let log = paged_log(&connection, merged);
    assert_eq!(log.first(), Some(&merged));
    assert_eq!(
        log.iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        log.len()
    );
    let base_position = log
        .iter()
        .position(|commit| *commit == base)
        .expect("merge base");
    assert!(
        log.iter()
            .position(|commit| *commit == parent1)
            .expect("parent1")
            < base_position
    );
    assert!(
        log.iter()
            .position(|commit| *commit == parent2)
            .expect("parent2")
            < base_position
    );
    let page = call(
        &connection,
        &format!(
            "CALL lithograph.log('{}', 1) YIELD cursor RETURN cursor",
            descriptor(merged)
        ),
    );
    let mut tampered = string(&page[0][0]).to_owned();
    let last = tampered.pop().expect("cursor checksum byte");
    tampered.push(if last == '0' { '1' } else { '0' });
    let error = execute(
        &connection,
        &format!(
            "CALL lithograph.log('{}', 1, '{tampered}') YIELD commit RETURN commit",
            descriptor(merged)
        ),
        ExecutionOptions::default(),
    )
    .expect_err("tampered log cursor must fail");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
}

fn property_conflict_fixture() -> (Connection, String, String, i64) {
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
            "CALL lithograph.branch.create('conflict', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.name='ours' FINISH",
        ExecutionOptions::default(),
    )
    .expect("ours");
    execute(
        &connection,
        "MATCH (n:Item) SET n.name='theirs' FINISH",
        options(r#"{"branch":"conflict"}"#),
    )
    .expect("theirs");
    let before_commits = commit_count(&connection);
    let started = call(
        &connection,
        "CALL lithograph.merge.start('branch/conflict') YIELD session, revision, status, unresolved RETURN session, revision, status, unresolved",
    );
    assert_eq!(started[0][2], Value::String("conflicted".to_owned()));
    assert_eq!(started[0][3], Value::Integer(1));
    let session = string(&started[0][0]).to_owned();
    let conflicts = call(
        &connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 10) YIELD conflictId, slot, base, ours, theirs RETURN conflictId, slot, base, ours, theirs"
        ),
    );
    assert_eq!(conflicts.len(), 1);
    assert!(string(&conflicts[0][1]).ends_with("/property/name"));
    assert_eq!(conflicts[0][2], Value::String("base".to_owned()));
    assert_eq!(conflicts[0][3], Value::String("ours".to_owned()));
    assert_eq!(conflicts[0][4], Value::String("theirs".to_owned()));
    (
        connection,
        session,
        string(&conflicts[0][0]).to_owned(),
        before_commits,
    )
}

#[test]
fn merge_resolution_is_revisioned_and_stale_candidate_finalize_fail() {
    let (connection, session, conflict_id, before_commits) = property_conflict_fixture();
    let unresolved = execute(
        &connection,
        "MATCH (n:Item) RETURN n.name",
        options(&format!(
            r#"{{"mergeSession":{{"id":"{session}","revision":1}}}}"#
        )),
    )
    .expect_err("unresolved candidate");
    assert_eq!(unresolved.kind, QueryErrorKind::MergeConflict);
    let resolved = call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{conflict_id}', choice:'ours'}}]) YIELD revision, status, unresolved RETURN revision, status, unresolved"
        ),
    );
    assert_eq!(resolved[0][0], Value::Integer(2));
    assert_eq!(resolved[0][2], Value::Integer(0));
    assert_eq!(commit_count(&connection), before_commits);
    let no_op = call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 2, [{{conflictId:'{conflict_id}', choice:'ours'}}]) YIELD revision RETURN revision"
        ),
    );
    assert_eq!(no_op[0][0], Value::Integer(2));
    let stale = execute(
        &connection,
        "MATCH (n:Item) RETURN n.name",
        options(&format!(
            r#"{{"mergeSession":{{"id":"{session}","revision":1}}}}"#
        )),
    )
    .expect_err("stale candidate revision");
    assert_eq!(stale.kind, QueryErrorKind::MergeSessionChanged);
    let (candidate, summary) = execute(
        &connection,
        "MATCH (n:Item) RETURN n.name",
        options(&format!(
            r#"{{"mergeSession":{{"id":"{session}","revision":2}}}}"#
        )),
    )
    .expect("resolved candidate");
    assert_eq!(candidate, vec![vec![Value::String("ours".to_owned())]]);
    assert!(summary.commit.is_none());
    let stale_finalize = execute(
        &connection,
        &format!("CALL lithograph.merge.finalize('{session}', 1) YIELD status RETURN status"),
        ExecutionOptions::default(),
    )
    .expect_err("stale finalize");
    assert_eq!(stale_finalize.kind, QueryErrorKind::MergeSessionChanged);
    let finalized = call(
        &connection,
        &format!(
            "CALL lithograph.merge.finalize('{session}', 2) YIELD status, commit RETURN status, commit"
        ),
    );
    assert_eq!(finalized[0][0], Value::String("merged".to_owned()));
}

fn derived_constraint_fixture() -> (Connection, String, String) {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE CONSTRAINT user_email_unique FOR (n:User) REQUIRE n.email IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("unique constraint");
    let base = branch_head(&connection, "main").expect("constraint base");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('constraint-feature', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "CREATE (:User {email:'same@example.test', side:'ours'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("ours stays individually valid");
    execute(
        &connection,
        "CREATE (:User {email:'same@example.test', side:'theirs'}) FINISH",
        options(r#"{"branch":"constraint-feature"}"#),
    )
    .expect("theirs stays individually valid");

    let started = call(
        &connection,
        "CALL lithograph.merge.start('branch/constraint-feature') YIELD session, revision, status, unresolved RETURN session, revision, status, unresolved",
    );
    assert_eq!(started[0][1], Value::Integer(1));
    assert_eq!(started[0][2], Value::String("conflicted".to_owned()));
    assert_eq!(started[0][3], Value::Integer(1));
    let session = string(&started[0][0]).to_owned();
    let conflicts = call(
        &connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 10) YIELD conflictId, slot RETURN conflictId, slot"
        ),
    );
    assert_eq!(conflicts.len(), 1);
    assert_eq!(
        conflicts[0][1],
        Value::String("constraint/user_email_unique".to_owned())
    );
    let conflict_id = string(&conflicts[0][0]).to_owned();
    (connection, session, conflict_id)
}

#[test]
fn derived_unique_constraint_conflict_requires_a_resolution_that_changes_candidate() {
    let (connection, session, conflict_id) = derived_constraint_fixture();
    let invalid_resolution = execute(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{conflict_id}', choice:'ours'}}]) YIELD revision RETURN revision"
        ),
        ExecutionOptions::default(),
    )
    .expect_err("keeping the same violating Constraint must not be accepted");
    assert_eq!(invalid_resolution.kind, QueryErrorKind::InvalidArgument);
    let after_invalid = call(
        &connection,
        &format!(
            "CALL lithograph.merge.get('{session}') YIELD revision, status, unresolved RETURN revision, status, unresolved"
        ),
    );
    assert_eq!(after_invalid[0][0], Value::Integer(1));
    assert_eq!(after_invalid[0][1], Value::String("conflicted".to_owned()));
    assert_eq!(after_invalid[0][2], Value::Integer(1));

    let resolved = call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{conflict_id}', choice:'value', value:null}}]) YIELD revision, status, unresolved RETURN revision, status, unresolved"
        ),
    );
    assert_eq!(resolved[0][0], Value::Integer(2));
    assert_eq!(resolved[0][1], Value::String("ready".to_owned()));
    assert_eq!(resolved[0][2], Value::Integer(0));

    let candidate_options = options(&format!(
        r#"{{"mergeSession":{{"id":"{session}","revision":2}}}}"#
    ));
    let (rows, summary) = execute(
        &connection,
        "MATCH (n:User) RETURN n.email, n.side ORDER BY n.side",
        candidate_options.clone(),
    )
    .expect("resolved candidate graph");
    assert_eq!(
        rows,
        vec![
            vec![
                Value::String("same@example.test".to_owned()),
                Value::String("ours".to_owned()),
            ],
            vec![
                Value::String("same@example.test".to_owned()),
                Value::String("theirs".to_owned()),
            ],
        ]
    );
    assert!(summary.commit.is_none());
    let (constraints, _) = execute(
        &connection,
        "SHOW CONSTRAINTS YIELD name RETURN name",
        candidate_options,
    )
    .expect("candidate constraint introspection");
    assert!(!constraints.contains(&vec![Value::String("user_email_unique".to_owned())]));

    let finalized = call(
        &connection,
        &format!(
            "CALL lithograph.merge.finalize('{session}', 2) YIELD status, commit RETURN status, commit"
        ),
    );
    assert_eq!(finalized[0][0], Value::String("merged".to_owned()));
    let final_constraints = call(&connection, "SHOW CONSTRAINTS YIELD name RETURN name");
    assert!(!final_constraints.contains(&vec![Value::String("user_email_unique".to_owned())]));
}

fn collect_conflict_pages(
    connection: &Connection,
    session: &str,
) -> (Vec<(String, String)>, String) {
    let mut cursor: Option<String> = None;
    let mut first_cursor = None;
    let mut conflicts = Vec::new();
    loop {
        let query = match cursor.as_deref() {
            Some(cursor) => format!(
                "CALL lithograph.merge.conflicts('{session}', 7, '{cursor}') YIELD conflictId, slot, cursor RETURN conflictId, slot, cursor"
            ),
            None => format!(
                "CALL lithograph.merge.conflicts('{session}', 7) YIELD conflictId, slot, cursor RETURN conflictId, slot, cursor"
            ),
        };
        let page = call(connection, &query);
        assert!(!page.is_empty());
        assert!(page.len() <= 7);
        let page_cursor = match &page.last().expect("page row")[2] {
            Value::Null => None,
            Value::String(value) => Some(value.clone()),
            other => panic!("unexpected conflict cursor {other:?}"),
        };
        if first_cursor.is_none() {
            first_cursor = page_cursor.clone();
        }
        conflicts.extend(
            page.iter()
                .map(|row| (string(&row[0]).to_owned(), string(&row[1]).to_owned())),
        );
        cursor = page_cursor;
        if cursor.is_none() {
            break;
        }
    }
    (conflicts, first_cursor.expect("first page cursor"))
}

#[test]
fn merge_conflicts_are_bounded_pageable_and_revision_pinned() {
    let connection = fresh_storage();
    execute(
        &connection,
        "UNWIND range(1, 200) AS i CREATE (:Bulk {i:i, v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("bulk base");
    let base = branch_head(&connection, "main").expect("bulk base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('bulk-conflicts', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Bulk) SET n.v=1 FINISH",
        ExecutionOptions::default(),
    )
    .expect("bulk ours");
    execute(
        &connection,
        "MATCH (n:Bulk) SET n.v=2 FINISH",
        options(r#"{"branch":"bulk-conflicts"}"#),
    )
    .expect("bulk theirs");
    let started = call(
        &connection,
        "CALL lithograph.merge.start('branch/bulk-conflicts') YIELD session, revision, status, unresolved RETURN session, revision, status, unresolved",
    );
    assert_eq!(started[0][1], Value::Integer(1));
    assert_eq!(started[0][2], Value::String("conflicted".to_owned()));
    assert_eq!(started[0][3], Value::Integer(200));
    let session = string(&started[0][0]).to_owned();

    let (conflicts, stale_cursor) = collect_conflict_pages(&connection, &session);
    assert_eq!(conflicts.len(), 200);
    assert_eq!(
        conflicts
            .iter()
            .map(|(id, _)| id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        200
    );
    assert!(
        conflicts
            .windows(2)
            .all(|window| window[0].1 <= window[1].1)
    );

    let first_conflict = &conflicts[0].0;
    let resolved = call(
        &connection,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{first_conflict}', choice:'ours'}}]) YIELD revision, unresolved RETURN revision, unresolved"
        ),
    );
    assert_eq!(resolved[0][0], Value::Integer(2));
    assert_eq!(resolved[0][1], Value::Integer(199));
    let stale = execute(
        &connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 7, '{stale_cursor}') YIELD conflictId RETURN conflictId"
        ),
        ExecutionOptions::default(),
    )
    .expect_err("old conflict cursor must be revision-pinned");
    assert_eq!(stale.kind, QueryErrorKind::MergeSessionChanged);
}

#[test]
fn rebase_conflict_rolls_back_and_success_rewrites_without_copying_commit_data_or_tags() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Item {v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('topic', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    call(
        &connection,
        "CALL lithograph.branch.checkout('topic') YIELD name RETURN name",
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.v=1 FINISH",
        options(r#"{"author":"topic-author","message":"topic-change"}"#),
    )
    .expect("topic write");
    let old_topic = branch_head(&connection, "topic").expect("old topic");
    call(
        &connection,
        &format!(
            "CALL lithograph.commit.data.set('{}', {{keep:'old-only'}}) YIELD commit RETURN commit",
            descriptor(old_topic)
        ),
    );
    call(
        &connection,
        &format!(
            "CALL lithograph.tag.create('topic-old', '{}') YIELD name RETURN name",
            descriptor(old_topic)
        ),
    );
    call(
        &connection,
        "CALL lithograph.branch.checkout('main') YIELD name RETURN name",
    );
    execute(
        &connection,
        "MATCH (n:Item) SET n.v=2 FINISH",
        ExecutionOptions::default(),
    )
    .expect("main write");
    let main = branch_head(&connection, "main").expect("main");
    call(
        &connection,
        "CALL lithograph.branch.checkout('topic') YIELD name RETURN name",
    );
    let count_before = commit_count(&connection);
    let conflict = call(
        &connection,
        "CALL lithograph.rebase('branch/main') YIELD status, commit, rewritten, conflicts RETURN status, commit, rewritten, conflicts",
    );
    assert_eq!(conflict[0][0], Value::String("conflicted".to_owned()));
    assert_eq!(conflict[0][1], Value::Null);
    assert_eq!(
        branch_head(&connection, "topic").expect("topic after conflict"),
        old_topic
    );
    assert_eq!(commit_count(&connection), count_before);
    let conflict_id = match &conflict[0][3] {
        Value::List(values) => string(&map(&values[0])["conflictId"]).to_owned(),
        other => panic!("expected conflict list, got {other:?}"),
    };

    let rebased = call(
        &connection,
        &format!(
            "CALL lithograph.rebase('branch/main', {{resolutions:[{{conflictId:'{conflict_id}', choice:'theirs'}}]}}) YIELD status, commit, rewritten RETURN status, commit, rewritten"
        ),
    );
    assert_eq!(rebased[0][0], Value::String("rebased".to_owned()));
    let new_topic = branch_head(&connection, "topic").expect("new topic");
    assert_ne!(new_topic, old_topic);
    assert_ne!(new_topic, main);
    let new_record = load_commit(&connection, new_topic).expect("rebased commit");
    assert_eq!(new_record.metadata.author.as_deref(), Some("topic-author"));
    assert_eq!(new_record.metadata.message.as_deref(), Some("topic-change"));
    let data = call(
        &connection,
        &format!(
            "CALL lithograph.commit.get('{}') YIELD hasData RETURN hasData",
            descriptor(new_topic)
        ),
    );
    assert_eq!(data, vec![vec![Value::Boolean(false)]]);
    let tags = call(
        &connection,
        "CALL lithograph.tag.list() YIELD name, commit RETURN name, commit",
    );
    assert!(tags.iter().any(|row| {
        row[0] == Value::String("topic-old".to_owned()) && string(&row[1]) == descriptor(old_topic)
    }));
}

#[test]
fn squash_preserves_snapshot_metadata_and_old_history() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:N {v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base");
    let base = branch_head(&connection, "main").expect("base");
    execute(
        &connection,
        "MATCH (n:N) SET n.v=1 FINISH",
        ExecutionOptions::default(),
    )
    .expect("v1");
    let one = branch_head(&connection, "main").expect("one");
    execute(
        &connection,
        "MATCH (n:N) SET n.v=2 FINISH",
        ExecutionOptions::default(),
    )
    .expect("v2");
    let old_head = branch_head(&connection, "main").expect("old head");
    let old_state = snapshot(&connection, old_head);
    call(
        &connection,
        &format!(
            "CALL lithograph.commit.data.set('{}', {{legacy:true}}) YIELD commit RETURN commit",
            descriptor(old_head)
        ),
    );
    call(
        &connection,
        &format!(
            "CALL lithograph.tag.create('old-squash', '{}') YIELD name RETURN name",
            descriptor(old_head)
        ),
    );
    let squashed = execute(
        &connection,
        &format!(
            "CALL lithograph.squash('{}') YIELD commit RETURN commit",
            descriptor(base)
        ),
        options(r#"{"author":"squash-author","message":"squash-message"}"#),
    )
    .expect("squash")
    .0;
    let squashed_id = HashId::from_hex(&string(&squashed[0][0])[7..]).expect("squashed id");
    assert_eq!(snapshot(&connection, squashed_id), old_state);
    let squash_record = load_commit(&connection, squashed_id).expect("squash record");
    assert_eq!(squash_record.parent1, Some(base));
    assert_eq!(
        squash_record.metadata.author.as_deref(),
        Some("squash-author")
    );
    assert_eq!(
        squash_record.metadata.message.as_deref(),
        Some("squash-message")
    );
    let squash_data = call(
        &connection,
        &format!(
            "CALL lithograph.commit.get('{}') YIELD hasData RETURN hasData",
            descriptor(squashed_id)
        ),
    );
    assert_eq!(squash_data, vec![vec![Value::Boolean(false)]]);
    let old_tag = call(
        &connection,
        "CALL lithograph.tag.list() YIELD name, commit RETURN name, commit",
    );
    assert!(old_tag.iter().any(|row| {
        row[0] == Value::String("old-squash".to_owned()) && string(&row[1]) == descriptor(old_head)
    }));
    call(
        &connection,
        "CALL lithograph.tag.delete('old-squash') YIELD name RETURN name",
    );
    assert!(commit_exists(&connection, one).expect("old one exists"));
    assert!(commit_exists(&connection, old_head).expect("old head exists"));
}

#[test]
fn reset_revert_and_gc_respect_tag_roots() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:N {v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base");
    let base = branch_head(&connection, "main").expect("base");
    execute(
        &connection,
        "MATCH (n:N) SET n.v=1 FINISH",
        ExecutionOptions::default(),
    )
    .expect("v1");
    let one = branch_head(&connection, "main").expect("one");
    execute(
        &connection,
        "MATCH (n:N) SET n.v=2 FINISH",
        ExecutionOptions::default(),
    )
    .expect("v2");
    let old_head = branch_head(&connection, "main").expect("old head");
    call(
        &connection,
        &format!(
            "CALL lithograph.commit.data.set('{}', {{gc:'sidecar'}}) YIELD commit RETURN commit",
            descriptor(old_head)
        ),
    );
    call(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(one)
        ),
    );
    assert_eq!(branch_head(&connection, "main").expect("reset head"), one);
    let reverted = call(
        &connection,
        &format!(
            "CALL lithograph.revert('{}') YIELD commit RETURN commit",
            descriptor(one)
        ),
    );
    let reverted_id = HashId::from_hex(&string(&reverted[0][0])[7..]).expect("reverted id");
    assert_eq!(
        snapshot(&connection, reverted_id),
        snapshot(&connection, base)
    );

    call(
        &connection,
        &format!(
            "CALL lithograph.tag.create('protect', '{}') YIELD name RETURN name",
            descriptor(old_head)
        ),
    );
    call(
        &connection,
        "CALL lithograph.gc() YIELD commits RETURN commits",
    );
    assert!(commit_exists(&connection, old_head).expect("tag-protected history"));
    let protected_data: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_commit_data WHERE commit_id=?1",
            [old_head.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("tag-protected Commit Data");
    assert_eq!(protected_data, 1);
    call(
        &connection,
        "CALL lithograph.tag.delete('protect') YIELD name RETURN name",
    );
    call(
        &connection,
        "CALL lithograph.gc() YIELD commits RETURN commits",
    );
    assert!(!commit_exists(&connection, old_head).expect("unreachable history collected"));
    let collected_data: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_commit_data WHERE commit_id=?1",
            [old_head.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("collected Commit Data");
    assert_eq!(collected_data, 0);
}

#[path = "phase09_version/lifecycle.rs"]
mod lifecycle;

#[path = "phase09_version/merge_semantics.rs"]
mod merge_semantics;

#[path = "phase09_version/concurrency.rs"]
mod concurrency;
