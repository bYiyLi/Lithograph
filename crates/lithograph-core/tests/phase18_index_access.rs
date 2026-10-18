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
                id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL,\
                database_id TEXT NOT NULL, storage_format INTEGER NOT NULL\
            );",
        )
        .expect("metadata table");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn options(text: &str) -> ExecutionOptions {
    ExecutionOptions::parse_text(text).expect("valid options")
}

fn execute(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<Vec<Vec<Value>>, QueryError> {
    execute_with_params(connection, query, BTreeMap::new(), options)
}

fn execute_with_params(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Result<Vec<Vec<Value>>, QueryError> {
    let prepared = prepare(connection, query, params, options)?;
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

fn seed(connection: &Connection) {
    execute(
        connection,
        "CREATE (a:Doc {name:'A', secret:'alpha', public:'graph', lang:'en', embedding:vector([1.0,0.0],2,FLOAT64)}), (b:Doc {name:'B', secret:'beta', public:'graph', lang:'fr', embedding:vector([0.0,1.0],2,FLOAT64)}), (a)-[:LINK {name:'edge', secret:'alpha', public:'graph', embedding:vector([1.0,0.0],2,FLOAT64)}]->(b) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed graph");
}

fn assert_denied(connection: &Connection, query: &str, policy: ExecutionOptions) {
    let error = execute(connection, query, policy).expect_err("restricted Search");
    assert_eq!(error.kind, QueryErrorKind::AccessDenied, "{query}: {error}");
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_AUTH));
    assert!(!error.message.contains("alpha"));
}

#[test]
fn standard_indexes_skip_hidden_predicates_and_preserve_public_seeks() {
    let connection = fresh_storage();
    seed(&connection);
    for query in [
        "CREATE CONSTRAINT doc_secret_type FOR (n:Doc) REQUIRE n.secret IS :: STRING",
        "CREATE CONSTRAINT doc_public_type FOR (n:Doc) REQUIRE n.public IS :: STRING",
        "CREATE CONSTRAINT link_secret_type FOR ()-[r:LINK]-() REQUIRE r.secret IS :: STRING",
        "CREATE RANGE INDEX doc_secret FOR (n:Doc) ON (n.secret)",
        "CREATE TEXT INDEX doc_public FOR (n:Doc) ON (n.public)",
        "CREATE RANGE INDEX link_secret FOR ()-[r:LINK]-() ON (r.secret)",
    ] {
        execute(&connection, query, ExecutionOptions::default()).expect("create index");
    }
    let hidden = options(r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#);
    for query in [
        "EXPLAIN MATCH (n:Doc) WHERE n.secret = 'alpha' RETURN n.name",
        "EXPLAIN MATCH ()-[r:LINK]->() WHERE r.secret = 'alpha' RETURN r.name",
    ] {
        let unrestricted = execute(&connection, query, ExecutionOptions::default())
            .expect("unrestricted index plan");
        assert!(matches!(&unrestricted[0][0], Value::String(plan) if plan.contains("IndexSeek")));
        let plan = execute(&connection, query, hidden.clone()).expect("restricted plan");
        assert!(matches!(&plan[0][0], Value::String(plan) if !plan.contains("IndexSeek")));
    }
    let plan = execute(
        &connection,
        "EXPLAIN MATCH (n:Doc) WHERE n.public STARTS WITH 'gr' RETURN n.name",
        hidden.clone(),
    )
    .expect("unrestricted property index");
    assert!(matches!(&plan[0][0], Value::String(plan) if plan.contains("doc_public")));
    assert!(
        execute(
            &connection,
            "MATCH (n:Doc) WHERE n.secret = 'alpha' RETURN n.name",
            hidden.clone(),
        )
        .expect("hidden equality")
        .is_empty()
    );
    assert_eq!(
        execute(
            &connection,
            "MATCH (n:Doc) WHERE n.secret IS NULL RETURN n.name ORDER BY n.name",
            hidden.clone(),
        )
        .expect("hidden property is absent"),
        vec![
            vec![Value::String("A".to_owned())],
            vec![Value::String("B".to_owned())]
        ],
    );
    assert_eq!(
        execute(
            &connection,
            "MATCH ()-[r:LINK]->() WHERE r.secret IS NULL RETURN r.name, r.secret",
            hidden,
        )
        .expect("hidden relationship property"),
        vec![vec![Value::String("edge".to_owned()), Value::Null]],
    );
}

fn seed_standard_index_matrix(connection: &Connection) {
    execute(
        connection,
        "CREATE (a:Indexed {name:'A', secret:'alpha', rank:1, location:point({x:1.0,y:1.0})}), (b:Indexed {name:'B', secret:'beta', rank:2, location:point({x:9.0,y:9.0})}), (a)-[:INDEXED_LINK {name:'edge', secret:'alpha', rank:1, location:point({x:1.0,y:1.0})}]->(b) FINISH",
        ExecutionOptions::default(),
    )
    .expect("standard index fixture");
    for (property, property_type) in [
        ("secret", "STRING"),
        ("rank", "INTEGER"),
        ("location", "POINT"),
    ] {
        for (prefix, target, variable) in [
            ("node", "(n:Indexed)", "n"),
            ("relationship", "()-[r:INDEXED_LINK]-()", "r"),
        ] {
            execute(
                connection,
                &format!(
                    "CREATE CONSTRAINT {prefix}_{property}_type FOR {target} REQUIRE {variable}.{property} IS :: {property_type}"
                ),
                ExecutionOptions::default(),
            )
            .expect("property type constraint");
        }
    }
    for query in [
        "CREATE RANGE INDEX node_composite FOR (n:Indexed) ON (n.secret, n.rank)",
        "CREATE RANGE INDEX relationship_composite FOR ()-[r:INDEXED_LINK]-() ON (r.secret, r.rank)",
        "CREATE TEXT INDEX node_text FOR (n:Indexed) ON (n.secret)",
        "CREATE TEXT INDEX relationship_text FOR ()-[r:INDEXED_LINK]-() ON (r.secret)",
        "CREATE POINT INDEX node_point FOR (n:Indexed) ON (n.location)",
        "CREATE POINT INDEX relationship_point FOR ()-[r:INDEXED_LINK]-() ON (r.location)",
    ] {
        execute(connection, query, ExecutionOptions::default()).expect("standard index");
    }
}

fn assert_masked_index_predicate(
    connection: &Connection,
    query: &str,
    index: &str,
    hidden_property: &str,
    expected_name: &str,
) {
    let params = BTreeMap::from([
        ("text".to_owned(), Value::String("alpha".to_owned())),
        ("rank".to_owned(), Value::Integer(1)),
    ]);
    let explain = format!("EXPLAIN {query}");
    let baseline = execute_with_params(
        connection,
        &explain,
        params.clone(),
        ExecutionOptions::default(),
    )
    .expect("baseline index plan");
    assert!(
        matches!(&baseline[0][0], Value::String(plan) if plan.contains("IndexSeek") && plan.contains(index)),
        "{index}: {baseline:?}"
    );
    assert_eq!(
        execute_with_params(
            connection,
            query,
            params.clone(),
            ExecutionOptions::default()
        )
        .expect("baseline indexed rows"),
        vec![vec![Value::String(expected_name.to_owned())]],
        "{index} baseline must match a real graph element"
    );
    let hidden = options(&format!(
        r#"{{"readAccess":{{"query":{{"denyProperties":["{hidden_property}"]}}}}}}"#
    ));
    let plan = execute_with_params(connection, &explain, params.clone(), hidden.clone())
        .expect("masked plan");
    assert!(
        matches!(&plan[0][0], Value::String(plan) if !plan.contains("IndexSeek")),
        "{index}: {plan:?}"
    );
    assert!(
        execute_with_params(connection, query, params, hidden)
            .expect("masked indexed predicate")
            .is_empty(),
        "{index} must not select rows using a hidden property's canonical value"
    );
}

#[test]
fn composite_range_and_text_indexes_mask_parameter_predicates_for_nodes_and_relationships() {
    let connection = fresh_storage();
    seed_standard_index_matrix(&connection);
    for (index, query, expected_name) in [
        (
            "node_composite",
            "MATCH (n:Indexed) WHERE n.secret=$text AND n.rank=$rank RETURN n.name",
            "A",
        ),
        (
            "relationship_composite",
            "MATCH ()-[r:INDEXED_LINK]->() WHERE r.secret=$text AND r.rank=$rank RETURN r.name",
            "edge",
        ),
        (
            "node_text",
            "MATCH (n:Indexed) WHERE n.secret STARTS WITH $text RETURN n.name",
            "A",
        ),
        (
            "relationship_text",
            "MATCH ()-[r:INDEXED_LINK]->() WHERE r.secret CONTAINS $text RETURN r.name",
            "edge",
        ),
    ] {
        assert_masked_index_predicate(&connection, query, index, "secret", expected_name);
    }
}

#[test]
fn point_indexes_cannot_match_hidden_bounding_box_or_distance_sources() {
    let connection = fresh_storage();
    seed_standard_index_matrix(&connection);
    for (index, query, expected_name) in [
        (
            "node_point",
            "MATCH (n:Indexed) WHERE point.withinBBox(n.location, point({x:0.0,y:0.0}), point({x:2.0,y:2.0})) RETURN n.name",
            "A",
        ),
        (
            "relationship_point",
            "MATCH ()-[r:INDEXED_LINK]->() WHERE point.distance(r.location, point({x:0.0,y:0.0})) < 2.0 RETURN r.name",
            "edge",
        ),
    ] {
        assert_masked_index_predicate(&connection, query, index, "location", expected_name);
    }
}

#[test]
fn hiding_only_the_second_composite_column_preserves_a_public_single_property_seek() {
    let connection = fresh_storage();
    seed_standard_index_matrix(&connection);
    let hidden = options(r#"{"readAccess":{"query":{"denyProperties":["rank"]}}}"#);
    let params = BTreeMap::from([
        ("text".to_owned(), Value::String("alpha".to_owned())),
        ("rank".to_owned(), Value::Integer(1)),
    ]);
    for (query, composite_index, public_index, expected_name) in [
        (
            "MATCH (n:Indexed) WHERE n.secret=$text AND n.rank=$rank RETURN n.name",
            "node_composite",
            "node_text",
            "A",
        ),
        (
            "MATCH ()-[r:INDEXED_LINK]->() WHERE r.secret=$text AND r.rank=$rank RETURN r.name",
            "relationship_composite",
            "relationship_text",
            "edge",
        ),
    ] {
        let explain = format!("EXPLAIN {query}");
        let baseline = execute_with_params(
            &connection,
            &explain,
            params.clone(),
            ExecutionOptions::default(),
        )
        .expect("baseline composite seek");
        assert!(
            matches!(&baseline[0][0], Value::String(plan) if plan.contains(composite_index)),
            "{baseline:?}"
        );
        assert_eq!(
            execute_with_params(
                &connection,
                query,
                params.clone(),
                ExecutionOptions::default()
            )
            .expect("baseline composite rows"),
            vec![vec![Value::String(expected_name.to_owned())]]
        );
        let restricted = execute_with_params(&connection, &explain, params.clone(), hidden.clone())
            .expect("remaining public seek");
        assert!(
            matches!(&restricted[0][0], Value::String(plan) if plan.contains("IndexSeek") && plan.contains(public_index) && !plan.contains(composite_index)),
            "{restricted:?}"
        );
        assert!(
            execute_with_params(&connection, query, params.clone(), hidden.clone())
                .expect("hidden second column is absent after the public seek")
                .is_empty()
        );
    }
}

#[test]
fn vector_search_rejects_hidden_sources_before_cache_or_zero_limit() {
    let connection = fresh_storage();
    seed(&connection);
    for query in [
        "CREATE VECTOR INDEX doc_vector FOR (n:Doc) ON (n.embedding) WITH [n.lang]",
        "CREATE VECTOR INDEX link_vector FOR ()-[r:LINK]-() ON (r.embedding)",
    ] {
        execute(&connection, query, ExecutionOptions::default()).expect("vector index");
    }
    let node = "MATCH (n:Doc) SEARCH n IN (VECTOR INDEX doc_vector FOR [1.0,0.0] LIMIT 1) SCORE AS score RETURN n.name, score";
    let relationship = "MATCH ()-[r:LINK]->() SEARCH r IN (VECTOR INDEX link_vector FOR [1.0,0.0] LIMIT 1) RETURN r.name";
    for query in [node, relationship] {
        assert!(
            !execute(&connection, query, ExecutionOptions::default())
                .unwrap()
                .is_empty()
        );
        assert_denied(
            &connection,
            query,
            options(r#"{"readAccess":{"query":{"denyPropertyPatterns":["embedding"]}}}"#),
        );
    }
    for query in [
        "MATCH (n:Doc) SEARCH n IN (VECTOR INDEX doc_vector FOR [1.0,0.0] LIMIT 0) RETURN n",
        "WITH null AS q MATCH (n:Doc) SEARCH n IN (VECTOR INDEX doc_vector FOR q LIMIT 1) RETURN n",
    ] {
        assert_denied(
            &connection,
            query,
            options(r#"{"readAccess":{"query":{"denyProperties":["embedding"]}}}"#),
        );
    }
    assert!(
        !execute(
            &connection,
            node,
            options(
                r#"{"readAccess":{"query":{"denyProperties":["lang"]},"allowExternalIO":false}}"#
            ),
        )
        .expect("unused additional property imposes no Search restriction")
        .is_empty()
    );
    assert_denied(
        &connection,
        "MATCH (n:Doc) SEARCH n IN (VECTOR INDEX doc_vector FOR [1.0,0.0] WHERE n.lang = 'en' LIMIT 1) RETURN n.name",
        options(r#"{"readAccess":{"query":{"denyProperties":["lang"]}}}"#),
    );
}

#[test]
fn fulltext_hidden_source_rejection_is_identical_for_nodes_relationships_and_history() {
    let connection = fresh_storage();
    seed(&connection);
    for query in [
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.secret, n.public]",
        "CREATE FULLTEXT INDEX link_text FOR ()-[r:LINK]-() ON EACH [r.secret]",
        "CREATE FULLTEXT INDEX public_text FOR (n:Doc) ON EACH [n.public]",
    ] {
        execute(&connection, query, ExecutionOptions::default()).expect("full-text index");
    }
    let head = branch_head(&connection, "main").expect("head");
    for query in [
        "CALL db.index.fulltext.queryNodes('doc_text','alpha') YIELD node RETURN node.name",
        "CALL db.index.fulltext.queryRelationships('link_text','alpha') YIELD relationship RETURN relationship.name",
    ] {
        assert!(
            !execute(&connection, query, ExecutionOptions::default())
                .unwrap()
                .is_empty()
        );
        assert_denied(
            &connection,
            query,
            options(r#"{"readAccess":{"query":{"denyPropertyPrefixes":["sec"]}}}"#),
        );
        assert_denied(
            &connection,
            query,
            options(&format!(
                r#"{{"at":"commit/{}","readAccess":{{"query":{{"denyProperties":["secret"]}}}}}}"#,
                head.to_hex()
            )),
        );
    }
    assert_eq!(
        execute(
            &connection,
            "CALL db.index.fulltext.queryNodes('public_text','graph') YIELD node RETURN node.name, node.secret ORDER BY node.name",
            options(r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#),
        )
        .expect("public Search source remains accessible"),
        vec![
            vec![Value::String("A".to_owned()), Value::Null],
            vec![Value::String("B".to_owned()), Value::Null],
        ],
    );
}
