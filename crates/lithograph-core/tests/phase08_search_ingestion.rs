use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QuerySummary, SnapshotSelector, prepare,
};
use lithograph_core::storage::{branch_head, create_storage_schema, initialize_root};
use rusqlite::Connection;

fn csv_fixture(name: &str, contents: &str) -> (PathBuf, String) {
    let path = std::env::temp_dir().join(format!(
        "lithograph_phase08_{}_{}_{}.csv",
        std::process::id(),
        name,
        contents.len()
    ));
    fs::write(&path, contents).expect("write csv fixture");
    let uri = local_file_uri(&path);
    (path, uri)
}

fn local_file_uri(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        format!("file:///{}", normalized.trim_start_matches('/'))
    } else {
        format!("file://{normalized}")
    }
}

fn one_shot_http_csv(contents: &'static str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local HTTP fixture");
    let address = listener.local_addr().expect("local HTTP fixture address");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept local HTTP fixture");
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).expect("read HTTP request");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/csv\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            contents.len(),
            contents
        );
        stream
            .write_all(response.as_bytes())
            .expect("write HTTP response");
    });
    (format!("http://{address}/fixture.csv"), handle)
}

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite must open");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1),magic TEXT NOT NULL,database_id TEXT NOT NULL,storage_format INTEGER NOT NULL);",
        )
        .expect("metadata table");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn fresh_file_storage(name: &str) -> (PathBuf, Connection) {
    let path = std::env::temp_dir().join(format!(
        "lithograph_phase08_{}_{}.sqlite",
        std::process::id(),
        name
    ));
    let _ = fs::remove_file(&path);
    let connection = Connection::open(&path).expect("file SQLite must open");
    connection
        .busy_timeout(Duration::ZERO)
        .expect("disable SQLite busy wait for retry fixture");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1),magic TEXT NOT NULL,database_id TEXT NOT NULL,storage_format INTEGER NOT NULL);",
        )
        .expect("metadata table");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    (path, connection)
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
        let batch = cursor.next_batch(connection, 32)?;
        rows.extend(batch.rows);
        if batch.done {
            let summary = cursor.complete(connection)?;
            return Ok((rows, summary));
        }
    }
}

fn commit_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count")
}

#[test]
fn vector_index_searches_nodes_with_score_and_filter() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc {name: 'A', lang: 'en', embedding: vector([1.0, 0.0], 2, FLOAT64)}), (:Doc {name: 'B', lang: 'fr', embedding: vector([0.8, 0.2], 2, FLOAT64)}), (:Doc {name: 'C', lang: 'en', embedding: vector([0.0, 1.0], 2, FLOAT64)})",
        ExecutionOptions::default(),
    )
    .expect("create documents");
    execute(
        &connection,
        "CREATE VECTOR INDEX doc_embedding FOR (n:Doc) ON (n.embedding) WITH [n.lang] OPTIONS {indexConfig: {`vector.dimensions`: 2, `vector.similarity_function`: 'cosine'}}",
        ExecutionOptions::default(),
    )
    .expect("create vector index");

    let search = "MATCH (n:Doc) SEARCH n IN (VECTOR INDEX doc_embedding FOR vector([1.0, 0.0], 2, FLOAT64) WHERE n.lang = 'en' LIMIT 2) SCORE AS score RETURN n.name AS name, score";
    let (rows, _) =
        execute(&connection, search, ExecutionOptions::default()).expect("vector search");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::String("A".to_owned()));
    assert_eq!(rows[1][0], Value::String("C".to_owned()));
    assert!(matches!(rows[0][1], Value::Float(score) if score >= 0.99));

    let (cached_rows, _) = execute(&connection, search, ExecutionOptions::default())
        .expect("vector search uses HNSW cache");
    assert_eq!(cached_rows, rows);
    connection
        .execute(
            "UPDATE temp._lithograph_vector_cache SET neighbors_json = 'not-json' WHERE owner_id = (SELECT min(owner_id) FROM temp._lithograph_vector_cache)",
            [],
        )
        .expect("corrupt disposable vector cache");
    let (recovered_rows, _) = execute(&connection, search, ExecutionOptions::default())
        .expect("corrupt HNSW cache falls back and rebuilds");
    assert_eq!(recovered_rows, rows);
    connection
        .execute_batch(
            "DROP TABLE temp._lithograph_vector_cache; DROP TABLE temp._lithograph_vector_cache_meta;",
        )
        .expect("delete derived vector cache");
    let (rebuilt_rows, _) = execute(&connection, search, ExecutionOptions::default())
        .expect("vector exact fallback after cache deletion");
    assert_eq!(rebuilt_rows, rows);
}

