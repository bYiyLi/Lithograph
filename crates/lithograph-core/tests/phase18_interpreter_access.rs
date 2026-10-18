use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, prepare};
use lithograph_core::storage::{create_storage_schema, initialize_root};
use rusqlite::Connection;

fn fixture() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata table");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    execute(&connection, "CREATE (a:Person {name:'A', secret:11, audit_1:21}), (b:Person {name:'B', secret:12, audit_1:22}), (a)-[:KNOWS {name:'AB', secret:31}]->(b) FINISH", "{}").expect("graph fixture");
    connection
}

fn execute(
    connection: &Connection,
    query: &str,
    text: &str,
) -> Result<Vec<Vec<Value>>, QueryError> {
    let options = ExecutionOptions::parse_text(text)?;
    let prepared = prepare(connection, query, BTreeMap::new(), options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 1)?;
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection)?;
            return Ok(rows);
        }
    }
}

fn hidden() -> &'static str {
    r#"{"readAccess":{"query":{"denyProperties":["secret"],"denyPropertyPatterns":["audit_[0-9]+"]}}}"#
}

#[test]
fn interpreter_uses_hidden_values_for_grouping_subqueries_and_interpolation() {
    let connection = fixture();
    assert_eq!(execute(&connection,
        "MATCH (p:Person) CALL (p) { RETURN p.secret AS hidden, p['audit_1'] AS dynamic } RETURN hidden, dynamic, count(*) AS total ORDER BY hidden",
        hidden()).expect("grouped subquery"), vec![vec![Value::Null, Value::Null, Value::Integer(2)]]);
    assert_eq!(
        execute(
            &connection,
            "MATCH (p:Person) RETURN count(p.secret), collect(p.secret), sum(p.secret)",
            hidden()
        )
        .expect("hidden aggregate inputs"),
        vec![vec![
            Value::Integer(0),
            Value::List(Vec::new()),
            Value::Integer(0)
        ]]
    );
    assert_eq!(execute(&connection,
        "MATCH (p:Person) RETURN p.name, EXISTS { WITH p WHERE p.secret IS NOT NULL RETURN p }, COUNT { WITH p WHERE p.secret IS NOT NULL RETURN p }, s\"hidden={p.secret IS NULL}\" ORDER BY p.name",
        hidden()).expect("graph expressions"), vec![
            vec![Value::String("A".to_owned()), Value::Boolean(false), Value::Integer(0), Value::String("hidden=true".to_owned())],
            vec![Value::String("B".to_owned()), Value::Boolean(false), Value::Integer(0), Value::String("hidden=true".to_owned())],
        ]);
}

#[test]
fn path_predicates_and_grouped_relationships_never_observe_hidden_properties() {
    let connection = fixture();
    assert!(
        execute(
            &connection,
            "MATCH (a:Person {secret:11})-[:KNOWS]->(b) RETURN b.name",
            hidden()
        )
        .expect("hidden pattern map")
        .is_empty()
    );
    assert!(
        execute(
            &connection,
            "MATCH (a)-[r:KNOWS {secret:31}]->(b) RETURN b.name",
            hidden()
        )
        .expect("hidden relationship pattern")
        .is_empty()
    );
    let rows = execute(
        &connection,
        "MATCH p = (a:Person {name:'A'})-[r:KNOWS*1..2]->(b) RETURN p, r, [x IN r | x.secret]",
        hidden(),
    )
    .expect("grouped relationship path");
    assert_eq!(rows.len(), 1);
    let Value::Path(path) = &rows[0][0] else {
        panic!("Path result")
    };
    assert!(
        path.nodes
            .iter()
            .all(|node| !node.properties.contains_key("secret")
                && !node.properties.contains_key("audit_1"))
    );
    assert!(
        path.relationships
            .iter()
            .all(|relationship| !relationship.properties.contains_key("secret"))
    );
    let Value::List(relationships) = &rows[0][1] else {
        panic!("Relationship list")
    };
    assert!(relationships.iter().all(|relationship| matches!(relationship, Value::Relationship(value) if !value.properties.contains_key("secret"))));
    assert_eq!(rows[0][2], Value::List(vec![Value::Null]));
}

