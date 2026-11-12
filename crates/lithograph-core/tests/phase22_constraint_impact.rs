use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, prepare};
use lithograph_core::storage::{self, CommitMetadata, LayerBuilder, OwnerKind, PropertyValue};
use rusqlite::Connection;

fn empty_graph() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY, magic TEXT, database_id TEXT, storage_format INTEGER);").unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    db
}

fn execute_with_options(
    db: &Connection,
    text: &str,
    options: ExecutionOptions,
) -> Result<Vec<Vec<Value>>, QueryError> {
    let prepared = prepare(db, text, BTreeMap::new(), options)?;
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    let result = loop {
        match cursor.next_batch(db, 32) {
            Ok(batch) => {
                rows.extend(batch.rows);
                if batch.done {
                    break cursor.complete(db).map(|_| rows);
                }
            }
            Err(error) => break Err(error),
        }
    };
    drop(cursor);
    assert_eq!(resources.used(), 0, "{text}");
    result
}

fn execute(db: &Connection, text: &str) -> Vec<Vec<Value>> {
    execute_with_options(db, text, ExecutionOptions::default()).expect(text)
}

fn constrained_graph() -> Connection {
    let db = empty_graph();
    execute(
        &db,
        "CREATE (a:Common {typed:1,u:1,scope:'a',key:1,required:1,hit:0}), (b:Common {typed:2,u:2,scope:'a',key:2,required:2,hit:0}), (a)-[:REL {typed:1,u:1,scope:'a',key:1,required:1,hit:0}]->(b), (a)-[:REL {typed:2,u:2,scope:'a',key:2,required:2,hit:0}]->(b)",
    );
    for (prefix, target, key_kind) in [
        ("node", "(x:Common)", "NODE"),
        ("rel", "()-[x:REL]-()", "RELATIONSHIP"),
    ] {
        for (name, requirement) in [
            ("typed", "x.typed IS :: INTEGER".to_owned()),
            ("unique", "x.u IS UNIQUE".to_owned()),
            ("key", format!("(x.scope,x.key) IS {key_kind} KEY")),
            ("required", "x.required IS NOT NULL".to_owned()),
        ] {
            execute(
                &db,
                &format!("CREATE CONSTRAINT {prefix}_{name} FOR {target} REQUIRE {requirement}"),
            );
        }
    }
    db
}

fn rejects_without_commit(db: &Connection, text: &str) {
    let before = storage::branch_head(db, "main").unwrap();
    let error = execute_with_options(db, text, ExecutionOptions::default()).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Constraint, "{text}: {error:?}");
    assert_eq!(storage::branch_head(db, "main").unwrap(), before, "{text}");
}

