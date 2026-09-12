use super::*;

#[test]
fn composed_writes_commit_once_and_later_clauses_see_staged_state() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "UNWIND [1, 2] AS x CREATE (:N {value:x}) WITH count(*) AS created MATCH (n:N) RETURN created, count(n) AS visible",
        ),
        vec![vec![Value::Integer(2), Value::Integer(2)]]
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [3, 4] AS x CALL (x) { CREATE (:N {value:x}) WITH x MATCH (n:N) RETURN count(n) AS visible } RETURN x, visible ORDER BY x",
        ),
        vec![
            vec![Value::Integer(3), Value::Integer(3)],
            vec![Value::Integer(4), Value::Integer(4)],
        ]
    );

    assert_eq!(
        rows(
            &connection,
            "UNWIND [5, 6] AS x CALL (x) { WITH 0 AS ignored CREATE (:Scoped {value:x}) } RETURN x ORDER BY x",
        ),
        vec![vec![Value::Integer(5)], vec![Value::Integer(6)]]
    );
    assert_eq!(
        rows(
            &connection,
            "UNWIND [7, 8] AS x CALL { WITH x CREATE (:Legacy {value:x}) } RETURN x ORDER BY x",
        ),
        vec![vec![Value::Integer(7)], vec![Value::Integer(8)]]
    );
}

#[test]
fn simple_mutating_queries_use_complete_grouped_aggregation() {
    let connection = fresh_storage();
    let _ = rows(
        &connection,
        "CREATE (:Grouped {kind:'a'}), (:Grouped {kind:'a'}), (:Grouped {kind:'b'}) FINISH",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (node:Grouped) SET node.touched = true RETURN node.kind AS kind, count(*) AS total ORDER BY kind",
        ),
        vec![
            vec![Value::String("a".to_owned()), Value::Integer(2)],
            vec![Value::String("b".to_owned()), Value::Integer(1)],
        ]
    );
}

#[test]
fn foreach_updates_each_item_without_leaking_its_loop_variable() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "WITH [1, 2, 3] AS values FOREACH (x IN values | CREATE (:N {value:x})) MATCH (n:N) RETURN collect(n.value) AS values",
        ),
        vec![vec![Value::List(vec![
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(3),
        ])]]
    );
}

#[test]
fn dynamic_write_names_flow_through_create_set_remove_and_match() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "WITH ['A', 'B'] AS dynamicLabels, 'R' AS dynamicType CREATE (a:$(dynamicLabels))-[r:$(dynamicType)]->(b:Target) SET b:$('Added') REMOVE a:$('B') RETURN labels(a) AS leftLabels, type(r) AS relationshipType, labels(b) AS rightLabels",
        ),
        vec![vec![
            Value::List(vec![Value::String("A".to_owned())]),
            Value::String("R".to_owned()),
            Value::List(vec![
                Value::String("Target".to_owned()),
                Value::String("Added".to_owned()),
            ]),
        ]]
    );
    assert_eq!(
        rows(
            &connection,
            "WITH 'A' AS dynamicLabel MATCH (n:$(dynamicLabel)) RETURN count(n) AS nodes",
        ),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn create_and_insert_keep_their_distinct_label_contracts() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "CREATE (:Colon:Separated), (:AlsoColon) RETURN count(*)",
        ),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        rows(
            &connection,
            "INSERT (node:Inserted&Actor {name:'Ada'}) RETURN labels(node)",
        ),
        vec![vec![Value::List(vec![
            Value::String("Inserted".to_owned()),
            Value::String("Actor".to_owned()),
        ])]]
    );
    for query in [
        "INSERT (:A:B) FINISH",
        "INSERT (:$('Dynamic')) FINISH",
        "INSERT ()-[:$('DYNAMIC')]->() FINISH",
        "CREATE (:A:B&C) FINISH",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }
}