#[test]
fn graph_property_enumeration_is_filtered_but_ordinary_maps_keep_their_keys() {
    let connection = fixture();
    assert_eq!(
        execute(
            &connection,
            "CALL db.propertyKeys() YIELD propertyKey RETURN propertyKey ORDER BY propertyKey",
            hidden()
        )
        .expect("property keys"),
        vec![vec![Value::String("name".to_owned())]]
    );
    assert_eq!(execute(&connection, "MATCH (p:Person {name:'A'}) CALL (p) { RETURN properties(p) AS props, p{.*} AS projection, keys(p) AS names } RETURN props, projection, names, {secret:99}.secret", hidden()).expect("enumeration"), vec![vec![
        Value::Map(BTreeMap::from([("name".to_owned(), Value::String("A".to_owned()))])),
        Value::Map(BTreeMap::from([("name".to_owned(), Value::String("A".to_owned()))])),
        Value::List(vec![Value::String("name".to_owned())]),
        Value::Integer(99),
    ]]);
    assert_eq!(
        execute(
            &connection,
            "MATCH (p:Person {name:'A'}) RETURN p.secret, p.audit_1",
            "{}"
        )
        .expect("canonical properties retained"),
        vec![vec![Value::Integer(11), Value::Integer(21)]]
    );
}

#[test]
fn search_rejects_hidden_sources_and_only_filter_properties_it_uses() {
    let connection = fixture();
    execute(
        &connection,
        "MATCH (p:Person) SET p.embedding = vector([1.0,0.0],2,FLOAT64) FINISH",
        "{}",
    )
    .expect("vectors");
    execute(
        &connection,
        "CREATE VECTOR INDEX people_vectors FOR (p:Person) ON (p.embedding) WITH [p.secret]",
        "{}",
    )
    .expect("vector index");
    let search = "MATCH (p:Person) SEARCH p IN (VECTOR INDEX people_vectors FOR [1.0,0.0] LIMIT 1) RETURN p.name";
    assert_eq!(
        execute(&connection, search, hidden())
            .expect("unused hidden filter field")
            .len(),
        1
    );
    for query in [
        "MATCH (p:Person) SEARCH p IN (VECTOR INDEX people_vectors FOR [1.0,0.0] WHERE p.secret = 11 LIMIT 1) RETURN p.name",
        "MATCH (p:Person) SEARCH p IN (VECTOR INDEX people_vectors FOR [1.0,0.0] WHERE p.secret = 11 LIMIT 0) RETURN p.name",
    ] {
        assert_eq!(
            execute(&connection, query, hidden())
                .expect_err("hidden filter")
                .kind,
            QueryErrorKind::AccessDenied
        );
    }
    for limit in [0, 1] {
        let query = format!(
            "MATCH (p:Person) SEARCH p IN (VECTOR INDEX people_vectors FOR [1.0,0.0] LIMIT {limit}) RETURN p.name"
        );
        assert_eq!(
            execute(
                &connection,
                &query,
                r#"{"readAccess":{"query":{"denyProperties":["embedding"]}}}"#
            )
            .expect_err("hidden source")
            .kind,
            QueryErrorKind::AccessDenied
        );
    }
}

#[test]
fn endpoint_functions_and_projection_barriers_keep_query_objects_restricted() {
    let connection = fixture();
    let rows = execute(&connection,
        "MATCH (a:Person {name:'A'})-[r:KNOWS]->(b) WITH startNode(r) AS sourceNode, endNode(r) AS targetNode, {secret:99, nested:[a,r]} AS boxed RETURN sourceNode, targetNode, sourceNode.secret, targetNode.secret, boxed ORDER BY sourceNode.name SKIP 0 LIMIT 1",
        hidden()).expect("endpoint materialization and projection barrier");
    assert_eq!(rows.len(), 1);
    for value in &rows[0][0..2] {
        let Value::Node(node) = value else {
            panic!("endpoint Node")
        };
        assert!(!node.properties.contains_key("secret"));
        assert!(!node.properties.contains_key("audit_1"));
    }
    assert_eq!(rows[0][2], Value::Null);
    assert_eq!(rows[0][3], Value::Null);
    let Value::Map(boxed) = &rows[0][4] else {
        panic!("ordinary Map")
    };
    assert_eq!(boxed["secret"], Value::Integer(99));
    let Value::List(nested) = &boxed["nested"] else {
        panic!("nested graph objects")
    };
    assert!(matches!(&nested[0], Value::Node(node) if !node.properties.contains_key("secret")));
    assert!(
        matches!(&nested[1], Value::Relationship(relationship) if !relationship.properties.contains_key("secret"))
    );

    let rows = execute(
        &connection,
        "MATCH (p:Person) WITH DISTINCT p ORDER BY p.name SKIP 1 LIMIT 1 RETURN collect(p)",
        hidden(),
    )
    .expect("distinct sort paginate and aggregate");
    let Value::List(nodes) = &rows[0][0] else {
        panic!("collected Node values")
    };
    assert_eq!(nodes.len(), 1);
    let Value::Node(node) = &nodes[0] else {
        panic!("collected Node")
    };
    assert_eq!(node.properties["name"], Value::String("B".to_owned()));
    assert!(!node.properties.contains_key("secret"));
    assert!(!node.properties.contains_key("audit_1"));
}