#[test]
fn vector_search_covers_relationship_optional_historical_and_planner_paths() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (a:Visible {name:'A'}), (b:Visible {name:'B'}), (c:Hidden {name:'C'}), (a)-[:RELATED {name:'near', embedding:vector([0.9,0.1],2,FLOAT64)}]->(b), (a)-[:RELATED {name:'hidden', embedding:vector([1.0,0.0],2,FLOAT64)}]->(c), (:Doc {name:'old', embedding:vector([0.0,1.0],2,FLOAT64), text:'legacy graph'})",
        ExecutionOptions::default(),
    )
    .expect("create vector fixtures");
    execute(
        &connection,
        "CREATE VECTOR INDEX related_embedding FOR ()-[r:RELATED]-() ON (r.embedding)",
        ExecutionOptions::default(),
    )
    .expect("relationship vector index");
    execute(
        &connection,
        "CREATE VECTOR INDEX historical_embedding FOR (n:Doc) ON (n.embedding)",
        ExecutionOptions::default(),
    )
    .expect("historical vector index");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX historical_text FOR (n:Doc) ON EACH [n.text]",
        ExecutionOptions::default(),
    )
    .expect("historical fulltext index");
    let historical = branch_head(&connection, "main").expect("historical head");

    let mut visible = ExecutionOptions::default();
    visible.graph_view.require_all_labels = ["Visible".to_owned()].into_iter().collect();
    let (rows, _) = execute(
        &connection,
        "MATCH ()-[r:RELATED]->() SEARCH r IN (VECTOR INDEX related_embedding FOR vector([1.0,0.0],2,FLOAT64) LIMIT 1) SCORE AS score RETURN r.name AS name, score",
        visible,
    )
    .expect("relationship search honors endpoint visibility");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("near".to_owned()));
    let mut cached_visible = ExecutionOptions::default();
    cached_visible.graph_view.require_all_labels = ["Visible".to_owned()].into_iter().collect();
    let (cached_rows, _) = execute(
        &connection,
        "MATCH ()-[r:RELATED]->() SEARCH r IN (VECTOR INDEX related_embedding FOR vector([1.0,0.0],2,FLOAT64) LIMIT 1) RETURN r.name",
        cached_visible,
    )
    .expect("cached HNSW search filters hidden top candidate before LIMIT");
    assert_eq!(cached_rows, vec![vec![Value::String("near".to_owned())]]);

    let mut empty_view = ExecutionOptions::default();
    empty_view.graph_view.require_all_labels =
        ["MissingViewLabel".to_owned()].into_iter().collect();
    let (optional, _) = execute(
        &connection,
        "OPTIONAL MATCH (n:Doc) SEARCH n IN (VECTOR INDEX historical_embedding FOR vector([1.0,0.0],2,FLOAT64) LIMIT 1) RETURN n",
        empty_view,
    )
    .expect("optional search");
    assert_eq!(optional, vec![vec![Value::Null]]);

    execute(
        &connection,
        "CREATE (:Doc {name:'future', embedding:vector([1.0,0.0],2,FLOAT64), text:'future graph'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("future document");
    let (current, _) = execute(
        &connection,
        "MATCH (n:Doc) SEARCH n IN (VECTOR INDEX historical_embedding FOR vector([1.0,0.0],2,FLOAT64) LIMIT 1) RETURN n.name",
        ExecutionOptions::default(),
    )
    .expect("current vector search");
    assert_eq!(current, vec![vec![Value::String("future".to_owned())]]);

    let mut at_historical = ExecutionOptions::default();
    at_historical.snapshot = SnapshotSelector::Commit(historical.to_hex());
    let (old, _) = execute(
        &connection,
        "MATCH (n:Doc) SEARCH n IN (VECTOR INDEX historical_embedding FOR vector([1.0,0.0],2,FLOAT64) LIMIT 1) RETURN n.name",
        at_historical.clone(),
    )
    .expect("historical vector search");
    assert_eq!(old, vec![vec![Value::String("old".to_owned())]]);
    let (no_future_text, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('historical_text', 'future') YIELD node RETURN node.name",
        at_historical,
    )
    .expect("historical fulltext query");
    assert!(no_future_text.is_empty());

    let prepared = prepare(
        &connection,
        "EXPLAIN MATCH (n:Doc) SEARCH n IN (VECTOR INDEX historical_embedding FOR vector([1.0,0.0],2,FLOAT64) LIMIT 1) RETURN n",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare SEARCH EXPLAIN");
    let explanation = prepared.physical.explain();
    assert!(explanation.contains("VectorSearch"));
    assert!(explanation.contains("historical_embedding"));
}

#[test]
fn fulltext_index_queries_nodes_and_applies_skip_limit_after_visibility() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc:Visible {title: 'graph database'}), (:Doc:Hidden {title: 'graph database'}), (:Doc:Visible {title: 'graph engine'})",
        ExecutionOptions::default(),
    )
    .expect("create documents");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.title]",
        ExecutionOptions::default(),
    )
    .expect("create fulltext index");

    let mut options = ExecutionOptions::default();
    options.graph_view.require_all_labels = ["Visible".to_owned()].into_iter().collect();
    let query = "CALL db.index.fulltext.queryNodes('doc_text', 'graph', {skip: 1, limit: 1}) YIELD node, score RETURN node.title AS title, score";
    let (rows, _) = execute(&connection, query, options.clone()).expect("fulltext query");
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0][0], Value::String(value) if value.starts_with("graph")));
    assert!(matches!(rows[0][1], Value::Float(score) if score >= 0.0));

    let table = connection
        .query_row(
            "SELECT name FROM temp.sqlite_schema WHERE type='table' AND name LIKE '_lithograph_fts_%' ORDER BY name LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("full-text derived table");
    connection
        .execute_batch(&format!("DROP TABLE temp.{table}"))
        .expect("delete full-text cache");
    let (rebuilt, _) = execute(&connection, query, options).expect("rebuild full-text cache");
    assert_eq!(rebuilt, rows);
}

#[test]
fn fulltext_relationship_query_supports_score_options_and_analyzer_contract() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (a:Doc)-[:MENTIONS {text:'running graphs quickly'}]->(b:Doc), (a)-[:MENTIONS {text:'other topic'}]->(b) FINISH",
        ExecutionOptions::default(),
    )
    .expect("relationship text fixtures");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX mention_text FOR ()-[r:MENTIONS]-() ON EACH [r.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'porter unicode61'}}",
        ExecutionOptions::default(),
    )
    .expect("relationship fulltext index");
    let (rows, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryRelationships('mention_text', 'run', {analyzer:'porter unicode61', skip:0, limit:1}) YIELD relationship, score RETURN relationship.text, score",
        ExecutionOptions::default(),
    )
    .expect("relationship fulltext query");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        Value::String("running graphs quickly".to_owned())
    );
    let (override_rows, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryRelationships('mention_text', 'running', {analyzer:'unicode61'}) YIELD relationship RETURN relationship.text",
        ExecutionOptions::default(),
    )
    .expect("query-time analyzer override");
    assert!(override_rows.is_empty());
}

