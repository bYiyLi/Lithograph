use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryErrorKind, prepare};
use lithograph_core::storage::{branch_head, create_storage_schema, initialize_root};
use rusqlite::Connection;

fn storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn execute(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Vec<Vec<Value>> {
    let prepared = prepare(connection, query, params, options).expect(query);
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 1).expect(query);
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection).expect("complete query");
            return rows;
        }
    }
}

fn query(connection: &Connection, text: &str) -> Vec<Vec<Value>> {
    execute(
        connection,
        text,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
}

fn assert_count(connection: &Connection, text: &str, expected: i64) {
    assert_eq!(
        query(connection, text),
        vec![vec![Value::Integer(expected)]],
        "{text}"
    );
}

fn seed(connection: &Connection) {
    query(
        connection,
        "CREATE (a:人物:Person {name:'甲', rank:1}), (b:`人物`:`Person` {name:'乙', rank:2}), (a)-[:相识]->(b), (a)-[:`KNOWS`]->(b) FINISH",
    );
}

#[test]
fn quoted_name_ast_keeps_original_text_and_source_span() {
    let source = "MATCH (n:`带``反引号`)-[:`相 识`]->() RETURN n";
    let ast = lithograph_core::cypher::parse(source).expect("parse quoted names");
    for (kind, text) in [
        (lithograph_core::cypher::AstKind::LabelName, "`带``反引号`"),
        (
            lithograph_core::cypher::AstKind::RelationshipTypeName,
            "`相 识`",
        ),
    ] {
        let node = ast
            .root
            .descendants()
            .find(|node| node.kind == kind)
            .expect("name AST");
        assert_eq!(node.text.as_deref(), Some(text));
        assert_eq!(&source[node.span.start..node.span.end], text);
    }
}

#[test]
fn quoted_labels_and_relationship_types_equal_unquoted_names() {
    let connection = storage();
    seed(&connection);
    for text in [
        "MATCH (n:人物) RETURN count(n)",
        "MATCH (n:`人物`) RETURN count(n)",
        "MATCH (n:Person) RETURN count(n)",
        "MATCH (n:`Person`) RETURN count(n)",
        "MATCH (n) WHERE n:`人物` RETURN count(n)",
        "MATCH (n) WHERE n:`Person` RETURN count(n)",
        "MATCH (n:`人物`:`Person`) RETURN count(n)",
    ] {
        assert_count(&connection, text, 2);
    }
    for text in [
        "MATCH ()-[r:相识]->() RETURN count(r)",
        "MATCH ()-[r:`相识`]->() RETURN count(r)",
        "MATCH ()-[r:KNOWS]->() RETURN count(r)",
        "MATCH ()-[r:`KNOWS`]->() RETURN count(r)",
        "MATCH ()-[r]->() WHERE r:`相识` RETURN count(r)",
        "MATCH (a:`人物`)-[r:`相识`]->(b:`Person`) RETURN count(r)",
    ] {
        assert_count(&connection, text, 1);
    }
    assert_count(&connection, "MATCH (n:`不存在`) RETURN count(n)", 0);
    assert_count(&connection, "MATCH ()-[r:`不存在`]->() RETURN count(r)", 0);
}

#[test]
fn quoted_static_names_produce_the_same_access_plans() {
    let connection = storage();
    seed(&connection);
    for (plain, escaped) in [
        (
            "MATCH (n:人物) RETURN n.name AS name",
            "MATCH (n:`人物`) RETURN n.name AS name",
        ),
        (
            "WITH 1 AS x MATCH (n:人物) RETURN n.name AS name",
            "WITH 1 AS x MATCH (n:`人物`) RETURN n.name AS name",
        ),
        (
            "MATCH (a:人物)-[:相识]->(b:人物) RETURN b.name AS name",
            "MATCH (a:`人物`)-[:`相识`]->(b:`人物`) RETURN b.name AS name",
        ),
    ] {
        let plain = prepare(
            &connection,
            plain,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .expect("plain access plan");
        let escaped = prepare(
            &connection,
            escaped,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .expect("escaped access plan");
        assert_eq!(plain.logical, escaped.logical);
        assert_eq!(plain.physical, escaped.physical);
    }
}

#[test]
fn generic_name_expressions_and_complex_paths_decode_static_atoms() {
    let connection = storage();
    seed(&connection);
    for text in [
        "MATCH (n:`人物`&`Person`) RETURN count(n)",
        "MATCH (n:`人物`|Other) RETURN count(n)",
        "MATCH (n:!`不存在`) RETURN count(n)",
        "MATCH (n) WHERE n:`人物`&!`不存在` RETURN count(n)",
        "WITH 1 AS x MATCH (n:`人物`) RETURN count(n)",
    ] {
        assert_count(&connection, text, 2);
    }
    for text in [
        "MATCH (a:`人物`)-[r:`相识`*1..2]->(b:`Person`) RETURN count(b)",
        "MATCH (a:`人物`)-[r:`相识`|`KNOWS`]->(b:`Person`) RETURN count(b)",
        "MATCH (a:`人物`)-[:`相识`]->(b:`人物`), (b)<-[:`KNOWS`]-(a) RETURN count(b)",
    ] {
        let expected = if text.contains("|`KNOWS`") { 2 } else { 1 };
        assert_count(&connection, text, expected);
    }
    assert_eq!(
        query(
            &connection,
            "MATCH (n:`人物`) OPTIONAL MATCH (n)-[:`不存在`]->(m) RETURN n.name, m ORDER BY n.name"
        ),
        vec![
            vec![Value::String("乙".to_owned()), Value::Null],
            vec![Value::String("甲".to_owned()), Value::Null],
        ]
    );
}

fn quoted(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

#[test]
fn escaped_names_are_decoded_once_and_dynamic_names_are_literal() {
    let connection = storage();
    for name in ["人 物", "MATCH", "人.物", "带`反引号", "`Person`"] {
        let identifier = quoted(name);
        let params = BTreeMap::from([("name".to_owned(), Value::String(name.to_owned()))]);
        execute(
            &connection,
            &format!("CREATE (a:{identifier})-[:{identifier}]->(b:Target) FINISH"),
            BTreeMap::new(),
            ExecutionOptions::default(),
        );
        for text in [
            format!("MATCH (n:{identifier}) RETURN count(n)"),
            format!("MATCH (n) WHERE n:{identifier} RETURN count(n)"),
            format!("MATCH ()-[r:{identifier}]->() RETURN count(r)"),
            format!("MATCH ()-[r]->() WHERE r:{identifier} RETURN count(r)"),
            "MATCH (n:$($name)) RETURN count(n)".to_owned(),
            "MATCH ()-[r:$($name)]->() RETURN count(r)".to_owned(),
        ] {
            assert_eq!(
                execute(
                    &connection,
                    &text,
                    params.clone(),
                    ExecutionOptions::default()
                ),
                vec![vec![Value::Integer(1)]],
                "name={name:?}, query={text}"
            );
        }
    }
    assert_count(&connection, "MATCH (n:Person) RETURN count(n)", 0);
    for text in [
        "MATCH (n:$($name)) RETURN n",
        "MATCH ()-[r:$($name)]->() RETURN r",
    ] {
        for invalid in [
            Value::Null,
            Value::Integer(1),
            Value::List(vec![Value::Null]),
        ] {
            let error = prepare(
                &connection,
                text,
                BTreeMap::from([("name".to_owned(), invalid)]),
                ExecutionOptions::default(),
            )
            .and_then(|prepared| QueryCursor::new(prepared).next_batch(&connection, 1))
            .expect_err("invalid dynamic name");
            assert_eq!(error.kind, QueryErrorKind::Type, "{text}");
        }
    }
}

#[test]
fn quoted_name_writes_and_merge_share_the_read_name() {
    let connection = storage();
    seed(&connection);
    query(
        &connection,
        "MERGE (n:`人物` {name:'甲'}) ON MATCH SET n:`已读` RETURN n.name",
    );
    assert_count(&connection, "MATCH (n:人物) RETURN count(n)", 2);
    assert_count(&connection, "MATCH (n:已读) RETURN count(n)", 1);
    assert_count(&connection, "MATCH (n:`已读`) RETURN count(n)", 1);
    query(&connection, "MATCH (n:`已读`) REMOVE n:`已读` FINISH");
    assert_count(&connection, "MATCH (n:已读) RETURN count(n)", 0);
    query(&connection, "CREATE (:`人``物`) FINISH");
    assert_count(&connection, "MATCH (n:`人``物`) RETURN count(n)", 1);
}

#[test]
fn quoted_names_preserve_indexes_history_and_graph_view() {
    let connection = storage();
    seed(&connection);
    for text in [
        "CREATE CONSTRAINT `人物 排名类型` FOR (n:`人物`) REQUIRE n.rank IS :: INTEGER",
        "CREATE CONSTRAINT `关系 排名类型` FOR ()-[r:`相识`]-() REQUIRE r.rank IS :: INTEGER",
        "CREATE RANGE INDEX `人物 排名` FOR (n:`人物`) ON (n.rank)",
        "CREATE RANGE INDEX `关系 排名` FOR ()-[r:`相识`]-() ON (r.rank)",
    ] {
        query(&connection, text);
    }
    query(&connection, "MATCH ()-[r:`相识`]->() SET r.rank=1 FINISH");
    let historical = branch_head(&connection, "main").expect("historical head");
    for text in [
        "EXPLAIN MATCH (n:`人物`) WHERE n.rank=1 RETURN n.name",
        "EXPLAIN MATCH ()-[r:`相识`]->() WHERE r.rank=1 RETURN r.rank",
    ] {
        assert!(
            query(&connection, text).iter().flatten().any(|value| {
                matches!(value, Value::String(plan) if plan.contains("IndexSeek"))
            }),
            "{text}"
        );
    }
    query(
        &connection,
        "CREATE (:`人物`:Hidden {name:'丙', rank:1}) FINISH",
    );
    for options in [
        format!(r#"{{"at":"commit/{}"}}"#, historical.to_hex()),
        r#"{"graphView":{"requireAllLabels":["人物"],"excludeAnyLabels":["Hidden"]}}"#.to_owned(),
        format!(
            r#"{{"at":"commit/{}","graphView":{{"requireAllLabels":["人物"]}}}}"#,
            historical.to_hex()
        ),
    ] {
        let options = ExecutionOptions::parse_text(&options).expect("execution options");
        for text in [
            "MATCH (n:`人物`) RETURN count(n)",
            "WITH 1 AS x MATCH (n:`人物`) RETURN count(n)",
        ] {
            assert_eq!(
                execute(&connection, text, BTreeMap::new(), options.clone()),
                vec![vec![Value::Integer(2)]]
            );
        }
        for text in [
            "MATCH (n:`人物`) WHERE n.rank=1 RETURN n.name",
            "MATCH (a:`人物`)-[:`相识`*1..2]->(b:`人物`) RETURN a.name",
        ] {
            assert_eq!(
                execute(&connection, text, BTreeMap::new(), options.clone()),
                vec![vec![Value::String("甲".to_owned())]]
            );
        }
    }
}