#[test]
fn simple_stream_projection_and_count_fast_paths_apply_the_same_rules() {
    let connection = fixture();
    let rows = execute(&connection,
        "MATCH (p:Person) RETURN p.secret, p['audit_1'], keys(p), properties(p), p{.*}, p{.secret}, p.secret IS NULL ORDER BY p.name",
        hidden()).expect("simple stream projection");
    let expected = ["A", "B"]
        .into_iter()
        .map(|name| {
            let properties = Value::Map(BTreeMap::from([(
                "name".to_owned(),
                Value::String(name.to_owned()),
            )]));
            vec![
                Value::Null,
                Value::Null,
                Value::List(vec![Value::String("name".to_owned())]),
                properties.clone(),
                properties,
                Value::Map(BTreeMap::from([("secret".to_owned(), Value::Null)])),
                Value::Boolean(true),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(rows, expected);
    assert_eq!(
        execute(
            &connection,
            "MATCH (p:Person) RETURN count(p.secret), count(p)",
            hidden()
        )
        .expect("count fast path"),
        vec![vec![Value::Integer(0), Value::Integer(2)]]
    );
}

#[test]
fn optional_hidden_relationship_predicates_preserve_the_left_row_and_null_extend() {
    let connection = fixture();
    for query in [
        "MATCH (a:Person {name:'A'}) OPTIONAL MATCH (a)-[r:KNOWS {secret:31}]->(b) RETURN a.name, r.secret, b.name",
        "MATCH (a:Person {name:'A'}) OPTIONAL MATCH (a)-[r:KNOWS]->(b) WHERE r.secret=31 RETURN a.name, r.secret, b.name",
    ] {
        assert_eq!(
            execute(&connection, query, "{}").expect("baseline optional relationship"),
            vec![vec![
                Value::String("A".to_owned()),
                Value::Integer(31),
                Value::String("B".to_owned()),
            ]]
        );
        assert_eq!(
            execute(&connection, query, hidden()).expect("restricted optional relationship"),
            vec![vec![
                Value::String("A".to_owned()),
                Value::Null,
                Value::Null,
            ]],
            "hidden optional predicates must not discard the already matched left row: {query}"
        );
    }
}

#[test]
fn nested_graph_aliases_cannot_restore_hidden_node_relationship_or_path_properties() {
    let connection = fixture();
    let query = "MATCH p=(a:Person {name:'A'})-[r:KNOWS]->(b) WITH {node:a,edge:r,path:p,secret:99} AS box RETURN box.node.secret, box.edge['secret'], [x IN nodes(box.path)|x.secret], [x IN relationships(box.path)|x.secret], box.secret";
    let baseline = vec![vec![
        Value::Integer(11),
        Value::Integer(31),
        Value::List(vec![Value::Integer(11), Value::Integer(12)]),
        Value::List(vec![Value::Integer(31)]),
        Value::Integer(99),
    ]];
    assert_eq!(
        execute(&connection, query, "{}").expect("baseline nested graph aliases"),
        baseline
    );
    assert_eq!(
        execute(&connection, query, hidden()).expect("restricted nested graph aliases"),
        vec![vec![
            Value::Null,
            Value::Null,
            Value::List(vec![Value::Null, Value::Null]),
            Value::List(vec![Value::Null]),
            Value::Integer(99),
        ]]
    );
    assert_eq!(
        execute(&connection, query, "{}").expect("canonical data was not masked in storage"),
        baseline
    );
}
