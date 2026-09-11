use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryErrorKind, QuerySummary, QueryType, prepare,
};
use lithograph_core::storage::{
    CommitMetadata, LayerBuilder, OwnerKind, PropertyValue, RelationshipRecord, Snapshot,
    allocate_node_id, allocate_relationship_id, branch_head, commit_layer, create_storage_schema,
    initialize_root, intern_label, intern_property_key, intern_relationship_type,
};
use rusqlite::Connection;

#[path = "phase05_mutation/execution.rs"]
mod execution;
#[path = "phase05_mutation/regressions.rs"]
mod regressions;
#[path = "phase05_mutation/value_roundtrip.rs"]
mod value_roundtrip;
#[path = "phase05_mutation/write_options.rs"]
mod write_options;

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite must open");
    initialize_storage(&connection);
    connection
}

fn initialize_storage(connection: &Connection) {
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1),magic TEXT NOT NULL,database_id TEXT NOT NULL,storage_format INTEGER NOT NULL);",
        )
        .expect("metadata table");
    create_storage_schema(connection).expect("storage schema");
    initialize_root(connection).expect("root");
}

fn fresh_file_storage() -> (Connection, std::path::PathBuf) {
    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);
    let path = std::env::temp_dir().join(format!(
        "lithograph-phase05-{}-{}.db",
        std::process::id(),
        NEXT_DATABASE.fetch_add(1, Ordering::Relaxed)
    ));
    let connection = Connection::open(&path).expect("file SQLite must open");
    initialize_storage(&connection);
    (connection, path)
}

fn metadata(message: &str, committed_at: i64) -> CommitMetadata {
    CommitMetadata {
        author: Some("phase05-test".to_owned()),
        message: Some(message.to_owned()),
        committed_at,
    }
}

fn execute(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), lithograph_core::query::QueryError> {
    execute_with_params(connection, query, BTreeMap::new(), options)
}

fn execute_with_params(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), lithograph_core::query::QueryError> {
    let prepared = prepare(connection, query, params, options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 2)?;
        rows.extend(batch.rows);
        if batch.done {
            let summary = cursor.complete(connection)?;
            return Ok((rows, summary));
        }
    }
}

fn read_rows(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    execute(connection, query, ExecutionOptions::default())
        .expect("read query")
        .0
}

#[test]
fn create_commits_once_and_is_immediately_queryable() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before");
    let (rows, summary) = execute(
        &connection,
        "CREATE (n:Person {name: 'Alice', age: 41}) RETURN n.name, n.age",
        ExecutionOptions::default(),
    )
    .expect("CREATE");
    let after = branch_head(&connection, "main").expect("head after");

    assert_ne!(before, after);
    assert_eq!(summary.query_type, QueryType::Write);
    assert_eq!(summary.commit, format!("commit/{}", after.to_hex()));
    assert_eq!(summary.counters.nodes_created, 1);
    assert_eq!(summary.counters.labels_added, 1);
    assert_eq!(summary.counters.properties_set, 2);
    assert_eq!(
        rows,
        vec![vec![Value::String("Alice".to_owned()), Value::Integer(41)]]
    );
    assert_eq!(
        read_rows(&connection, "MATCH (n:Person) RETURN n.name, n.age"),
        rows
    );

    let old = Snapshot::resolve(&connection, before).expect("old snapshot");
    assert!(
        old.scan_nodes_after(0, 10)
            .expect("old nodes")
            .items
            .is_empty()
    );
}

#[test]
fn insert_uses_the_versioned_write_path() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before");
    let (rows, summary) = execute(
        &connection,
        "INSERT (n:Person {name:'Inserted'}) RETURN n.name",
        ExecutionOptions::default(),
    )
    .expect("INSERT");
    let after = branch_head(&connection, "main").expect("head after");

    assert_ne!(before, after);
    assert_eq!(summary.commit, format!("commit/{}", after.to_hex()));
    assert_eq!(summary.counters.nodes_created, 1);
    assert_eq!(rows, vec![vec![Value::String("Inserted".to_owned())]]);
}