#[test]
fn fulltext_query_supports_profile_query_syntax_and_multi_target_schema() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Employee {name:'Nils-Erik Karlsson', team:'Kernel'}), (:Employee {name:'Nils Johansson', team:'Operations'}), (:Manager {name:'Lisa Danielsson', team:'Kernel'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("create multi-target fulltext fixtures");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX people_text FOR (n:Employee|Manager) ON EACH [n.name, n.team]",
        ExecutionOptions::default(),
    )
    .expect("create multi-label multi-property fulltext index");

    for (query_string, expected) in [
        (
            "nils kernel",
            vec!["Lisa Danielsson", "Nils Johansson", "Nils-Erik Karlsson"],
        ),
        ("nils AND kernel", vec!["Nils-Erik Karlsson"]),
        ("team:\"Operations\"", vec!["Nils Johansson"]),
        ("\"Nils-Erik\"", vec!["Nils-Erik Karlsson"]),
        ("nils NOT kernel", vec!["Nils Johansson"]),
        (
            "nils OR lisa",
            vec!["Lisa Danielsson", "Nils Johansson", "Nils-Erik Karlsson"],
        ),
        ("+nils +kernel", vec!["Nils-Erik Karlsson"]),
        ("nils -kernel", vec!["Nils Johansson"]),
    ] {
        let cypher = format!(
            "CALL db.index.fulltext.queryNodes('people_text', '{query_string}') YIELD node RETURN node.name ORDER BY node.name"
        );
        let (rows, _) = execute(&connection, &cypher, ExecutionOptions::default())
            .unwrap_or_else(|error| panic!("fulltext query {query_string:?} failed: {error}"));
        let expected = expected
            .into_iter()
            .map(|name| vec![Value::String(name.to_owned())])
            .collect::<Vec<_>>();
        assert_eq!(rows, expected, "query={query_string:?}");
    }
}

fn assert_vector_show_configuration(connection: &Connection) {
    let (vector_show, _) = execute(
        connection,
        "SHOW VECTOR INDEXES YIELD name, options RETURN name, options",
        ExecutionOptions::default(),
    )
    .expect("show vector index configuration");
    assert_eq!(vector_show.len(), 1);
    let Value::Map(options) = &vector_show[0][1] else {
        panic!("VECTOR options must be a Map: {vector_show:?}");
    };
    let Some(Value::Map(config)) = options.get("indexConfig") else {
        panic!("VECTOR options must contain indexConfig: {options:?}");
    };
    assert_eq!(
        config.get("vector.default_search_expansion_factor"),
        Some(&Value::Float(2.5))
    );
    assert_eq!(config.get("vector.dimensions"), Some(&Value::Integer(2)));
    assert_eq!(config.get("vector.hnsw.m"), Some(&Value::Integer(32)));
    assert_eq!(
        config.get("vector.hnsw.ef_construction"),
        Some(&Value::Integer(128))
    );
}

#[test]
fn semantic_index_ddl_show_drop_and_historical_definitions_are_versioned() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:IndexedDoc {text:'graph engine', embedding:[1.0,0.0]}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("create semantic index fixture");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX indexed_text FOR (n:IndexedDoc) ON EACH [n.text]",
        ExecutionOptions::default(),
    )
    .expect("create fulltext index");
    execute(
        &connection,
        "CREATE VECTOR INDEX indexed_embedding FOR (n:IndexedDoc) ON (n.embedding) OPTIONS {indexConfig:{`vector.dimensions`:2, `vector.similarity_function`:'euclidean', `vector.quantization.type`:'binary', `vector.default_search_expansion_factor`:2.5, `vector.hnsw.m`:32, `vector.hnsw.ef_construction`:128}}",
        ExecutionOptions::default(),
    )
    .expect("create vector index");
    let indexed_commit = branch_head(&connection, "main").expect("indexed commit");

    let (shown, _) = execute(
        &connection,
        "SHOW INDEXES YIELD name, type RETURN name, type ORDER BY name",
        ExecutionOptions::default(),
    )
    .expect("show semantic indexes");
    assert!(shown.contains(&vec![
        Value::String("indexed_embedding".to_owned()),
        Value::String("VECTOR".to_owned()),
    ]));
    assert!(shown.contains(&vec![
        Value::String("indexed_text".to_owned()),
        Value::String("FULLTEXT".to_owned()),
    ]));
    assert_vector_show_configuration(&connection);

    execute(
        &connection,
        "DROP INDEX indexed_text",
        ExecutionOptions::default(),
    )
    .expect("drop fulltext index");
    execute(
        &connection,
        "DROP INDEX indexed_embedding",
        ExecutionOptions::default(),
    )
    .expect("drop vector index");
    let (current, _) = execute(
        &connection,
        "SHOW INDEXES YIELD name RETURN name ORDER BY name",
        ExecutionOptions::default(),
    )
    .expect("show indexes after drop");
    assert!(!current.iter().any(|row| {
        matches!(row.first(), Some(Value::String(name)) if name == "indexed_text" || name == "indexed_embedding")
    }));

    let mut historical = ExecutionOptions::default();
    historical.snapshot = SnapshotSelector::Commit(indexed_commit.to_hex());
    let (historical_indexes, _) = execute(
        &connection,
        "SHOW INDEXES YIELD name, type RETURN name, type ORDER BY name",
        historical.clone(),
    )
    .expect("historical SHOW INDEXES");
    assert!(historical_indexes.contains(&vec![
        Value::String("indexed_embedding".to_owned()),
        Value::String("VECTOR".to_owned()),
    ]));
    assert!(historical_indexes.contains(&vec![
        Value::String("indexed_text".to_owned()),
        Value::String("FULLTEXT".to_owned()),
    ]));
    let (fulltext, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('indexed_text', 'graph') YIELD node RETURN node.text",
        historical.clone(),
    )
    .expect("historical fulltext definition remains queryable");
    assert_eq!(
        fulltext,
        vec![vec![Value::String("graph engine".to_owned())]]
    );
    let (vector, _) = execute(
        &connection,
        "MATCH (n:IndexedDoc) SEARCH n IN (VECTOR INDEX indexed_embedding FOR [1.0,0.0] LIMIT 1) RETURN n.text",
        historical,
    )
    .expect("historical vector definition remains queryable");
    assert_eq!(vector, vec![vec![Value::String("graph engine".to_owned())]]);
}