#[test]
fn dynamic_property_writes_resolve_per_row_and_fail_atomically() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "CREATE (node:DynamicProperty {kept: 1}) WITH node, ['score'] AS keys SET node[keys[0]] = 7 RETURN node.score",
        ),
        vec![vec![Value::Integer(7)]]
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (node:DynamicProperty) WITH node, 'score' AS key REMOVE node[key] RETURN node.score, node.kept",
        ),
        vec![vec![Value::Null, Value::Integer(1)]]
    );
    assert_eq!(
        rows(
            &connection,
            "CREATE ()-[relationship:DYNAMIC_PROPERTY]->() WITH relationship, 'weight' AS key SET relationship[key] = 3 REMOVE relationship[key] RETURN relationship.weight",
        ),
        vec![vec![Value::Null]]
    );

    assert!(
        query_error(
            &connection,
            "CREATE (node:RolledBackDynamicProperty) SET node[42] = 1 RETURN node"
        )
        .contains("dynamic property key")
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (node:RolledBackDynamicProperty) RETURN count(node)"
        ),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn nodetach_delete_is_the_constraint_preserving_delete_spelling() {
    let connection = fresh_storage();
    let _ = rows(
        &connection,
        "CREATE (free:Free), (parent:Parent)-[:OWNS]->(:Child) FINISH",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (free:Free) NODETACH DELETE free RETURN count(*)",
        ),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        rows(&connection, "MATCH (free:Free) RETURN count(free)"),
        vec![vec![Value::Integer(0)]]
    );
    let error = execution_error(
        &connection,
        "MATCH (parent:Parent) NODETACH DELETE parent FINISH",
    );
    assert!(
        error.contains("Relationships still reference it"),
        "{error}"
    );
    assert_eq!(
        rows(&connection, "MATCH (parent:Parent) RETURN count(parent)"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn delete_accepts_paths_and_preserves_external_relationship_integrity() {
    let connection = fresh_storage();
    let _ = rows(
        &connection,
        "CREATE (a:PathNode {name:'a'})-[:PATH_EDGE]->(b:PathNode {name:'b'}) FINISH",
    );
    let _ = rows(
        &connection,
        "MATCH p = (a:PathNode {name:'a'})-[:PATH_EDGE]->(b:PathNode {name:'b'}) DELETE b, head([p]) FINISH",
    );
    assert_eq!(
        rows(&connection, "MATCH (n:PathNode) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH ()-[relationship:PATH_EDGE]->() RETURN count(relationship)",
        ),
        vec![vec![Value::Integer(0)]]
    );

    let _ = rows(
        &connection,
        "CREATE (a:ExpressionDelete), (b:ExpressionDelete) WITH [a, b] AS targets DELETE head(targets), last(targets) FINISH",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (node:ExpressionDelete) RETURN count(node)"
        ),
        vec![vec![Value::Integer(0)]]
    );

    assert!(
        query_error(
            &connection,
            "CREATE (node:InvalidDelete) WITH node DELETE CASE WHEN true THEN 42 ELSE node END FINISH",
        )
        .contains("DELETE requires")
    );
    assert_eq!(
        rows(&connection, "MATCH (node:InvalidDelete) RETURN count(node)"),
        vec![vec![Value::Integer(0)]]
    );

    let _ = rows(
        &connection,
        "CREATE (a:Guarded {name:'a'})-[:IN_PATH]->(b:Guarded {name:'b'}), (b)-[:EXTERNAL]->(:Outside) FINISH",
    );
    let error = execution_error(
        &connection,
        "MATCH p = (a:Guarded {name:'a'})-[:IN_PATH]->(b:Guarded {name:'b'}) DELETE p FINISH",
    );
    assert!(
        error.contains("Relationships still reference it"),
        "{error}"
    );
    assert_eq!(
        rows(&connection, "MATCH (n:Guarded) RETURN count(n)"),
        vec![vec![Value::Integer(2)]]
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH ()-[relationship]->() RETURN count(relationship)",
        ),
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn merge_map_set_and_nested_foreach_are_multi_row_and_atomic() {
    let connection = fresh_storage();
    let _ = rows(
        &connection,
        "UNWIND [1, 1, 2] AS id MERGE (node:Merged {id:id}) ON CREATE SET node.hits = 1 ON MATCH SET node.hits = node.hits + 1 FINISH",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (node:Merged) RETURN node.id, node.hits ORDER BY node.id",
        ),
        vec![
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(2), Value::Integer(1)],
        ]
    );

    assert_eq!(
        rows(
            &connection,
            "CREATE (node:MapSet {a:1, b:2}) SET node += {b:null, c:3} RETURN properties(node)",
        ),
        vec![vec![Value::Map(BTreeMap::from([
            ("a".to_owned(), Value::Integer(1)),
            ("c".to_owned(), Value::Integer(3)),
        ]))]]
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (node:MapSet) SET node = {c:4} RETURN properties(node)",
        ),
        vec![vec![Value::Map(BTreeMap::from([(
            "c".to_owned(),
            Value::Integer(4),
        )]))]]
    );

    let _ = rows(
        &connection,
        "FOREACH (x IN [1, 2] | CREATE (:OuterLoop {value:x}) FOREACH (y IN [1, 2] | CREATE (:InnerLoop {value:x * 10 + y}))) FINISH",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (node:InnerLoop) RETURN collect(node.value)",
        ),
        vec![vec![Value::List(vec![
            Value::Integer(11),
            Value::Integer(12),
            Value::Integer(21),
            Value::Integer(22),
        ])]]
    );

    let error = query_error(
        &connection,
        "FOREACH (x IN [1, 2] | CREATE (node:RolledBackLoop {value:x}) SET node[CASE x WHEN 2 THEN 42 ELSE 'ok' END] = x) FINISH",
    );
    assert!(error.contains("dynamic property key"), "{error}");
    assert_eq!(
        rows(
            &connection,
            "MATCH (node:RolledBackLoop) RETURN count(node)",
        ),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn transaction_subqueries_are_rejected_until_their_owning_phase() {
    let connection = fresh_storage();
    let error = prepare(
        &connection,
        "UNWIND [1] AS value CALL (value) { CREATE (:Deferred {value: value}) } IN TRANSACTIONS RETURN value",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect_err("Phase 06 must not silently execute transaction modifiers");
    assert!(error.message.contains("Phase 08 transaction executor"));
    assert_eq!(
        rows(&connection, "MATCH (node:Deferred) RETURN count(node)"),
        vec![vec![Value::Integer(0)]]
    );
}