fn staged_write_peak(db: &Connection, text: &str) -> u64 {
    let prepared = prepare(db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let batch = cursor.next_batch(db, 1).unwrap();
    assert_eq!(batch.rows.len(), 1);
    drop(batch);
    let peak = resources.peak();
    cursor.cancel(db).unwrap();
    drop(cursor);
    assert_eq!(resources.used(), 0);
    peak
}

#[test]
fn unrelated_node_and_relationship_properties_preserve_all_constraint_kinds() {
    let db = constrained_graph();
    for (pattern, identity) in [("(x:Common)", "n:1"), ("()-[x:REL]->()", "r:1")] {
        let select = format!("MATCH {pattern} WHERE elementId(x)='{identity}'");
        assert_eq!(
            execute(&db, &format!("{select} SET x.hit=7 RETURN x.hit")),
            vec![vec![Value::Integer(7)]]
        );
        for mutation in [
            "SET x.typed='wrong'",
            "SET x.u=2",
            "SET x.key=2",
            "REMOVE x.key",
            "REMOVE x.scope",
            "REMOVE x.required",
            "SET x.key=null",
            "SET x.required=null",
        ] {
            rejects_without_commit(&db, &format!("{select} {mutation} FINISH"));
        }
        execute(&db, &format!("{select} REMOVE x.typed, x.u FINISH"));
        assert_eq!(
            execute(
                &db,
                &format!("{select} RETURN x.typed,x.u,x.key,x.scope,x.required,x.hit")
            ),
            vec![vec![
                Value::Null,
                Value::Null,
                Value::Integer(1),
                Value::String("a".to_owned()),
                Value::Integer(1),
                Value::Integer(7)
            ]]
        );
        rejects_without_commit(
            &db,
            &format!("{select} SET x.hit=99, x.typed='wrong' FINISH"),
        );
        assert_eq!(
            execute(&db, &format!("{select} RETURN x.hit")),
            vec![vec![Value::Integer(7)]]
        );
        execute(&db, &format!("{select} SET x.scope='b',x.key=2 FINISH"));
        rejects_without_commit(&db, &format!("{select} SET x.scope='a' FINISH"));
    }
}

#[test]
fn membership_changes_validate_preexisting_values_and_new_relationships() {
    let db = constrained_graph();
    // Only hit changes in the same Layer; the newly covered values existed in
    // an unconstrained domain before this label membership change.
    for properties in [
        "typed:'wrong',u:3,scope:'a',key:3,required:3",
        "typed:3,u:1,scope:'a',key:3,required:3",
        "typed:3,u:3,scope:'a',key:1,required:3",
        "typed:3,u:3,scope:'a',key:3",
        "hit:1",
    ] {
        execute(&db, &format!("CREATE (:Candidate {{{properties}}}) FINISH"));
        rejects_without_commit(&db, "MATCH (x:Candidate) SET x:Common, x.hit=1 FINISH");
        execute(&db, "MATCH (x:Candidate) DELETE x FINISH");
    }
    for properties in [
        "typed:'wrong',u:3,scope:'a',key:3,required:3",
        "typed:3,u:1,scope:'a',key:3,required:3",
        "typed:3,u:3,scope:'a',key:1,required:3",
        "typed:3,u:3,scope:'a',key:3",
        "hit:1",
    ] {
        rejects_without_commit(
            &db,
            &format!(
                "MATCH (a:Common),(b:Common) WHERE elementId(a)='n:1' AND elementId(b)='n:2' CREATE (a)-[:REL {{{properties}}}]->(b) SET a.hit=1 FINISH"
            ),
        );
    }
    rejects_without_commit(&db, "CREATE (:Common {hit:1}) FINISH");
    assert_eq!(
        execute(&db, "MATCH (x:Common) RETURN count(x)"),
        vec![vec![Value::Integer(2)]]
    );
    assert_eq!(
        execute(&db, "MATCH ()-[x:REL]->() RETURN count(x)"),
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn graph_type_validation_is_retained_for_property_only_writes() {
    let db = empty_graph();
    execute(
        &db,
        "ALTER CURRENT GRAPH TYPE SET { (:Typed => {value :: INTEGER NOT NULL}), ()-[:TYPED => {value :: INTEGER NOT NULL}]->() }",
    );
    execute(
        &db,
        "CREATE (a:Typed {value:1}), (b), (a)-[:TYPED {value:1}]->(b)",
    );
    for pattern in ["(x:Typed)", "()-[x:TYPED]->()"] {
        rejects_without_commit(&db, &format!("MATCH {pattern} SET x.value='wrong' FINISH"));
        rejects_without_commit(&db, &format!("MATCH {pattern} REMOVE x.value FINISH"));
        // The declared property rules remain enforced while Graph Type keeps
        // its existing open-schema semantics for undeclared properties.
        execute(&db, &format!("MATCH {pattern} SET x.extra=1 FINISH"));
    }
}

#[test]
fn cancelled_and_resource_limited_writes_leave_no_commit_or_reservations() {
    let db = constrained_graph();
    let before = storage::branch_head(&db, "main").unwrap();
    for mutation in ["SET x.hit=5", "SET x.typed=5"] {
        let text = format!("MATCH (x:Common) WHERE elementId(x)='n:1' {mutation} RETURN x.hit");
        let prepared = prepare(&db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        let batch = cursor.next_batch(&db, 1).unwrap();
        assert_eq!(batch.rows.len(), 1);
        drop(batch);
        assert_eq!(
            cursor
                .next_batch_with_interrupt(&db, 1, &|| true)
                .unwrap_err()
                .kind,
            QueryErrorKind::Interrupted
        );
        drop(cursor);
        assert_eq!(resources.used(), 0);
        assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
        let options = ExecutionOptions::parse_text(r#"{"memoryLimitBytes":1}"#).unwrap();
        let error = execute_with_options(&db, &text, options).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
    }
    // Stage a constrained-property mutation first, then consume the remaining
    // shared budget with a lease before its validation/finalization boundary.
    let text = "MATCH (x:Common) WHERE elementId(x)='n:1' SET x.typed=5 RETURN x.typed";
    let limit = staged_write_peak(&db, text);
    let options =
        ExecutionOptions::parse_text(&format!(r#"{{"memoryLimitBytes":{limit}}}"#)).unwrap();
    let prepared = prepare(&db, text, BTreeMap::new(), options).unwrap();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let batch = cursor.next_batch(&db, 1).unwrap();
    assert_eq!(batch.rows.len(), 1);
    drop(batch);
    let pressure = resources.reserve(limit - resources.used()).unwrap();
    let error = cursor.next_batch(&db, 32).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    drop(pressure);
    drop(cursor);
    assert_eq!(resources.used(), 0);
    assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
    assert_eq!(
        execute(
            &db,
            "MATCH (x:Common) WHERE elementId(x)='n:1' RETURN x.typed,x.hit"
        ),
        vec![vec![Value::Integer(1), Value::Integer(0)]]
    );
}

fn work_fixture(count: i64) -> Connection {
    let db = empty_graph();
    let label = storage::intern_label(&db, "Common").unwrap();
    let value = storage::intern_property_key(&db, "v").unwrap();
    let hit = storage::intern_property_key(&db, "hit").unwrap();
    let mut layer = LayerBuilder::default();
    for index in 0..count {
        let id = storage::allocate_node_id(&db).unwrap();
        layer.add_node(id).unwrap();
        layer.add_label(id, label).unwrap();
        layer
            .set_property(OwnerKind::Node, id, value, PropertyValue::Integer(index))
            .unwrap();
        layer
            .set_property(OwnerKind::Node, id, hit, PropertyValue::Integer(0))
            .unwrap();
    }
    storage::commit_layer(
        &db,
        "main",
        storage::branch_head(&db, "main").unwrap(),
        None,
        &layer,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 1,
        },
    )
    .unwrap();
    execute(
        &db,
        "CREATE CONSTRAINT common_value FOR (n:Common) REQUIRE n.v IS :: INTEGER",
    );
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    db
}

fn sqlite_work(db: &Connection, text: &str) -> u64 {
    let callbacks = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&callbacks);
    db.progress_handler(
        1,
        Some(move || {
            observed.fetch_add(1, Ordering::Relaxed);
            false
        }),
    )
    .unwrap();
    execute(db, text);
    db.progress_handler(0, None::<fn() -> bool>).unwrap();
    callbacks.load(Ordering::Relaxed)
}

#[test]
fn unrelated_property_write_avoids_constraint_domain_sqlite_vm_work() {
    let mut measurements = Vec::new();
    for count in [1_000, 10_000] {
        let db = work_fixture(count);
        let unrelated = sqlite_work(
            &db,
            "MATCH (n:Common) WHERE elementId(n)='n:1' SET n.hit=1 FINISH",
        );
        let relevant = sqlite_work(
            &db,
            "MATCH (n:Common) WHERE elementId(n)='n:1' SET n.v=-1 FINISH",
        );
        eprintln!(
            "constraint impact owners={count} unrelated_vm={unrelated} relevant_vm={relevant}"
        );
        assert!(relevant > unrelated * 5, "{count}: {unrelated}/{relevant}");
        assert_eq!(
            execute(
                &db,
                "MATCH (n:Common) WHERE elementId(n)='n:1' RETURN n.hit,n.v"
            ),
            vec![vec![Value::Integer(1), Value::Integer(-1)]]
        );
        measurements.push((unrelated, relevant));
    }
    assert!(
        measurements[1].0 < measurements[0].0 * 2,
        "{measurements:?}"
    );
    assert!(
        measurements[1].1 > measurements[0].1 * 5,
        "{measurements:?}"
    );
}