#[test]
fn load_csv_http_and_io_failures_preserve_atomic_rollback() {
    let connection = fresh_storage();
    let (uri, server) = one_shot_http_csv("name\nAlice\nBob\n");
    let query = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row RETURN row.name, file() ORDER BY row.name"
    );
    let (rows, _) = execute(&connection, &query, ExecutionOptions::default())
        .expect("load CSV over local HTTP");
    server.join().expect("join local HTTP fixture");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::String("Alice".to_owned()));
    assert_eq!(rows[1][0], Value::String("Bob".to_owned()));
    let Value::String(remote_file) = &rows[0][1] else {
        panic!("HTTP LOAD CSV file() must return a local path: {rows:?}");
    };
    assert!(PathBuf::from(remote_file).is_absolute());
    assert_ne!(remote_file, &uri);
    assert_eq!(rows[1][1], Value::String(remote_file.clone()));

    for (kind, source) in [
        (
            "file",
            "file:///definitely/not/a/lithograph/phase08/missing.csv",
        ),
        (
            "http",
            "http://127.0.0.1:1/unreachable.csv?token=phase08-secret",
        ),
        ("https", "https://127.0.0.1:1/unreachable.csv"),
    ] {
        let query = format!(
            "CREATE (:IoRollback {{kind:'{kind}'}}) WITH '{source}' AS source LOAD CSV FROM source AS row CREATE (:NeverImported {{value:row[0]}}) FINISH"
        );
        let error = execute(&connection, &query, ExecutionOptions::default())
            .expect_err("I/O failure must abort the ordinary mutation");
        assert_eq!(error.kind, lithograph_core::query::QueryErrorKind::Io);
        assert!(
            !error.message.contains("phase08-secret"),
            "LOAD CSV I/O errors must not expose URL credentials/query secrets: {}",
            error.message
        );
    }
    let (rows, _) = execute(
        &connection,
        "MATCH (n:IoRollback) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("verify I/O rollback");
    assert_eq!(rows, vec![vec![Value::Integer(0)]]);
}

#[test]
fn load_csv_mutation_streams_large_input_and_malformed_input_rolls_back() {
    let connection = fresh_storage();
    let mut large = String::from("value\n");
    for value in 0..5_000 {
        large.push_str(&format!("{value}\n"));
    }
    let (large_path, large_uri) = csv_fixture("large", &large);
    let query = format!(
        "LOAD CSV WITH HEADERS FROM '{large_uri}' AS row CREATE (:Imported {{value: toInteger(row.value)}}) FINISH"
    );
    execute(&connection, &query, ExecutionOptions::default()).expect("streaming csv mutation");
    let (rows, _) = execute(
        &connection,
        "MATCH (n:Imported) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("count imported nodes");
    assert_eq!(rows, vec![vec![Value::Integer(5_000)]]);
    fs::remove_file(large_path).expect("remove large csv fixture");

    let (bad_path, bad_uri) = csv_fixture("bad", "name\nGood\n\"unterminated\n");
    let bad_query = format!(
        "LOAD CSV WITH HEADERS FROM '{bad_uri}' AS row CREATE (:RejectedImport {{name: row.name}}) FINISH"
    );
    execute(&connection, &bad_query, ExecutionOptions::default())
        .expect_err("malformed csv must fail the whole ordinary mutation");
    let (rows, _) = execute(
        &connection,
        "MATCH (n:RejectedImport) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("verify rollback");
    assert_eq!(rows, vec![vec![Value::Integer(0)]]);
    fs::remove_file(bad_path).expect("remove malformed csv fixture");
}

#[path = "phase08_search_ingestion/load_csv_regressions.rs"]
mod load_csv_regressions;

#[test]
fn transaction_subquery_commits_once_per_mutating_batch_and_read_only_batches_do_not_commit() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let (rows, summary) = execute(
        &connection,
        "UNWIND [1,2,3,4,5] AS value CALL (value) { CREATE (:Batched {value:value}) } IN TRANSACTIONS OF 2 ROWS RETURN value ORDER BY value",
        ExecutionOptions::default(),
    )
    .expect("mutating transaction batches");
    assert_eq!(rows.len(), 5);
    assert_eq!(summary.counters.nodes_created, 5);
    assert_eq!(commit_count(&connection), before + 3);

    let before_read = commit_count(&connection);
    let (rows, _) = execute(
        &connection,
        "UNWIND [1,2,3,4] AS value CALL (value) { RETURN value * 2 AS doubled } IN TRANSACTIONS OF 2 ROWS RETURN doubled ORDER BY doubled",
        ExecutionOptions::default(),
    )
    .expect("read-only transaction batches");
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(2)],
            vec![Value::Integer(4)],
            vec![Value::Integer(6)],
            vec![Value::Integer(8)],
        ]
    );
    assert_eq!(commit_count(&connection), before_read);
}

#[test]
fn transaction_subquery_composes_with_next_without_leaking_inner_scope() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let (rows, _) = execute(
        &connection,
        "UNWIND [1,2] AS value CALL (value) { CREATE (:NextBatch {value:value}) } IN TRANSACTIONS OF 1 ROWS NEXT RETURN 1 AS result",
        ExecutionOptions::default(),
    )
    .expect("transaction query NEXT composition");
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(1)]]);
    assert_eq!(commit_count(&connection), before + 2);
}

#[test]
fn transaction_subquery_continue_rolls_back_only_the_failed_batch() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let (rows, _) = execute(
        &connection,
        "UNWIND [1,2,3] AS value CALL (value) { CREATE (n:BatchStatus {value:value}) SET n[CASE value WHEN 2 THEN 42 ELSE 'ok' END] = value } IN TRANSACTIONS OF 1 ROWS ON ERROR CONTINUE REPORT STATUS AS status RETURN value, status.committed AS committed ORDER BY value",
        ExecutionOptions::default(),
    )
    .expect("continue after one failed batch");
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Boolean(true)],
            vec![Value::Integer(2), Value::Boolean(false)],
            vec![Value::Integer(3), Value::Boolean(true)],
        ]
    );
    assert_eq!(commit_count(&connection), before + 2);
    let (count, _) = execute(
        &connection,
        "MATCH (n:BatchStatus) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("count durable successful batches");
    assert_eq!(count, vec![vec![Value::Integer(2)]]);
}

