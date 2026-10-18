use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, QuerySummary, prepare,
};
use lithograph_core::storage::{branch_head, create_storage_schema, initialize_root};
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
            );",
        )
        .expect("metadata table");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn options(text: &str) -> ExecutionOptions {
    ExecutionOptions::parse_text(text).expect("valid access options")
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
        let batch = cursor.next_batch(connection, 2)?;
        rows.extend(batch.rows);
        if batch.done {
            return Ok((rows, cursor.complete(connection)?));
        }
    }
}

fn rows(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    execute(connection, query, ExecutionOptions::default())
        .expect("query without restrictions")
        .0
}

fn commit_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("Commit count")
}

fn assert_rejected(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
    kind: QueryErrorKind,
) {
    let head = branch_head(connection, "main").expect("head before rejection");
    let commits = commit_count(connection);
    let error = execute(connection, query, options).expect_err(query);
    assert_eq!(error.kind, kind, "{query}: {error}");
    assert_eq!(
        branch_head(connection, "main").expect("head after rejection"),
        head,
        "rejected query must preserve Branch head: {query}"
    );
    assert_eq!(
        commit_count(connection),
        commits,
        "rejected query must not leave a Commit: {query}"
    );
}

fn assert_denied(connection: &Connection, query: &str, options: ExecutionOptions) {
    assert_rejected(connection, query, options, QueryErrorKind::AccessDenied);
}

fn seed_person(connection: &Connection) {
    execute(
        connection,
        "CREATE (:Person {name:'Alice', secret:'classified-value', `@core:key`:'person-1', audit_1:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed Person");
}

#[test]
fn omitted_empty_and_explicitly_allowed_rules_preserve_writes() {
    for text in [
        "{}",
        r#"{"readAccess":{},"writeAccess":{}}"#,
        r#"{"writeAccess":{"denyProperties":[],"denyPropertyPrefixes":[],"denyPropertyPatterns":[]}}"#,
        r#"{"writeAccess":{"allowCreate":true,"allowDelete":true,"allowLabelChanges":true,"allowSchemaChanges":true,"allowVersionChanges":true,"allowMaintenance":true}}"#,
    ] {
        let connection = fresh_storage();
        let initial = branch_head(&connection, "main").expect("initial head");
        let policy = options(text);
        execute(
            &connection,
            "CREATE (n:Default {secret:1}) SET n.secret = 2, n:Extra RETURN n.secret",
            policy.clone(),
        )
        .expect("default policy preserves CREATE/SET/Label");
        assert_ne!(branch_head(&connection, "main").expect("head"), initial);
        assert_eq!(
            rows(&connection, "MATCH (n:Default:Extra) RETURN n.secret"),
            vec![vec![Value::Integer(2)]]
        );
        execute(
            &connection,
            "MATCH (n:Default) REMOVE n:Extra DELETE n FINISH",
            policy,
        )
        .expect("default policy preserves REMOVE/DELETE");
        assert_eq!(
            rows(&connection, "MATCH (n) RETURN count(n)"),
            vec![vec![Value::Integer(0)]]
        );
    }
}

#[test]
fn write_property_rules_use_union_literal_prefixes_and_full_name_patterns() {
    let connection = fresh_storage();
    seed_person(&connection);
    let policy = options(
        r#"{"writeAccess":{"denyProperties":["secret"],"denyPropertyPrefixes":["@core:"],"denyPropertyPatterns":["audit_[0-9]+"]}}"#,
    );
    for query in [
        "MATCH (n:Person) SET n.secret = 'changed' FINISH",
        "MATCH (n:Person) SET n.`@core:new` = 2 FINISH",
        "MATCH (n:Person) SET n.audit_99 = 2 FINISH",
    ] {
        assert_denied(&connection, query, policy.clone());
    }
    execute(
        &connection,
        "MATCH (n:Person) SET n.Secret = 1, n.`@Core:new` = 2, n.prefix_audit_1 = 3, n.audit_1_suffix = 4 RETURN n.Secret, n.`@Core:new`, n.prefix_audit_1, n.audit_1_suffix",
        policy,
    )
    .map(|(rows, _)| {
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3),
                Value::Integer(4),
            ]]
        );
    })
    .expect("nearby ordinary names must remain writable");
}