#[test]
fn repeated_labels_in_one_item_have_set_semantics() {
    let connection = fresh_storage();
    let (_, added) = execute(
        &connection,
        "CREATE (n:Once:Once) SET n:Twice:Twice FINISH",
        ExecutionOptions::default(),
    )
    .expect("repeated labels");
    assert_eq!(added.counters.labels_added, 2);

    let (_, removed) = execute(
        &connection,
        "MATCH (n:Once) REMOVE n:Twice:Twice FINISH",
        ExecutionOptions::default(),
    )
    .expect("remove repeated labels");
    assert_eq!(removed.counters.labels_removed, 1);
    assert_eq!(
        read_rows(&connection, "MATCH (n:Once) RETURN count(n)"),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        read_rows(&connection, "MATCH (n:Twice) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn no_op_mutation_still_creates_one_logical_commit() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (n:Person {name: 'Alice'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed");
    let before = branch_head(&connection, "main").expect("head before");
    let (_, summary) = execute(
        &connection,
        "MATCH (n:Person) WHERE n.name = 'Alice' SET n.name = 'Alice' FINISH",
        ExecutionOptions::default(),
    )
    .expect("no-op SET");
    let after = branch_head(&connection, "main").expect("head after");
    assert_ne!(before, after);
    assert_eq!(summary.counters.properties_set, 0);
}

#[test]
fn mutation_counters_report_net_delta_for_changes_that_cancel_within_the_query() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Seed {value:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed net-delta query");
    let before = branch_head(&connection, "main").expect("head before net-zero mutation");

    let (_, summary) = execute(
        &connection,
        "MATCH (seed:Seed) SET seed.value = 2 SET seed.value = 1 CREATE (n:Temporary {value:3}) DELETE n FINISH",
        ExecutionOptions::default(),
    )
    .expect("net-zero mutation");
    let after = branch_head(&connection, "main").expect("head after net-zero mutation");

    assert_ne!(
        after, before,
        "successful no-op mutation still creates a Commit"
    );
    assert_eq!(summary.counters, Default::default());
    assert_eq!(
        read_rows(&connection, "MATCH (seed:Seed) RETURN seed.value"),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        read_rows(&connection, "MATCH (n:Temporary) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn replacing_properties_reports_both_removed_and_set_counters() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Person {name:'before', keep:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed properties");

    let (_, direct) = execute(
        &connection,
        "MATCH (n:Person) SET n.name = 'after' FINISH",
        ExecutionOptions::default(),
    )
    .expect("replace one property");
    assert_eq!(direct.counters.properties_set, 1);
    assert_eq!(direct.counters.properties_removed, 1);

    let (_, map) = execute(
        &connection,
        "MATCH (n:Person) SET n = {name:'final', added:2} FINISH",
        ExecutionOptions::default(),
    )
    .expect("replace property map");
    assert_eq!(map.counters.properties_set, 2);
    assert_eq!(map.counters.properties_removed, 2);
}

#[test]
fn graph_view_is_checked_at_each_mutating_clause_boundary() {
    let connection = fresh_storage();
    let options = ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["A"]}}"#)
        .expect("view options");
    let before = branch_head(&connection, "main").expect("head");
    let error = execute(&connection, "CREATE (n) SET n:A FINISH", options.clone())
        .expect_err("CREATE must fail before later SET can repair view membership");
    assert_eq!(error.kind, QueryErrorKind::GraphViewViolation);
    assert_eq!(
        branch_head(&connection, "main").expect("head unchanged"),
        before
    );

    let (rows, _) = execute(
        &connection,
        "CREATE (n:A) MATCH (m:A) RETURN count(m)",
        options.clone(),
    )
    .expect("visible CREATE followed by staged read");
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    let head = branch_head(&connection, "main").expect("visible head");
    let error = execute(&connection, "MATCH (n:A) REMOVE n:A FINISH", options)
        .expect_err("REMOVE required label");
    assert_eq!(error.kind, QueryErrorKind::GraphViewViolation);
    assert_eq!(
        branch_head(&connection, "main").expect("head rollback"),
        head
    );

    let excluded = ExecutionOptions::parse_text(r#"{"graphView":{"excludeAnyLabels":["Hidden"]}}"#)
        .expect("excluded-label view options");
    let error = execute(&connection, "MATCH (n:A) SET n:Hidden FINISH", excluded)
        .expect_err("SET must not leave a touched Node outside the active view");
    assert_eq!(error.kind, QueryErrorKind::GraphViewViolation);
    assert_eq!(
        read_rows(&connection, "MATCH (n:Hidden) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn graph_view_hides_existing_elements_from_match_and_merge_mutation_targets() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:TenantData {key:1, untouched:true}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed hidden element");
    let options = ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#)
        .expect("required-label view options");

    let (_, no_match) = execute(
        &connection,
        "MATCH (n:TenantData) SET n.untouched = false REMOVE n.untouched DELETE n FINISH",
        options.clone(),
    )
    .expect("hidden MATCH produces no mutation targets");
    assert_eq!(no_match.counters, Default::default());

    let (_, merged) = execute(
        &connection,
        "MERGE (n:TenantData {key:1}) ON CREATE SET n:Visible ON MATCH SET n.untouched = false FINISH",
        options,
    )
    .expect("MERGE creates inside the view instead of matching the hidden Node");
    assert_eq!(merged.counters.nodes_created, 1);
    assert_eq!(merged.counters.properties_removed, 0);
    assert_eq!(
        read_rows(&connection, "MATCH (n:TenantData) RETURN count(n)"),
        vec![vec![Value::Integer(2)]]
    );
    assert_eq!(
        read_rows(
            &connection,
            "MATCH (n:TenantData) WHERE n.untouched = true RETURN count(n)"
        ),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        read_rows(
            &connection,
            "MATCH (n:TenantData) WHERE n.untouched = false RETURN count(n)"
        ),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn graph_view_rejects_relationship_with_an_invisible_endpoint() {
    let connection = fresh_storage();
    let options = ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["A"]}}"#)
        .expect("view options");
    let before = branch_head(&connection, "main").expect("head before relationship CREATE");

    let error = execute(&connection, "CREATE (:A)-[:LINK]->() FINISH", options)
        .expect_err("both Relationship endpoints must remain visible in the Graph View");

    assert_eq!(error.kind, QueryErrorKind::GraphViewViolation);
    assert_eq!(
        branch_head(&connection, "main").expect("head after rejected relationship CREATE"),
        before
    );
    assert!(
        lithograph_core::storage::find_relationship_type(&connection, "LINK")
            .expect("find rejected relationship type")
            .is_none()
    );
}

#[test]
fn merge_on_create_effects_complete_before_graph_view_boundary() {
    let connection = fresh_storage();
    let options = ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["A"]}}"#)
        .expect("view options");

    let (_, summary) = execute(
        &connection,
        "MERGE (n) ON CREATE SET n:A FINISH",
        options.clone(),
    )
    .expect("MERGE ON CREATE may establish final view membership");
    assert_eq!(summary.counters.nodes_created, 1);
    assert_eq!(summary.counters.labels_added, 1);

    let rows = execute(&connection, "MATCH (n:A) RETURN count(n)", options)
        .expect("view read")
        .0;
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn null_mutation_targets_are_no_ops() {
    let connection = fresh_storage();
    let (rows, summary) = execute(
        &connection,
        "OPTIONAL MATCH (a:DoesNotExist) SET a.num = 42 SET a:L REMOVE a.num REMOVE a:L RETURN a",
        ExecutionOptions::default(),
    )
    .expect("null mutation targets");

    assert_eq!(rows, vec![vec![Value::Null]]);
    assert_eq!(summary.counters.properties_set, 0);
    assert_eq!(summary.counters.properties_removed, 0);
    assert_eq!(summary.counters.labels_added, 0);
    assert_eq!(summary.counters.labels_removed, 0);
}

#[test]
fn set_remove_and_detach_delete_produce_canonical_graph_delta() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (a:Person {name:'A'})-[:KNOWS {since: 2020}]->(b:Person {name:'B'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed path");
    let (_, summary) = execute(
        &connection,
        "MATCH (a:Person)-[r:KNOWS]->(b:Person) WHERE a.name = 'A' AND b.name = 'B' SET a.age = 1 REMOVE r.since SET r.note = 'x' FINISH",
        ExecutionOptions::default(),
    )
    .expect("set/remove");
    assert_eq!(summary.counters.properties_set, 2);
    assert_eq!(summary.counters.properties_removed, 1);

    let (_, summary) = execute(
        &connection,
        "MATCH (a:Person) WHERE a.name = 'A' DETACH DELETE a FINISH",
        ExecutionOptions::default(),
    )
    .expect("detach delete");
    assert_eq!(summary.counters.nodes_deleted, 1);
    assert_eq!(summary.counters.relationships_deleted, 1);
    assert_eq!(
        read_rows(&connection, "MATCH (n:Person) RETURN n.name"),
        vec![vec![Value::String("B".to_owned())]]
    );
}

#[test]
fn deleting_a_connected_node_without_detach_is_atomic_constraint_failure() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Person {name:'A'})-[:KNOWS]->(:Person {name:'B'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed connected nodes");
    let before = branch_head(&connection, "main").expect("head before rejected delete");

    let error = execute(
        &connection,
        "MATCH (n:Person) WHERE n.name = 'A' DELETE n FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("plain DELETE must reject a connected Node");

    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&connection, "main").expect("head after rejected delete"),
        before
    );
    assert_eq!(
        read_rows(&connection, "MATCH (n:Person) RETURN count(n)"),
        vec![vec![Value::Integer(2)]]
    );
    assert_eq!(
        read_rows(&connection, "MATCH ()-[r:KNOWS]->() RETURN count(r)"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn merge_matches_or_creates_without_duplicate_nodes() {
    let connection = fresh_storage();
    let (_, created) = execute(
        &connection,
        "MERGE (n:Person {name:'Alice'}) ON CREATE SET n.created = true RETURN n.name",
        ExecutionOptions::default(),
    )
    .expect("merge create");
    assert_eq!(created.counters.nodes_created, 1);

    let (_, matched) = execute(
        &connection,
        "MERGE (n:Person {name:'Alice'}) ON MATCH SET n.seen = true RETURN n.name",
        ExecutionOptions::default(),
    )
    .expect("merge match");
    assert_eq!(matched.counters.nodes_created, 0);
    assert_eq!(matched.counters.properties_set, 1);
    assert_eq!(
        read_rows(&connection, "MATCH (n:Person) RETURN count(n)"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn caller_transaction_rollback_removes_commit_chain_and_dictionary_allocations() {
    let connection = fresh_storage();
    let root = branch_head(&connection, "main").expect("root");
    connection
        .execute_batch("BEGIN")
        .expect("outer transaction");
    execute(
        &connection,
        "CREATE (n:Transient {temporary: 'yes'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("nested write");
    assert_ne!(branch_head(&connection, "main").expect("inside head"), root);
    connection
        .execute_batch("ROLLBACK")
        .expect("outer rollback");
    assert_eq!(branch_head(&connection, "main").expect("rolled head"), root);
    assert!(
        lithograph_core::storage::find_label(&connection, "Transient")
            .expect("find label")
            .is_none()
    );
    assert!(
        lithograph_core::storage::find_property_key(&connection, "temporary")
            .expect("find property")
            .is_none()
    );
}

#[test]
fn caller_transaction_commit_publishes_the_complete_commit_chain_atomically() {
    let (writer, path) = fresh_file_storage();
    let reader = Connection::open(&path).expect("open reader connection");
    let root = branch_head(&reader, "main").expect("reader root");

    writer
        .execute_batch("BEGIN")
        .expect("begin writer transaction");
    execute(
        &writer,
        "CREATE (:Published {ordinal:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("first pending write");
    execute(
        &writer,
        "CREATE (:Published {ordinal:2}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("second pending write");
    let pending_head = branch_head(&writer, "main").expect("writer pending head");

    assert_eq!(branch_head(&reader, "main").expect("reader old head"), root);
    assert_eq!(
        read_rows(&reader, "MATCH (n:Published) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );

    writer
        .execute_batch("COMMIT")
        .expect("commit writer transaction");
    assert_eq!(
        branch_head(&reader, "main").expect("reader published head"),
        pending_head
    );
    assert_eq!(
        read_rows(&reader, "MATCH (n:Published) RETURN count(n)"),
        vec![vec![Value::Integer(2)]]
    );

    drop(reader);
    drop(writer);
    std::fs::remove_file(path).expect("remove committed file database");
}

#[test]
fn detach_delete_cannot_cross_hidden_relationship_boundary() {
    let connection = fresh_storage();
    let root = branch_head(&connection, "main").expect("root");
    let visible = intern_label(&connection, "Visible").expect("Visible");
    let hidden = intern_label(&connection, "Hidden").expect("Hidden");
    let rel_type = intern_relationship_type(&connection, "LINK").expect("LINK");
    let key = intern_property_key(&connection, "name").expect("name");
    let a = allocate_node_id(&connection).expect("a");
    let b = allocate_node_id(&connection).expect("b");
    let relationship_id = allocate_relationship_id(&connection).expect("relationship");
    let mut layer = LayerBuilder::default();
    layer.add_node(a).expect("a node");
    layer.add_node(b).expect("b node");
    layer.add_label(a, visible).expect("visible label");
    layer.add_label(b, hidden).expect("hidden label");
    layer
        .set_property(OwnerKind::Node, a, key, PropertyValue::String("A".into()))
        .expect("a name");
    layer
        .add_relationship(RelationshipRecord {
            id: relationship_id,
            source: a,
            type_id: rel_type,
            target: b,
        })
        .expect("relationship");
    commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("seed", 1),
    )
    .expect("seed commit");
    let before = branch_head(&connection, "main").expect("before delete");
    let options = ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#)
        .expect("view");
    let error = execute(
        &connection,
        "MATCH (n:Visible) WHERE n.name = 'A' DETACH DELETE n FINISH",
        options,
    )
    .expect_err("hidden edge must block DETACH DELETE");
    assert_eq!(error.kind, QueryErrorKind::GraphViewViolation);
    assert_eq!(
        branch_head(&connection, "main").expect("rollback head"),
        before
    );
}

#[test]
fn repeated_delete_targets_from_multiple_rows_are_applied_once() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (a)-[:R]->(b) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed relationship");

    let (rows, summary) = execute(
        &connection,
        "MATCH (a)-[r]-(b) DELETE r, a, b RETURN count(*) AS c",
        ExecutionOptions::default(),
    )
    .expect("duplicate delete targets from undirected expansion");

    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    assert_eq!(summary.counters.relationships_deleted, 1);
    assert_eq!(summary.counters.nodes_deleted, 2);
    assert_eq!(
        read_rows(&connection, "MATCH (n) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn delete_clause_removes_explicit_relationships_before_nodes() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (a:A)-[:R]->(b:B) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed relationship");

    let (_, summary) = execute(
        &connection,
        "MATCH (a:A)-[r:R]->(b:B) DELETE a, r FINISH",
        ExecutionOptions::default(),
    )
    .expect("DELETE arguments are one clause effect, independent of argument order");

    assert_eq!(summary.counters.relationships_deleted, 1);
    assert_eq!(summary.counters.nodes_deleted, 1);
    assert_eq!(
        read_rows(&connection, "MATCH (n) RETURN count(n)"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn delete_does_not_downgrade_property_expressions_to_their_owner_variable() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Kept {value:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed Node");
    let before = branch_head(&connection, "main").expect("head before invalid DELETE");

    let error = execute(
        &connection,
        "MATCH (n:Kept) DELETE n.value FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("Phase 05 only executes direct graph-variable DELETE targets");

    assert_eq!(error.kind, QueryErrorKind::Semantic);
    assert_eq!(
        branch_head(&connection, "main").expect("unchanged head"),
        before
    );
    assert_eq!(
        read_rows(&connection, "MATCH (n:Kept) RETURN count(n)"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn merge_relationship_matching_resets_prior_clause_uniqueness_state() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:A)-[:R]->(:B) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed relationship");

    let (rows, summary) = execute(
        &connection,
        "MATCH (a:A)-[:R]->(b:B) MERGE (a)-[:R]->(b) FINISH",
        ExecutionOptions::default(),
    )
    .expect("MERGE must be able to match a Relationship read by a prior clause");

    assert_eq!(summary.counters.relationships_created, 0);
    assert!(rows.is_empty());
    assert_eq!(
        read_rows(&connection, "MATCH ()-[r:R]->() RETURN count(r)"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn merge_rejects_null_pattern_properties_without_writing() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before MERGE");

    let error = execute(
        &connection,
        "MERGE (:Invalid {value:null}) FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("MERGE null property");

    assert_eq!(error.kind, QueryErrorKind::Semantic);
    assert_eq!(
        branch_head(&connection, "main").expect("unchanged head"),
        before
    );
    assert!(
        lithograph_core::storage::find_label(&connection, "Invalid")
            .expect("find rolled-back label")
            .is_none()
    );
    assert!(
        lithograph_core::storage::find_property_key(&connection, "value")
            .expect("find rolled-back property key")
            .is_none()
    );
}

#[test]
fn property_delta_uses_canonical_float_state_equality() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:FloatState {zero:-0.0, nan:0.0 / 0.0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed floating properties");

    let (_, summary) = execute(
        &connection,
        "MATCH (n:FloatState) SET n.zero = 0.0, n.nan = 0.0 / 0.0 FINISH",
        ExecutionOptions::default(),
    )
    .expect("update canonical floating states");
    assert_eq!(summary.counters.properties_set, 1);
    assert_eq!(summary.counters.properties_removed, 1);

    let head = branch_head(&connection, "main").expect("updated head");
    let changed_key: String = connection
        .query_row(
            "SELECT key.name FROM main._lithograph_commits AS commit_row JOIN main._lithograph_property_delta AS property ON property.layer_id = commit_row.layer_id JOIN main._lithograph_prop_keys AS key ON key.id = property.key_id WHERE commit_row.id = ?1",
            [head.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("one canonical property delta");
    assert_eq!(changed_key, "zero");

    let rows = read_rows(&connection, "MATCH (n:FloatState) RETURN n.zero, n.nan");
    let Value::Float(zero) = rows[0][0] else {
        panic!("zero property must remain Float");
    };
    let Value::Float(nan) = rows[0][1] else {
        panic!("nan property must remain Float");
    };
    assert_eq!(zero.to_bits(), 0.0_f64.to_bits());
    assert!(nan.is_nan());
}

#[test]
fn removing_an_unknown_property_does_not_create_dictionary_state() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Target) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed target");

    let (_, summary) = execute(
        &connection,
        "MATCH (n:Target) SET n.never = null SET n += {alsoNever:null} FINISH",
        ExecutionOptions::default(),
    )
    .expect("null removals are no-ops");

    assert_eq!(summary.counters.properties_removed, 0);
    for key in ["never", "alsoNever"] {
        assert!(
            lithograph_core::storage::find_property_key(&connection, key)
                .expect("find no-op property key")
                .is_none()
        );
    }
}

#[test]
fn host_interrupt_during_mutation_rolls_back_staged_identity_and_dictionary_state() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before interrupt");
    let query = format!(
        "CREATE {} FINISH",
        std::iter::repeat_n("(:Interrupted)", 16)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let prepared = prepare(
        &connection,
        &query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare interruptible mutation");
    let checks = Cell::new(0_u32);
    let interrupted = || {
        let next = checks.get().saturating_add(1);
        checks.set(next);
        next >= 6
    };
    let mut cursor = QueryCursor::new(prepared);

    let error = cursor
        .next_batch_with_interrupt(&connection, 1, &interrupted)
        .expect_err("host interrupt must abort the staged write");

    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(
        branch_head(&connection, "main").expect("head after interrupt"),
        before
    );
    assert!(
        lithograph_core::storage::find_label(&connection, "Interrupted")
            .expect("find interrupted label")
            .is_none()
    );
}

#[test]
fn host_interrupt_during_large_merge_on_match_rolls_back_the_clause() {
    let connection = fresh_storage();
    let seed = format!(
        "CREATE {} FINISH",
        std::iter::repeat_n("(:MergeInterrupt)", 300)
            .collect::<Vec<_>>()
            .join(", ")
    );
    execute(&connection, &seed, ExecutionOptions::default()).expect("seed MERGE candidates");
    let before = branch_head(&connection, "main").expect("head before MERGE interrupt");
    let prepared = prepare(
        &connection,
        "MERGE (node:MergeInterrupt) ON MATCH SET node.touched = true FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare interruptible MERGE");
    let checks = Cell::new(0_u32);
    let interrupted = || {
        let next = checks.get().saturating_add(1);
        checks.set(next);
        next >= 700
    };
    let mut cursor = QueryCursor::new(prepared);

    let error = cursor
        .next_batch_with_interrupt(&connection, 1, &interrupted)
        .expect_err("host interrupt must stop a long ON MATCH application loop");

    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(
        branch_head(&connection, "main").expect("head after MERGE interrupt"),
        before
    );
    assert!(
        lithograph_core::storage::find_property_key(&connection, "touched")
            .expect("find interrupted MERGE property")
            .is_none()
    );
}

#[test]
fn host_interrupt_between_write_batches_rolls_back_the_active_write() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Seed), (:Seed), (:Seed) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed rows");
    let before = branch_head(&connection, "main").expect("head before batched write");
    let prepared = prepare(
        &connection,
        "MATCH (n:Seed) CREATE (:Batched) RETURN n",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare batched write");
    let mut cursor = QueryCursor::new(prepared);

    let first = cursor
        .next_batch_with_interrupt(&connection, 1, &|| false)
        .expect("first result batch");
    assert!(!first.done);
    let error = cursor
        .next_batch_with_interrupt(&connection, 1, &|| true)
        .expect_err("interrupt between batches must cancel the active savepoint");

    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(
        branch_head(&connection, "main").expect("head after batched interrupt"),
        before
    );
    assert!(
        lithograph_core::storage::find_label(&connection, "Batched")
            .expect("find interrupted label")
            .is_none()
    );
}

#[test]
fn host_interrupt_before_write_completion_rolls_back_the_active_write() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before completion interrupt");
    let prepared = prepare(
        &connection,
        "CREATE (:BeforeComplete) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare write");
    let mut cursor = QueryCursor::new(prepared);
    let batch = cursor
        .next_batch_with_interrupt(&connection, 1, &|| false)
        .expect("terminal batch before completion");
    assert!(batch.done);

    let error = cursor
        .complete_with_interrupt(&connection, &|| true)
        .expect_err("interrupt at completion boundary must cancel the active savepoint");

    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(
        branch_head(&connection, "main").expect("head after completion interrupt"),
        before
    );
    assert!(
        lithograph_core::storage::find_label(&connection, "BeforeComplete")
            .expect("find interrupted completion label")
            .is_none()
    );
}