#[test]
fn transaction_subquery_fail_and_break_preserve_precise_partial_durability_and_status() {
    let connection = fresh_storage();
    let before_fail = commit_count(&connection);
    execute(
        &connection,
        "UNWIND [1,2,3] AS value CALL (value) { CREATE (n:FailBatch {value:value}) SET n[CASE value WHEN 2 THEN 42 ELSE 'ok' END] = value } IN TRANSACTIONS OF 1 ROWS FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("default FAIL stops at failed batch");
    assert_eq!(commit_count(&connection), before_fail + 1);
    let (count, _) = execute(
        &connection,
        "MATCH (n:FailBatch) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("count prior durable FAIL batch");
    assert_eq!(count, vec![vec![Value::Integer(1)]]);

    let before_break = commit_count(&connection);
    let (rows, _) = execute(
        &connection,
        "UNWIND [1,2,3] AS value CALL (value) { CREATE (n:BreakBatch {value:value}) SET n[CASE value WHEN 2 THEN 42 ELSE 'ok' END] = value } IN TRANSACTIONS OF 1 ROWS ON ERROR BREAK REPORT STATUS AS status RETURN value, status.started AS started, status.committed AS committed, status.transactionId AS transactionId, status.errorMessage AS errorMessage ORDER BY value",
        ExecutionOptions::default(),
    )
    .expect("BREAK status rows");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[0][1], Value::Boolean(true));
    assert_eq!(rows[0][2], Value::Boolean(true));
    assert!(matches!(rows[0][3], Value::String(_)));
    assert_eq!(rows[0][4], Value::Null);
    assert_eq!(rows[1][0], Value::Integer(2));
    assert_eq!(rows[1][1], Value::Boolean(true));
    assert_eq!(rows[1][2], Value::Boolean(false));
    assert!(matches!(rows[1][4], Value::String(_)));
    assert_eq!(rows[2][0], Value::Integer(3));
    assert_eq!(rows[2][1], Value::Boolean(false));
    assert_eq!(rows[2][2], Value::Boolean(false));
    assert_eq!(rows[2][3], Value::Null);
    assert_eq!(rows[2][4], Value::Null);
    assert_eq!(commit_count(&connection), before_break + 1);
}

#[test]
fn transaction_subquery_retry_recovers_from_transient_sqlite_busy() {
    let (path, connection) = fresh_file_storage("retry");
    let before = commit_count(&connection);
    let (ready_tx, ready_rx) = mpsc::channel();
    let lock_path = path.clone();
    let locker = thread::spawn(move || {
        let lock = Connection::open(lock_path).expect("open locking connection");
        lock.execute_batch("BEGIN IMMEDIATE")
            .expect("acquire SQLite write lock");
        ready_tx.send(()).expect("signal lock acquired");
        thread::sleep(Duration::from_millis(120));
        lock.execute_batch("COMMIT").expect("release SQLite lock");
    });
    ready_rx.recv().expect("wait for write lock");

    execute(
        &connection,
        "UNWIND [1] AS value CALL (value) { CREATE (:RetriedBatch {value:value}) } IN TRANSACTIONS OF 1 ROWS ON ERROR RETRY FOR 1 SEC THEN FAIL FINISH",
        ExecutionOptions::default(),
    )
    .expect("transient SQLITE_BUSY is retried");
    locker.join().expect("locking thread");
    assert_eq!(commit_count(&connection), before + 1);
    let (count, _) = execute(
        &connection,
        "MATCH (n:RetriedBatch) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("retry created node");
    assert_eq!(count, vec![vec![Value::Integer(1)]]);
    drop(connection);
    fs::remove_file(path).expect("remove retry database");
}

#[test]
fn transaction_batches_repin_latest_head_and_recompute_graph_view() {
    let connection = fresh_storage();
    let mut options = ExecutionOptions::default();
    options.graph_view.require_all_labels = ["Visible".to_owned()].into_iter().collect();
    let before = commit_count(&connection);
    let (rows, _) = execute(
        &connection,
        "UNWIND [1,2,3] AS value CALL (value) { CREATE (:Visible {value:value}) MATCH (n:Visible) RETURN count(n) AS visibleCount } IN TRANSACTIONS OF 1 ROWS RETURN value, visibleCount ORDER BY value",
        options,
    )
    .expect("ordered batches see earlier durable graph changes");
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Integer(1)],
            vec![Value::Integer(2), Value::Integer(2)],
            vec![Value::Integer(3), Value::Integer(3)],
        ]
    );
    assert_eq!(commit_count(&connection), before + 3);
}

#[test]
fn concurrent_transaction_syntax_uses_serial_commit_coordinator_without_false_stale_heads() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let mut options = ExecutionOptions::default();
    options.graph_view.require_all_labels = ["Visible".to_owned()].into_iter().collect();
    let (rows, _) = execute(
        &connection,
        "UNWIND [1,2,3] AS value CALL (value) { CREATE (:Visible {value:value}) MATCH (n:Visible) RETURN count(n) AS visibleCount } IN 2 CONCURRENT TRANSACTIONS OF 1 ROWS DISJOINT BY (value) RETURN value, visibleCount ORDER BY value",
        options,
    )
    .expect("concurrent transaction syntax");
    assert_eq!(rows.len(), 3);
    assert_eq!(commit_count(&connection), before + 3);
    let mut visible_counts = rows
        .iter()
        .map(|row| match row.get(1) {
            Some(Value::Integer(value)) => *value,
            other => panic!("expected visibleCount Integer, got {other:?}"),
        })
        .collect::<Vec<_>>();
    visible_counts.sort_unstable();
    assert_eq!(visible_counts, vec![1, 2, 3]);
}