#[test]
fn protected_node_properties_reject_explicit_writes_before_no_op_shortcuts() {
    let connection = fresh_storage();
    seed_person(&connection);
    let policy = options(r#"{"writeAccess":{"denyProperties":["secret","absent"]}}"#);
    for query in [
        "MATCH (n:Person) SET n.secret = 'changed' FINISH",
        "MATCH (n:Person) SET n.secret = 'classified-value' FINISH",
        "MATCH (n:Person) SET n.secret = null FINISH",
        "MATCH (n:Person) REMOVE n.secret FINISH",
        "MATCH (n:Person) SET n['secret'] = 'changed' FINISH",
        "MATCH (n:Person) REMOVE n['secret'] FINISH",
        "MATCH (n:Person) SET n.absent = null FINISH",
        "MATCH (n:Person) REMOVE n.absent FINISH",
        "MATCH (n:Person) SET n += {secret:'changed'} FINISH",
        "MATCH (n:Person) SET n += {absent:null} FINISH",
        "MATCH (n:Person) SET n = {name:'changed'} FINISH",
        "MATCH (n:Person) SET n = properties(n) FINISH",
    ] {
        assert_denied(&connection, query, policy.clone());
        assert_eq!(
            rows(&connection, "MATCH (n:Person) RETURN n.name, n.secret"),
            vec![vec![
                Value::String("Alice".to_owned()),
                Value::String("classified-value".to_owned()),
            ]]
        );
    }
}

#[test]
fn protected_relationship_properties_have_the_same_write_boundary() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Person)-[:KNOWS {secret:1, open:2}]->(:Person) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed relationship");
    let policy = options(r#"{"writeAccess":{"denyProperties":["secret"]}}"#);
    for query in [
        "MATCH ()-[r:KNOWS]->() SET r.secret = 1 FINISH",
        "MATCH ()-[r:KNOWS]->() SET r['secret'] = null FINISH",
        "MATCH ()-[r:KNOWS]->() REMOVE r.secret FINISH",
        "MATCH ()-[r:KNOWS]->() SET r += {secret:2} FINISH",
        "MATCH ()-[r:KNOWS]->() SET r = {open:3} FINISH",
    ] {
        assert_denied(&connection, query, policy.clone());
    }
    let (result, _) = execute(
        &connection,
        "MATCH ()-[r:KNOWS]->() SET r.open = 3 RETURN r.secret, r.open",
        policy,
    )
    .expect("unprotected relationship property");
    assert_eq!(result, vec![vec![Value::Integer(1), Value::Integer(3)]]);
}

