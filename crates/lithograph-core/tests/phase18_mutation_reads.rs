use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, prepare};
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

fn hidden_secret() -> ExecutionOptions {
    ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#)
        .expect("query property rule")
}

fn execute(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<Vec<Vec<Value>>, QueryError> {
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

fn unrestricted(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    execute(connection, query, ExecutionOptions::default()).expect("unrestricted query")
}

fn seed_people(connection: &Connection) {
    unrestricted(
        connection,
        "CREATE (:ReadProbe {name:'A', secret:1}), (:ReadProbe {name:'B', secret:9}) FINISH",
    );
}

#[test]
fn write_return_masks_explicit_values_enumeration_maps_and_graph_objects() {
    let connection = fresh_storage();
    seed_people(&connection);
    let rows = execute(
        &connection,
        "MATCH (n:ReadProbe) SET n.visited = true RETURN n.secret, n['secret'], keys(n), properties(n), n{.*}, n",
        hidden_secret(),
    )
    .expect("write result projection");
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row[0], Value::Null);
        assert_eq!(row[1], Value::Null);
        let Value::List(keys) = &row[2] else {
            panic!("keys must return a List");
        };
        assert!(!keys.contains(&Value::String("secret".to_owned())));
        for value in [&row[3], &row[4]] {
            let Value::Map(properties) = value else {
                panic!("property projection must return a Map");
            };
            assert!(!properties.contains_key("secret"));
            assert_eq!(properties.get("visited"), Some(&Value::Boolean(true)));
        }
        let Value::Node(node) = &row[5] else {
            panic!("RETURN n must materialize a Node");
        };
        assert!(!node.properties.contains_key("secret"));
    }
    assert_eq!(
        unrestricted(
            &connection,
            "MATCH (n:ReadProbe) RETURN n.secret ORDER BY n.name"
        ),
        vec![vec![Value::Integer(1)], vec![Value::Integer(9)]]
    );
}

#[test]
fn write_barriers_and_count_use_masked_values() {
    let connection = fresh_storage();
    seed_people(&connection);
    assert_eq!(
        execute(
            &connection,
            "MATCH (n:ReadProbe) SET n.visited = true RETURN n.secret AS hidden, n.name AS name ORDER BY n.secret DESC, n.name",
            hidden_secret(),
        )
        .expect("masked sort key"),
        vec![
            vec![Value::Null, Value::String("A".to_owned())],
            vec![Value::Null, Value::String("B".to_owned())],
        ]
    );
    assert_eq!(
        execute(
            &connection,
            "MATCH (n:ReadProbe) SET n.visited = true RETURN DISTINCT n.secret",
            hidden_secret(),
        )
        .expect("masked distinct key"),
        vec![vec![Value::Null]]
    );
    assert_eq!(
        execute(
            &connection,
            "MATCH (n:ReadProbe) SET n.visited = true RETURN count(n.secret), count(n)",
            hidden_secret(),
        )
        .expect("masked count argument"),
        vec![vec![Value::Integer(0), Value::Integer(2)]]
    );
}

#[test]
fn property_map_mutations_copy_only_query_visible_properties() {
    let connection = fresh_storage();
    seed_people(&connection);
    execute(
        &connection,
        "MATCH (n:ReadProbe) CREATE (copy:Copy) SET copy = properties(n) RETURN copy",
        hidden_secret(),
    )
    .expect("copy query-visible property maps");
    assert_eq!(
        unrestricted(
            &connection,
            "MATCH (n:Copy) RETURN n.name, n.secret ORDER BY n.name"
        ),
        vec![
            vec![Value::String("A".to_owned()), Value::Null],
            vec![Value::String("B".to_owned()), Value::Null],
        ]
    );
}

#[test]
fn merge_does_not_match_hidden_properties_but_still_validates_canonical_constraints() {
    let connection = fresh_storage();
    seed_people(&connection);
    let rows = execute(
        &connection,
        "MERGE (n:ReadProbe {secret:1}) RETURN n.secret, n.name",
        hidden_secret(),
    )
    .expect("read restriction is independent of permitted CREATE");
    assert_eq!(rows, vec![vec![Value::Null, Value::Null]]);
    assert_eq!(
        unrestricted(&connection, "MATCH (n:ReadProbe) RETURN count(n)"),
        vec![vec![Value::Integer(3)]]
    );

    let constrained = fresh_storage();
    seed_people(&constrained);
    unrestricted(
        &constrained,
        "CREATE CONSTRAINT probe_secret_unique FOR (n:ReadProbe) REQUIRE n.secret IS UNIQUE",
    );
    let before = branch_head(&constrained, "main").expect("constrained head");
    let error = execute(
        &constrained,
        "MERGE (n:ReadProbe {secret:1}) RETURN n.secret",
        hidden_secret(),
    )
    .expect_err("hidden match cannot evade canonical uniqueness");
    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&constrained, "main").expect("head after error"),
        before
    );
    assert_eq!(
        unrestricted(&constrained, "MATCH (n:ReadProbe) RETURN count(n)"),
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn new_properties_and_paths_are_masked_without_changing_the_written_state() {
    let connection = fresh_storage();
    let rows = execute(
        &connection,
        "CREATE p=(a:PathProbe {secret:1})-[r:LINK {secret:2}]->(b:PathProbe {secret:3}) RETURN p, a.secret, r.secret, b.secret",
        hidden_secret(),
    )
    .expect("create and project a Path");
    assert_eq!(rows.len(), 1);
    assert_eq!(&rows[0][1..], &[Value::Null, Value::Null, Value::Null]);
    let Value::Path(path) = &rows[0][0] else {
        panic!("RETURN p must produce a Path");
    };
    assert_eq!(path.nodes.len(), 2);
    assert_eq!(path.relationships.len(), 1);
    assert!(
        path.nodes
            .iter()
            .all(|node| !node.properties.contains_key("secret"))
    );
    assert!(
        path.relationships
            .iter()
            .all(|rel| !rel.properties.contains_key("secret"))
    );
    assert_eq!(
        unrestricted(
            &connection,
            "MATCH (a:PathProbe)-[r:LINK]->(b:PathProbe) RETURN a.secret, r.secret, b.secret"
        ),
        vec![vec![
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(3)
        ]]
    );
}

#[test]
fn merge_null_patterns_still_reject_before_matching_existing_missing_properties() {
    let connection = fresh_storage();
    seed_people(&connection);
    unrestricted(&connection, "CREATE (:Other {missing:1}) FINISH");
    let before = branch_head(&connection, "main").expect("head before null pattern");
    for options in [ExecutionOptions::default(), hidden_secret()] {
        let error = execute(
            &connection,
            "MERGE (n:ReadProbe {missing:null}) RETURN n",
            options,
        )
        .expect_err("MERGE null remains a semantic error");
        assert_eq!(error.kind, QueryErrorKind::Semantic);
        assert_eq!(
            branch_head(&connection, "main").expect("unchanged head"),
            before
        );
    }
}