#[test]
fn concurrent_transaction_options_validate_disjoint_and_concurrency_contract() {
    let connection = fresh_storage();
    for query in [
        "UNWIND [1,2] AS value CALL (value) { RETURN value AS innerValue } IN 2 CONCURRENT TRANSACTIONS DISJOINT BY AUTO RETURN innerValue ORDER BY innerValue",
        "UNWIND [1,2] AS value CALL (value) { RETURN value AS innerValue } IN 2 CONCURRENT TRANSACTIONS DISJOINT BY NONE RETURN innerValue ORDER BY innerValue",
    ] {
        let (rows, _) = execute(&connection, query, ExecutionOptions::default())
            .expect("supported concurrent DISJOINT mode");
        assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    }

    let error = prepare(
        &connection,
        "UNWIND [1] AS value CALL (value) { RETURN value } IN TRANSACTIONS DISJOINT BY (value) RETURN value",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect_err("DISJOINT BY without CONCURRENT must be rejected");
    assert_eq!(error.kind, lithograph_core::query::QueryErrorKind::Semantic);

    let zero = "UNWIND [1] AS value CALL (value) { RETURN value AS innerValue } IN 0 CONCURRENT TRANSACTIONS RETURN innerValue";
    let error = execute(&connection, zero, ExecutionOptions::default())
        .expect_err("zero concurrency must be rejected");
    assert_eq!(
        error.kind,
        lithograph_core::query::QueryErrorKind::InvalidArgument
    );

    let negative_literal = "UNWIND [1] AS value CALL (value) { RETURN value AS innerValue } IN -1 CONCURRENT TRANSACTIONS RETURN innerValue";
    let error = execute(&connection, negative_literal, ExecutionOptions::default())
        .expect_err("negative literal concurrency must be rejected");
    assert_eq!(error.kind, lithograph_core::query::QueryErrorKind::Semantic);

    let mut params = BTreeMap::new();
    params.insert("concurrency".to_owned(), Value::Integer(-1));
    let (rows, _) = execute_with_params(
        &connection,
        "UNWIND [1] AS value CALL (value) { RETURN value AS innerValue } IN $concurrency CONCURRENT TRANSACTIONS RETURN innerValue",
        params,
        ExecutionOptions::default(),
    )
    .expect("negative concurrency parameter must be accepted");
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn load_csv_in_transactions_streams_batches_and_preserves_prior_commits_on_late_csv_failure() {
    let connection = fresh_storage();
    let mut csv = String::from("name\n");
    for value in 0..250 {
        csv.push_str(&format!("person-{value}\n"));
    }
    let (path, uri) = csv_fixture("batched", &csv);
    let before = commit_count(&connection);
    let query = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row CALL (row) {{ CREATE (:CsvBatch {{name:row.name}}) }} IN TRANSACTIONS OF 100 ROWS FINISH"
    );
    execute(&connection, &query, ExecutionOptions::default()).expect("batched csv import");
    assert_eq!(commit_count(&connection), before + 3);
    let (count, _) = execute(
        &connection,
        "MATCH (n:CsvBatch) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("count batched csv nodes");
    assert_eq!(count, vec![vec![Value::Integer(250)]]);
    fs::remove_file(path).expect("remove batched csv fixture");

    let (bad_path, bad_uri) = csv_fixture("batched-bad", "name\nA\nB\n\"unterminated\n");
    let before_bad = commit_count(&connection);
    let bad_query = format!(
        "LOAD CSV WITH HEADERS FROM '{bad_uri}' AS row CALL (row) {{ CREATE (:CsvPartial {{name:row.name}}) }} IN TRANSACTIONS OF 1 ROWS FINISH"
    );
    execute(&connection, &bad_query, ExecutionOptions::default())
        .expect_err("late malformed csv fails the outer query");
    assert_eq!(commit_count(&connection), before_bad + 2);
    let (count, _) = execute(
        &connection,
        "MATCH (n:CsvPartial) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("prior csv batches remain durable");
    assert_eq!(count, vec![vec![Value::Integer(2)]]);
    fs::remove_file(bad_path).expect("remove malformed batched csv fixture");
}

#[test]
fn load_csv_in_transactions_applies_global_prefix_before_batching() {
    let connection = fresh_storage();
    let (path, uri) = csv_fixture("transaction-global-prefix", "value\n1\n2\n");
    let before = commit_count(&connection);
    let query = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row WITH count(row) AS total CALL (total) {{ CREATE (:CsvGlobalPrefix {{total:total}}) }} IN TRANSACTIONS OF 1 ROWS RETURN total"
    );
    let (rows, _) = execute(&connection, &query, ExecutionOptions::default())
        .expect("LOAD CSV transaction global prefix");
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    assert_eq!(commit_count(&connection), before + 1);
    let (stored, _) = execute(
        &connection,
        "MATCH (n:CsvGlobalPrefix) RETURN n.total",
        ExecutionOptions::default(),
    )
    .expect("query global-prefix batch result");
    assert_eq!(stored, vec![vec![Value::Integer(2)]]);
    fs::remove_file(path).expect("remove transaction-global-prefix CSV fixture");
}

#[test]
fn load_csv_in_transactions_break_marks_later_rows_not_started() {
    let connection = fresh_storage();
    let (path, uri) = csv_fixture("break", "name\ngood\nbad\nafter\n");
    let before = commit_count(&connection);
    let query = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row CALL (row) {{ CREATE (:CsvBreak {{name:row.name, probe:CASE row.name WHEN 'bad' THEN {{nested:1}} ELSE 'ok' END}}) }} IN TRANSACTIONS OF 1 ROWS ON ERROR BREAK REPORT STATUS AS status RETURN row.name, status.started, status.committed, status.errorMessage ORDER BY row.name"
    );
    let (rows, _) =
        execute(&connection, &query, ExecutionOptions::default()).expect("streaming BREAK status");
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter().any(|row| {
            row == &vec![
                Value::String("good".to_owned()),
                Value::Boolean(true),
                Value::Boolean(true),
                Value::Null,
            ]
        }),
        "rows={rows:?}"
    );
    assert!(rows.iter().any(|row| {
        matches!(row.as_slice(), [
            Value::String(value),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::String(_),
        ] if value == "bad")
    }));
    assert!(rows.iter().any(|row| {
        row == &vec![
            Value::String("after".to_owned()),
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Null,
        ]
    }));
    assert_eq!(commit_count(&connection), before + 1);
    let (count, _) = execute(
        &connection,
        "MATCH (n:CsvBreak) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("count CSV BREAK durable rows");
    assert_eq!(count, vec![vec![Value::Integer(1)]]);
    fs::remove_file(path).expect("remove BREAK csv fixture");
}

#[test]
fn transaction_subquery_allows_outer_write_after_batches_but_rejects_prior_outer_write() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    execute(
        &connection,
        "UNWIND [1,2] AS value CALL (value) { CREATE (:InnerBatch {value:value}) } IN TRANSACTIONS OF 1 ROWS WITH value CREATE (:AfterBatch {value:value}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("write after transaction batches");
    assert_eq!(commit_count(&connection), before + 3);

    let before_failure = commit_count(&connection);
    let error = execute(
        &connection,
        "UNWIND [3,4] AS value CALL (value) { CREATE (:DurableInner {value:value}) } IN TRANSACTIONS OF 1 ROWS WITH value CREATE (n:RolledBackOuter {value:value}) SET n.bad = {nested: 1} FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("outer suffix failure rolls back only outer suffix");
    assert_eq!(error.kind, lithograph_core::query::QueryErrorKind::Type);
    assert_eq!(commit_count(&connection), before_failure + 2);
    let (durable, _) = execute(
        &connection,
        "MATCH (n:DurableInner) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("inner commits survive suffix failure");
    assert_eq!(durable, vec![vec![Value::Integer(2)]]);
    let (rolled_back, _) = execute(
        &connection,
        "MATCH (n:RolledBackOuter) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("outer suffix rolled back");
    assert_eq!(rolled_back, vec![vec![Value::Integer(0)]]);

    let before_rejected = commit_count(&connection);
    execute(
        &connection,
        "CREATE (:BeforeTransaction) WITH 1 AS value CALL (value) { CREATE (:NeverBatched) } IN TRANSACTIONS OF 1 ROWS FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("transaction CALL after an outer write must be rejected");
    assert_eq!(commit_count(&connection), before_rejected);
}

#[test]
fn transaction_subquery_supports_row_singular_next_and_conditional_composition() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let (rows, _) = execute(
        &connection,
        "UNWIND [1,2] AS value CALL (value) { CREATE (:NextBatch {value:value}) } IN TRANSACTIONS OF 1 ROW NEXT RETURN 7 AS result",
        ExecutionOptions::default(),
    )
    .expect("ROW singular and NEXT composition");
    assert_eq!(rows, vec![vec![Value::Integer(7)], vec![Value::Integer(7)]]);
    assert_eq!(commit_count(&connection), before + 2);

    let before_conditional = commit_count(&connection);
    let (rows, _) = execute(
        &connection,
        "WHEN true THEN { UNWIND [3] AS value CALL (value) { CREATE (:ConditionalBatch {value:value}) } IN TRANSACTIONS RETURN value } ELSE { RETURN 0 AS value }",
        ExecutionOptions::default(),
    )
    .expect("conditional transaction composition");
    assert_eq!(rows, vec![vec![Value::Integer(3)]]);
    assert_eq!(commit_count(&connection), before_conditional + 1);

    let error = prepare(
        &connection,
        "UNWIND [1] AS value CALL (value) { RETURN value } IN TRANSACTIONS REPORT STATUS AS status RETURN status",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect_err("REPORT STATUS without non-failing error mode must be rejected");
    assert_eq!(error.kind, lithograph_core::query::QueryErrorKind::Semantic);
}

#[test]
fn semantic_indexes_support_relationships_and_historical_snapshot_isolation() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (a:Person {name:'A'}), (b:Person {name:'B'}), (c:Person {name:'C'}), (a)-[:SIMILAR {text:'graph database', embedding:vector([1.0,0.0],2,FLOAT64)}]->(b), (a)-[:SIMILAR {text:'other topic', embedding:vector([0.0,1.0],2,FLOAT64)}]->(c)",
        ExecutionOptions::default(),
    )
    .expect("create relationship search data");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX relationship_text FOR ()-[r:SIMILAR]-() ON EACH [r.text]",
        ExecutionOptions::default(),
    )
    .expect("create relationship fulltext index");
    execute(
        &connection,
        "CREATE VECTOR INDEX relationship_embedding FOR ()-[r:SIMILAR]-() ON (r.embedding) OPTIONS {indexConfig:{`vector.dimensions`:2}}",
        ExecutionOptions::default(),
    )
    .expect("create relationship vector index");
    let historical = branch_head(&connection, "main").expect("historical head");

    let (fulltext, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryRelationships('relationship_text', 'graph') YIELD relationship, score RETURN relationship.text, score",
        ExecutionOptions::default(),
    )
    .expect("relationship fulltext query");
    assert_eq!(fulltext.len(), 1);
    assert_eq!(fulltext[0][0], Value::String("graph database".to_owned()));

    let (vector, _) = execute(
        &connection,
        "MATCH ()-[r:SIMILAR]->() SEARCH r IN (VECTOR INDEX relationship_embedding FOR vector([1.0,0.0],2,FLOAT64) LIMIT 1) RETURN r.text",
        ExecutionOptions::default(),
    )
    .expect("relationship vector search");
    assert_eq!(
        vector,
        vec![vec![Value::String("graph database".to_owned())]]
    );

    execute(
        &connection,
        "MATCH (a:Person {name:'A'}), (c:Person {name:'C'}) CREATE (a)-[:SIMILAR {text:'graph newest', embedding:vector([1.0,0.0],2,FLOAT64)}]->(c) FINISH",
        ExecutionOptions::default(),
    )
    .expect("add current-head relationship");

    let (current_text, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryRelationships('relationship_text', 'graph') YIELD relationship RETURN relationship.text ORDER BY relationship.text",
        ExecutionOptions::default(),
    )
    .expect("current fulltext");
    assert_eq!(current_text.len(), 2);

    let mut historical_options = ExecutionOptions::default();
    historical_options.snapshot = SnapshotSelector::Commit(historical.to_hex());
    let (historical_text, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryRelationships('relationship_text', 'graph') YIELD relationship RETURN relationship.text",
        historical_options.clone(),
    )
    .expect("historical fulltext");
    assert_eq!(
        historical_text,
        vec![vec![Value::String("graph database".to_owned())]]
    );
    let (historical_vector, _) = execute(
        &connection,
        "MATCH ()-[r:SIMILAR]->() SEARCH r IN (VECTOR INDEX relationship_embedding FOR vector([1.0,0.0],2,FLOAT64) LIMIT 10) RETURN r.text ORDER BY r.text",
        historical_options,
    )
    .expect("historical vector search");
    assert_eq!(historical_vector.len(), 2);
    assert!(
        !historical_vector
            .iter()
            .any(|row| { row[0] == Value::String("graph newest".to_owned()) })
    );
}

#[test]
fn vector_search_supports_numeric_lists_and_enforces_search_filter_contract() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:ListVector {name:'A', lang:'en', hidden:'x', embedding:[1.0,0.0]}), (:ListVector {name:'B', lang:'fr', embedding:[0.8,0.2]}), (:ListVector {name:'OtherDimension', lang:'en', embedding:[1.0,0.0,0.0]})",
        ExecutionOptions::default(),
    )
    .expect("create list vectors");
    execute(
        &connection,
        "CREATE VECTOR INDEX list_embedding FOR (n:ListVector) ON (n.embedding) WITH [n.lang]",
        ExecutionOptions::default(),
    )
    .expect("create dimensionless list vector index");

    let (rows, _) = execute(
        &connection,
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] WHERE n.lang IN ['en'] LIMIT 10) SCORE AS score RETURN n.name, score ORDER BY n.name",
        ExecutionOptions::default(),
    )
    .expect("LIST query vector and indexed LIST property");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("A".to_owned()));

    let (null_match, _) = execute(
        &connection,
        "WITH null AS q MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR q LIMIT 10) RETURN n.name",
        ExecutionOptions::default(),
    )
    .expect("null SEARCH query vector in MATCH");
    assert!(null_match.is_empty());
    let (null_optional, _) = execute(
        &connection,
        "WITH null AS q OPTIONAL MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR q LIMIT 10) RETURN n.name",
        ExecutionOptions::default(),
    )
    .expect("null SEARCH query vector in OPTIONAL MATCH");
    assert_eq!(null_optional, vec![vec![Value::Null]]);

    let (zero_limit, _) = execute(
        &connection,
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] LIMIT 0) RETURN n.name",
        ExecutionOptions::default(),
    )
    .expect("SEARCH LIMIT zero");
    assert!(zero_limit.is_empty());

    let mut limit_params = BTreeMap::new();
    limit_params.insert("limit".to_owned(), Value::Integer(1));
    let (parameter_limit, _) = execute_with_params(
        &connection,
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] LIMIT $limit) RETURN n.name",
        limit_params,
        ExecutionOptions::default(),
    )
    .expect("SEARCH LIMIT parameter");
    assert_eq!(parameter_limit.len(), 1);

    for query in [
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] LIMIT -1) RETURN n",
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] LIMIT 1.0) RETURN n",
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] LIMIT null) RETURN n",
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] LIMIT 2147483648) RETURN n",
    ] {
        execute(&connection, query, ExecutionOptions::default())
            .expect_err("invalid SEARCH LIMIT must fail");
    }

    for query in [
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] WHERE n.hidden = 'x' LIMIT 1) RETURN n",
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] WHERE n.lang = 'en' OR n.lang = 'fr' LIMIT 1) RETURN n",
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] WHERE n.lang > 'a' AND n.lang >= 'b' LIMIT 1) RETURN n",
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] WHERE n.lang = 'en' AND n.lang > 'a' LIMIT 1) RETURN n",
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] WHERE n.lang = point({x:1.0,y:2.0}) LIMIT 1) RETURN n",
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] WHERE n.lang = vector([1.0,2.0],2,FLOAT64) LIMIT 1) RETURN n",
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] WHERE n.lang = ['en'] LIMIT 1) RETURN n",
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR n.embedding LIMIT 1) RETURN n",
        "MATCH (n:ListVector)-[r]->() SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] LIMIT 1) RETURN n",
        "MATCH p = (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] LIMIT 1) RETURN n",
        "MATCH (:Other)-[]->(n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] LIMIT 1) RETURN n",
    ] {
        execute(&connection, query, ExecutionOptions::default())
            .expect_err("invalid SEARCH shape/filter must fail");
    }

    let mut params = BTreeMap::new();
    params.insert(
        "badFilter".to_owned(),
        Value::List(vec![Value::String("en".to_owned())]),
    );
    let error = execute_with_params(
        &connection,
        "MATCH (n:ListVector) SEARCH n IN (VECTOR INDEX list_embedding FOR [1.0,0.0] WHERE n.lang = $badFilter LIMIT 1) RETURN n",
        params,
        ExecutionOptions::default(),
    )
    .expect_err("runtime SEARCH comparison filter value must enforce the index filter type contract");
    assert_eq!(error.kind, lithograph_core::query::QueryErrorKind::Type);

    let error = execute(
        &connection,
        "MATCH ()-[r]->() SEARCH r IN (VECTOR INDEX list_embedding FOR [1.0,0.0] LIMIT 1) RETURN r",
        ExecutionOptions::default(),
    )
    .expect_err("node vector index cannot bind a relationship SEARCH variable");
    assert_eq!(error.kind, lithograph_core::query::QueryErrorKind::Semantic);
}