#[test]
fn forbidden_initial_properties_rollback_nodes_relationships_and_prior_writes() {
    let connection = fresh_storage();
    let policy = options(r#"{"writeAccess":{"denyProperties":["secret"]}}"#);
    for query in [
        "CREATE (:Allowed {open:1}), (:Blocked {secret:1}) FINISH",
        "CREATE (:Blocked {secret:null}) FINISH",
        "CREATE (:Allowed)-[:BLOCKED {secret:1}]->(:Allowed) FINISH",
        "INSERT (:Blocked {secret:1}) FINISH",
        "CREATE (n:Allowed {open:1}) SET n.secret = 1 FINISH",
        "CALL { CREATE (:Allowed {open:1}) } CREATE (:Blocked {secret:1}) FINISH",
        "FOREACH (x IN [1,2] | CREATE (n:Allowed {open:x}) SET n[CASE x WHEN 2 THEN 'secret' ELSE 'open' END] = x) FINISH",
    ] {
        assert_denied(&connection, query, policy.clone());
        assert_eq!(
            rows(&connection, "MATCH (n) RETURN count(n)"),
            vec![vec![Value::Integer(0)]]
        );
    }
}

#[test]
fn merge_checks_only_the_executed_property_mutations() {
    let connection = fresh_storage();
    seed_person(&connection);
    let policy = options(r#"{"writeAccess":{"denyProperties":["secret"]}}"#);
    let (result, _) = execute(
        &connection,
        "MERGE (n:Person {secret:'classified-value'}) ON CREATE SET n.secret = 'blocked' ON MATCH SET n.name = 'Matched' RETURN n.name",
        policy.clone(),
    )
    .expect("protected match property and skipped ON CREATE are reads");
    assert_eq!(result, vec![vec![Value::String("Matched".to_owned())]]);
    for query in [
        "MERGE (n:Person {name:'New'}) ON CREATE SET n.secret = 'blocked' FINISH",
        "MERGE (n:Person {secret:'new-secret'}) FINISH",
        "MERGE (n:Person {name:'Matched'}) ON MATCH SET n.secret = 'classified-value' FINISH",
    ] {
        assert_denied(&connection, query, policy.clone());
    }
    assert_eq!(
        rows(&connection, "MATCH (n:Person) RETURN n.name, n.secret"),
        vec![vec![
            Value::String("Matched".to_owned()),
            Value::String("classified-value".to_owned()),
        ]]
    );
}

#[test]
fn creation_permission_does_not_restrict_existing_element_updates_or_merge_matches() {
    let connection = fresh_storage();
    seed_person(&connection);
    let policy = options(r#"{"writeAccess":{"allowCreate":false}}"#);
    let (result, _) = execute(
        &connection,
        "MERGE (n:Person {name:'Alice'}) ON MATCH SET n.name = 'Updated' RETURN n.name",
        policy.clone(),
    )
    .expect("MERGE existing element does not create");
    assert_eq!(result, vec![vec![Value::String("Updated".to_owned())]]);
    for query in [
        "CREATE (:Blocked) FINISH",
        "INSERT (:Blocked) FINISH",
        "MERGE (:Person {name:'Missing'}) FINISH",
        "MATCH (a:Person), (b:Person) CREATE (a)-[:BLOCKED]->(b) FINISH",
    ] {
        assert_denied(&connection, query, policy.clone());
    }
    execute(
        &connection,
        "MATCH (n:Person) SET n.name = 'StillWritable' FINISH",
        policy,
    )
    .expect("ordinary update is independent of creation permission");
    assert_eq!(
        rows(&connection, "MATCH (n:Person) RETURN n.name"),
        vec![vec![Value::String("StillWritable".to_owned())]]
    );
}

#[test]
fn deletion_permission_and_property_protection_are_independent() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Person {secret:1})-[:KNOWS {secret:2}]->(:Person {secret:3}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed deletion graph");
    let denied = options(r#"{"writeAccess":{"allowDelete":false}}"#);
    for query in [
        "MATCH ()-[r:KNOWS]->() DELETE r FINISH",
        "MATCH (n:Person) DETACH DELETE n FINISH",
    ] {
        assert_denied(&connection, query, denied.clone());
    }
    let allowed = options(
        r#"{"writeAccess":{"denyProperties":["secret"],"allowDelete":true,"allowCreate":false,"allowLabelChanges":false}}"#,
    );
    execute(
        &connection,
        "MATCH (n:Person) DETACH DELETE n FINISH",
        allowed,
    )
    .expect("authorized deletion cleans protected properties and existing Labels");
    assert_eq!(
        rows(&connection, "MATCH (n) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
    assert_eq!(
        rows(&connection, "MATCH ()-[r]->() RETURN count(r)"),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn label_permission_includes_initial_labels_but_allows_unlabeled_creation() {
    let connection = fresh_storage();
    seed_person(&connection);
    let policy = options(r#"{"writeAccess":{"allowLabelChanges":false}}"#);
    for query in [
        "MATCH (n:Person) SET n:Blocked FINISH",
        "MATCH (n:Person) REMOVE n:Person FINISH",
        "CREATE (:Blocked) FINISH",
        "INSERT (:Blocked) FINISH",
    ] {
        assert_denied(&connection, query, policy.clone());
    }
    let (unchanged, summary) = execute(
        &connection,
        "MATCH (n:Person) SET n:Person REMOVE n:Absent RETURN labels(n)",
        policy.clone(),
    )
    .expect("Label operations without an actual change remain allowed");
    assert_eq!(
        unchanged,
        vec![vec![Value::List(vec![Value::String("Person".to_owned())])]]
    );
    assert_eq!(summary.counters.labels_added, 0);
    assert_eq!(summary.counters.labels_removed, 0);
    let (missing, summary) = execute(
        &connection,
        "OPTIONAL MATCH (n:Missing) SET n:Blocked REMOVE n:Blocked RETURN n",
        policy.clone(),
    )
    .expect("null Label target causes no Label effect");
    assert_eq!(missing, vec![vec![Value::Null]]);
    assert_eq!(summary.counters.labels_added, 0);
    assert_eq!(summary.counters.labels_removed, 0);
    let (result, _) = execute(
        &connection,
        "CREATE (n {open:1}) RETURN labels(n), n.open",
        policy,
    )
    .expect("creation without a Label has no Label effect");
    assert_eq!(
        result,
        vec![vec![Value::List(Vec::new()), Value::Integer(1)]]
    );
}

#[test]
fn schema_permission_is_independent_of_property_and_version_permissions() {
    let connection = fresh_storage();
    seed_person(&connection);
    let denied = options(r#"{"writeAccess":{"allowSchemaChanges":false}}"#);
    for query in [
        "CREATE RANGE INDEX blocked_index FOR (n:Person) ON (n.secret)",
        "CREATE CONSTRAINT blocked_constraint FOR (n:Person) REQUIRE n.secret IS UNIQUE",
        "ALTER CURRENT GRAPH TYPE SET { (:Person => {name :: STRING}) }",
    ] {
        assert_denied(&connection, query, denied.clone());
    }
    execute(
        &connection,
        "MATCH (n:Person) SET n.name = 'Allowed' FINISH",
        denied.clone(),
    )
    .expect("Schema prohibition does not prohibit graph writes");
    let allowed = options(
        r#"{"writeAccess":{"denyProperties":["secret"],"allowCreate":false,"allowDelete":false,"allowLabelChanges":false,"allowVersionChanges":false,"allowMaintenance":false}}"#,
    );
    execute(
        &connection,
        "CREATE RANGE INDEX protected_name_index FOR (n:Person) ON (n.secret)",
        allowed.clone(),
    )
    .expect("Schema names are not graph-property writes or explicit version operations");
    assert_eq!(
        rows(&connection, "SHOW INDEXES YIELD name RETURN name"),
        vec![vec![Value::String("protected_name_index".to_owned())]]
    );
    assert_denied(&connection, "DROP INDEX protected_name_index", denied);
    execute(&connection, "DROP INDEX protected_name_index", allowed)
        .expect("permitted Schema deletion");
    assert!(rows(&connection, "SHOW INDEXES YIELD name RETURN name").is_empty());
}

#[test]
fn query_masking_does_not_weaken_canonical_constraint_validation() {
    let connection = fresh_storage();
    seed_person(&connection);
    execute(
        &connection,
        "CREATE CONSTRAINT person_secret_unique FOR (n:Person) REQUIRE n.secret IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("canonical unique constraint");
    let policy = options(r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#);
    assert_rejected(
        &connection,
        "CREATE (:Person {name:'Duplicate', secret:'classified-value'}) FINISH",
        policy.clone(),
        QueryErrorKind::Constraint,
    );
    let (result, _) = execute(
        &connection,
        "CREATE (n:Person {name:'Unique', secret:'different'}) RETURN n.name, n.secret",
        policy,
    )
    .expect("different canonical value succeeds while query reads remain masked");
    assert_eq!(
        result,
        vec![vec![Value::String("Unique".to_owned()), Value::Null]]
    );
}

#[test]
fn permission_allowances_cannot_override_property_or_existing_read_only_guards() {
    let connection = fresh_storage();
    seed_person(&connection);
    assert_denied(
        &connection,
        "MATCH (n:Person) SET n.secret = 'changed' FINISH",
        options(
            r#"{"writeAccess":{"denyProperties":["secret"],"allowCreate":true,"allowDelete":true,"allowLabelChanges":true,"allowSchemaChanges":true,"allowVersionChanges":true,"allowMaintenance":true}}"#,
        ),
    );
    assert_rejected(
        &connection,
        "CREATE (:Blocked) FINISH",
        options(r#"{"readOnlyNoExternalIO":true,"writeAccess":{"allowCreate":true}}"#),
        QueryErrorKind::InvalidArgument,
    );
    let head = branch_head(&connection, "main").expect("head");
    assert_rejected(
        &connection,
        "CREATE (:Blocked) FINISH",
        options(&format!(
            r#"{{"at":"commit/{}","writeAccess":{{"allowCreate":true}}}}"#,
            head.to_hex()
        )),
        QueryErrorKind::ReadOnlySnapshot,
    );
}
