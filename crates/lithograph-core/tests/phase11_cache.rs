use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::performance;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryErrorKind, QuerySummary, QueryType, prepare,
};
use lithograph_core::storage::{
    STORAGE_FORMAT, branch_head, create_storage_schema, initialize_root, integrity_check,
    structural_integrity_issues,
};
use rusqlite::{Connection, params};
use tempfile::NamedTempFile;

fn initialize_database(path: &std::path::Path) -> Connection {
    let connection = Connection::open(path).expect("open Phase 11 database");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
                 id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, \
                 database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);",
        )
        .expect("metadata");
    connection
        .execute(
            "INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format) \
             VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000011', ?1)",
            params![STORAGE_FORMAT],
        )
        .expect("metadata row");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn execute(
    connection: &Connection,
    query: &str,
) -> Result<(Vec<Vec<Value>>, QuerySummary), lithograph_core::query::QueryError> {
    execute_options(connection, query, ExecutionOptions::default())
}

fn execute_options(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), lithograph_core::query::QueryError> {
    let prepared = prepare(connection, query, BTreeMap::new(), options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 17)?;
        rows.extend(batch.rows);
        if batch.done {
            return Ok((rows, cursor.complete(connection)?));
        }
    }
}

fn clear_query_local_index_cache(connection: &Connection) {
    connection
        .execute_batch(
            "DROP VIEW IF EXISTS temp._lithograph_standard_index_cache;\
             DROP TABLE IF EXISTS temp._lithograph_standard_index_cache_local;\
             DROP TABLE IF EXISTS temp._lithograph_standard_index_cache_meta;\
             DROP TABLE IF EXISTS temp._lithograph_standard_index_cache_config;\
             DROP TABLE IF EXISTS temp._lithograph_standard_index_changed_owners;",
        )
        .expect("drop query-local Standard Index cache");
}

#[test]
fn current_schema_is_structurally_exact() {
    let file = NamedTempFile::new().expect("temporary database");
    let connection = initialize_database(file.path());
    assert!(
        structural_integrity_issues(&connection)
            .expect("structural integrity")
            .is_empty()
    );
}

#[test]
fn range_generation_survives_reopen_and_integrity_detects_offline_payload_damage() {
    let file = NamedTempFile::new().expect("temporary database");
    let path = file.path().to_path_buf();
    drop(file);
    let connection = initialize_database(&path);
    execute(
        &connection,
        "UNWIND range(1,100) AS value CREATE (:Metric {value:value}) FINISH",
    )
    .expect("seed metrics");
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
    )
    .expect("type proof");
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    )
    .expect("range index");
    let anchor = branch_head(&connection, "main").expect("index anchor");
    let generation: (i64, i64, i64) = connection
        .query_row(
            "SELECT generation_id, indexed_entities, entry_count \
             FROM main._lithograph_index_generations \
             WHERE anchor_commit = ?1 AND complete = 1",
            [anchor.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("complete generation");
    assert_eq!(generation.1, 100);
    assert_eq!(generation.2, 100);
    drop(connection);

    let connection = Connection::open(&path).expect("reopen database");
    performance::reset();
    performance::set_enabled(true);
    let (rows, _) = execute(
        &connection,
        "MATCH (n:Metric) WHERE n.value = 42 RETURN n.value",
    )
    .expect("reopened persistent seek");
    let counters = performance::snapshot();
    performance::set_enabled(false);
    assert_eq!(rows, vec![vec![Value::Integer(42)]]);
    assert_eq!(counters.standard_index_builds, 0);

    connection
        .execute(
            "DELETE FROM main._lithograph_index_entries \
             WHERE generation_id = ?1 AND owner_id = (\
                 SELECT min(owner_id) FROM main._lithograph_index_entries WHERE generation_id = ?1\
             )",
            [generation.0],
        )
        .expect("damage one derived entry");
    let issues = integrity_check(&connection).expect("integrity after derived damage");
    assert!(
        issues
            .iter()
            .any(|issue| issue.code == "index_generation.entry_count"),
        "explicit integrity must detect single-entry persistent payload damage: {issues:?}"
    );
    connection
        .execute_batch(
            "DROP VIEW IF EXISTS temp._lithograph_standard_index_cache;\
             DROP TABLE IF EXISTS temp._lithograph_standard_index_cache_local;\
             DROP TABLE IF EXISTS temp._lithograph_standard_index_cache_meta;\
             DROP TABLE IF EXISTS temp._lithograph_standard_index_cache_config;",
        )
        .expect("drop query-local cache");
    performance::reset();
    performance::set_enabled(true);
    let (rows, _) = execute(
        &connection,
        "MATCH (n:Metric) WHERE n.value = 42 RETURN n.value",
    )
    .expect("unaffected persistent seek after offline damage");
    let counters = performance::snapshot();
    performance::set_enabled(false);
    assert_eq!(rows, vec![vec![Value::Integer(42)]]);
    assert_eq!(
        counters.standard_index_builds, 0,
        "ordinary reads must not rescan the complete persistent payload solely to detect offline tampering"
    );
}

#[test]
fn persistent_generation_metadata_states_fall_back_without_becoming_correctness_source() {
    let file = NamedTempFile::new().expect("temporary database");
    let path = file.path().to_path_buf();
    drop(file);
    let connection = initialize_database(&path);
    execute(
        &connection,
        "UNWIND range(1,20) AS value CREATE (:Metric {value:value}) FINISH",
    )
    .expect("seed metrics");
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
    )
    .expect("type proof");
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    )
    .expect("range index");
    let generation_id: i64 = connection
        .query_row(
            "SELECT generation_id FROM main._lithograph_index_generations WHERE complete = 1",
            [],
            |row| row.get(0),
        )
        .expect("generation id");

    let assert_fallback = |connection: &Connection| {
        clear_query_local_index_cache(connection);
        performance::reset();
        performance::set_enabled(true);
        let (rows, _) = execute(
            connection,
            "MATCH (n:Metric) WHERE n.value = 10 RETURN n.value",
        )
        .expect("fallback seek");
        let counters = performance::snapshot();
        performance::set_enabled(false);
        assert_eq!(rows, vec![vec![Value::Integer(10)]]);
        assert_eq!(counters.standard_index_builds, 1);
    };

    connection
        .execute(
            "UPDATE main._lithograph_index_generations SET encoding_version = 99 WHERE generation_id = ?1",
            [generation_id],
        )
        .expect("damage encoding version");
    let issues = integrity_check(&connection).expect("encoding integrity");
    assert!(
        issues
            .iter()
            .any(|issue| issue.code == "index_generation.encoding_version")
    );
    assert_fallback(&connection);

    connection
        .execute(
            "UPDATE main._lithograph_index_generations SET encoding_version = 1, complete = 0 WHERE generation_id = ?1",
            [generation_id],
        )
        .expect("mark generation incomplete");
    let issues = integrity_check(&connection).expect("incomplete integrity");
    assert!(
        issues
            .iter()
            .any(|issue| issue.code == "index_generation.incomplete")
    );
    assert_fallback(&connection);

    connection
        .execute(
            "DELETE FROM main._lithograph_index_entries WHERE generation_id = ?1",
            [generation_id],
        )
        .expect("delete generation payload");
    connection
        .execute(
            "DELETE FROM main._lithograph_index_generations WHERE generation_id = ?1",
            [generation_id],
        )
        .expect("delete generation manifest");
    assert!(
        integrity_check(&connection)
            .expect("missing cache integrity")
            .is_empty(),
        "absence of derived generation must be legal"
    );
    assert_fallback(&connection);
}

#[test]
fn all_standard_index_families_reuse_persistent_generations_after_reopen() {
    let file = NamedTempFile::new().expect("temporary database");
    let path = file.path().to_path_buf();
    drop(file);
    let connection = initialize_database(&path);
    execute(
        &connection,
        "CREATE (a:Person {name:'Alice', age:1, score:8, location:point({x:1, y:1})}), \
         (b:Person {name:'Bob', age:2, score:10, location:point({x:2, y:2})}), \
         (c:Person {name:'Carol', age:3, score:12, location:point({x:3, y:3})}), \
         (a)-[:ROUTE {name:'alpha', distance:5, location:point({x:1, y:1})}]->(b), \
         (b)-[:ROUTE {name:'beta', distance:15, location:point({x:2, y:2})}]->(c), \
         (c)-[:ROUTE {name:'gamma', distance:25, location:point({x:3, y:3})}]->(a) FINISH",
    )
    .expect("seed indexed graph");
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { \
         (:Person => {name :: STRING, age :: INTEGER, score :: INTEGER, location :: POINT}), \
         ()-[r:ROUTE => {name :: STRING, distance :: INTEGER, location :: POINT}]->() }",
    )
    .expect("typed graph schema");
    for ddl in [
        "CREATE LOOKUP INDEX relationship_lookup FOR ()-[r]-() ON EACH type(r)",
        "CREATE RANGE INDEX person_age_score FOR (n:Person) ON (n.age, n.score)",
        "CREATE TEXT INDEX person_name FOR (n:Person) ON (n.name)",
        "CREATE POINT INDEX person_location FOR (n:Person) ON (n.location)",
        "CREATE RANGE INDEX route_distance FOR ()-[r:ROUTE]-() ON (r.distance)",
        "CREATE TEXT INDEX route_name FOR ()-[r:ROUTE]-() ON (r.name)",
        "CREATE POINT INDEX route_location FOR ()-[r:ROUTE]-() ON (r.location)",
    ] {
        execute(&connection, ddl).unwrap_or_else(|error| panic!("DDL {ddl}: {error}"));
    }
    drop(connection);

    let connection = Connection::open(&path).expect("reopen database");
    performance::reset();
    performance::set_enabled(true);
    let cases = [
        (
            "MATCH (n:Person) WHERE n.age >= 2 AND n.score <= 10 RETURN n.name ORDER BY n.name",
            vec![vec![Value::String("Bob".to_owned())]],
        ),
        (
            "MATCH (n:Person) WHERE n.name STARTS WITH 'Al' RETURN n.name ORDER BY n.name",
            vec![vec![Value::String("Alice".to_owned())]],
        ),
        (
            "MATCH (n:Person) WHERE point.withinBBox(n.location, point({x:1.5,y:1.5}), point({x:3.5,y:3.5})) RETURN n.name ORDER BY n.name",
            vec![
                vec![Value::String("Bob".to_owned())],
                vec![Value::String("Carol".to_owned())],
            ],
        ),
        (
            "MATCH ()-[r:ROUTE]->() RETURN r.distance ORDER BY r.distance",
            vec![
                vec![Value::Integer(5)],
                vec![Value::Integer(15)],
                vec![Value::Integer(25)],
            ],
        ),
        (
            "MATCH ()-[r:ROUTE]->() WHERE r.distance < 20 RETURN r.distance ORDER BY r.distance",
            vec![vec![Value::Integer(5)], vec![Value::Integer(15)]],
        ),
        (
            "MATCH ()-[r:ROUTE]->() WHERE r.name CONTAINS 'mm' RETURN r.name ORDER BY r.name",
            vec![vec![Value::String("gamma".to_owned())]],
        ),
        (
            "MATCH ()-[r:ROUTE]->() WHERE point.withinBBox(r.location, point({x:0.5,y:0.5}), point({x:2.5,y:2.5})) RETURN r.distance ORDER BY r.distance",
            vec![vec![Value::Integer(5)], vec![Value::Integer(15)]],
        ),
    ];
    for (query, expected) in cases {
        let (rows, _) = execute(&connection, query)
            .unwrap_or_else(|error| panic!("persistent query {query}: {error}"));
        assert_eq!(rows, expected, "{query}");
    }
    let counters = performance::snapshot();
    performance::set_enabled(false);
    assert_eq!(
        counters.standard_index_builds, 0,
        "reopened persistent-ready Standard Index reads must not rebuild"
    );
}

#[test]
fn ancestor_generation_reads_apply_only_changed_owner_overlay() {
    let file = NamedTempFile::new().expect("temporary database");
    let path = file.path().to_path_buf();
    drop(file);
    let connection = initialize_database(&path);
    execute(
        &connection,
        "UNWIND range(1,100) AS value CREATE (:Metric {value:value}) FINISH",
    )
    .expect("seed metrics");
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
    )
    .expect("type proof");
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    )
    .expect("range index");
    let generation_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_index_generations WHERE complete = 1",
            [],
            |row| row.get(0),
        )
        .expect("generation count");
    assert_eq!(generation_count, 1);

    execute(
        &connection,
        "MATCH (n:Metric) WHERE n.value = 42 SET n.value = 420 FINISH",
    )
    .expect("change indexed owner");
    execute(&connection, "CREATE (:Metric {value:101}) FINISH").expect("new indexed owner");
    execute(
        &connection,
        "MATCH (n:Metric) WHERE n.value = 5 REMOVE n.value FINISH",
    )
    .expect("remove indexed value");
    execute(
        &connection,
        "MATCH (n:Metric) WHERE n.value = 6 DELETE n FINISH",
    )
    .expect("delete indexed owner");

    performance::reset();
    performance::set_enabled(true);
    let (old_rows, _) = execute(
        &connection,
        "MATCH (n:Metric) WHERE n.value = 42 RETURN n.value",
    )
    .expect("old value seek");
    let (new_rows, _) = execute(
        &connection,
        "MATCH (n:Metric) WHERE n.value IN [420,101] RETURN n.value ORDER BY n.value",
    )
    .expect("new value seek");
    let (removed_rows, _) = execute(
        &connection,
        "MATCH (n:Metric) WHERE n.value = 5 RETURN n.value",
    )
    .expect("removed value seek");
    let (deleted_rows, _) = execute(
        &connection,
        "MATCH (n:Metric) WHERE n.value = 6 RETURN n.value",
    )
    .expect("deleted owner seek");
    let (range_rows, _) = execute(
        &connection,
        "MATCH (n:Metric) WHERE n.value >= 40 AND n.value < 45 RETURN n.value ORDER BY n.value",
    )
    .expect("bounded Range seek over ancestor generation + changed-owner overlay");
    let counters = performance::snapshot();
    performance::set_enabled(false);
    assert!(old_rows.is_empty());
    assert_eq!(
        new_rows,
        vec![vec![Value::Integer(101)], vec![Value::Integer(420)]]
    );
    assert!(removed_rows.is_empty());
    assert!(deleted_rows.is_empty());
    assert_eq!(
        range_rows,
        [40, 41, 43, 44]
            .into_iter()
            .map(|value| vec![Value::Integer(value)])
            .collect::<Vec<_>>()
    );
    assert_eq!(counters.standard_index_builds, 0);
    assert_eq!(
        counters.resolved_state_builds, 5,
        "each ancestor-generation query must resolve Snapshot state exactly once"
    );
    let generation_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_index_generations WHERE complete = 1",
            [],
            |row| row.get(0),
        )
        .expect("generation count after delta");
    assert_eq!(generation_count, 1);
}

#[test]
fn index_rebuild_replaces_generation_without_creating_commit() {
    let file = NamedTempFile::new().expect("temporary database");
    let path = file.path().to_path_buf();
    drop(file);
    let connection = initialize_database(&path);
    execute(
        &connection,
        "UNWIND range(1,25) AS value CREATE (:Metric {value:value}) FINISH",
    )
    .expect("seed metrics");
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
    )
    .expect("type proof");
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    )
    .expect("range index");
    let before = branch_head(&connection, "main").expect("head before rebuild");
    let old_generation: i64 = connection
        .query_row(
            "SELECT generation_id FROM main._lithograph_index_generations \
             WHERE anchor_commit = ?1 AND complete = 1",
            [before.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("generation before rebuild");
    connection
        .execute(
            "DELETE FROM main._lithograph_index_entries WHERE generation_id = ?1",
            [old_generation],
        )
        .expect("delete derived payload");

    let (rows, summary) = execute(
        &connection,
        "CALL lithograph.index.rebuild('metric_value', 'branch/main') \
         YIELD name, commit, indexedEntities RETURN name, commit, indexedEntities",
    )
    .expect("explicit index rebuild");
    assert_eq!(summary.query_type, QueryType::Version);
    assert_eq!(summary.commit, None);
    assert_eq!(summary.counters, Default::default());
    assert_eq!(
        rows,
        vec![vec![
            Value::String("metric_value".to_owned()),
            Value::String(format!("commit/{}", before.to_hex())),
            Value::Integer(25),
        ]]
    );
    assert_eq!(
        branch_head(&connection, "main").expect("head after rebuild"),
        before
    );
    let generation: (i64, i64, i64) = connection
        .query_row(
            "SELECT generation_id, indexed_entities, entry_count \
             FROM main._lithograph_index_generations \
             WHERE anchor_commit = ?1 AND complete = 1",
            [before.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("generation after rebuild");
    assert_eq!((generation.1, generation.2), (25, 25));
    let payload_rows: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_index_entries WHERE generation_id = ?1",
            [generation.0],
            |row| row.get(0),
        )
        .expect("rebuilt payload rows");
    assert_eq!(payload_rows, 25);
}

fn seed_dual_metric_indexes(connection: &Connection) {
    for query in [
        "CREATE (:Metric {value:1, other:101}) FINISH",
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
        "CREATE CONSTRAINT metric_other_type FOR (n:Metric) REQUIRE n.other IS :: INTEGER",
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
        "CREATE RANGE INDEX metric_other FOR (n:Metric) ON (n.other)",
        "CALL lithograph.index.rebuild('metric_value', 'branch/main') YIELD name RETURN name",
    ] {
        execute(connection, query).unwrap_or_else(|error| panic!("{query}: {error}"));
    }
}

fn cached_generation_for_snapshot(
    connection: &Connection,
    snapshot: lithograph_core::storage::HashId,
    index_name: &str,
) -> i64 {
    connection
        .query_row(
            "SELECT generation_id FROM temp._lithograph_standard_index_cache_meta \
             WHERE snapshot_hash=?1 AND index_name=?2 AND complete=1",
            rusqlite::params![snapshot.as_bytes().as_slice(), index_name],
            |row| row.get(0),
        )
        .unwrap_or_else(|error| panic!("cached generation for {index_name}: {error}"))
}

#[test]
fn persistent_reader_rebinds_after_cross_connection_generation_replacement() {
    let file = NamedTempFile::new().expect("temporary database");
    let path = file.path().to_path_buf();
    drop(file);
    let reader = initialize_database(&path);
    seed_dual_metric_indexes(&reader);
    let (rows, _) = execute(&reader, "MATCH (n:Metric) WHERE n.value = 1 RETURN n.value")
        .expect("prime reader TEMP binding");
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    let current_head = branch_head(&reader, "main").expect("current head");
    let cached_generation = cached_generation_for_snapshot(&reader, current_head, "metric_value");
    let definition_hash: Vec<u8> = reader
        .query_row(
            "SELECT definition_hash FROM main._lithograph_index_generations \
             WHERE generation_id = ?1",
            [cached_generation],
            |row| row.get(0),
        )
        .expect("cached definition hash");

    let writer = Connection::open(&path).expect("writer connection");
    execute(
        &writer,
        "CALL lithograph.index.rebuild('metric_value', 'branch/main') YIELD name RETURN name",
    )
    .expect("replace value generation from writer connection");
    let current_generation: i64 = writer
        .query_row(
            "SELECT generation_id FROM main._lithograph_index_generations AS generations \
             JOIN main._lithograph_branches AS branches ON branches.commit_id=generations.anchor_commit \
             WHERE branches.name='main' AND generations.complete=1 \
               AND generations.definition_hash = ?1",
            [&definition_hash],
            |row| row.get(0),
        )
        .expect("current value generation");
    assert_ne!(cached_generation, current_generation);
    assert_eq!(
        cached_generation_for_snapshot(&reader, current_head, "metric_value"),
        cached_generation
    );

    performance::reset();
    performance::set_enabled(true);
    let (rows, _) = execute(&reader, "MATCH (n:Metric) WHERE n.value = 1 RETURN n.value")
        .expect("reader rebind after writer rebuild");
    let counters = performance::snapshot();
    performance::set_enabled(false);
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    assert_eq!(counters.standard_index_builds, 0);
    let rebound_generation = cached_generation_for_snapshot(&reader, current_head, "metric_value");
    assert_ne!(rebound_generation, cached_generation);
    assert_eq!(
        reader
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM main._lithograph_index_generations \
                 WHERE generation_id=?1 AND definition_hash=?2 AND complete=1)",
                rusqlite::params![rebound_generation, &definition_hash],
                |row| row.get::<_, i64>(0),
            )
            .expect("rebound generation manifest"),
        1
    );
    assert!(current_generation > cached_generation);
}

#[test]
fn index_rebuild_public_contract_and_option_boundaries() {
    let file = NamedTempFile::new().expect("temporary database");
    let path = file.path().to_path_buf();
    drop(file);
    let connection = initialize_database(&path);
    execute(&connection, "CREATE (:Metric {value:1}) FINISH").expect("seed metric");
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
    )
    .expect("type proof");
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    )
    .expect("range index");

    let (procedures, _) = execute(&connection, "SHOW PROCEDURES").expect("SHOW PROCEDURES");
    assert!(procedures.iter().any(|row| {
        matches!(row.first(), Some(Value::String(name)) if name == "lithograph.index.rebuild")
    }));
    let generation_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_index_generations",
            [],
            |row| row.get(0),
        )
        .expect("generation count before EXPLAIN");
    let (plan, _) = execute(
        &connection,
        "EXPLAIN CALL lithograph.index.rebuild('metric_value', 'branch/main') \
         YIELD name RETURN name",
    )
    .expect("EXPLAIN rebuild");
    assert_eq!(plan.len(), 1);
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM main._lithograph_index_generations",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("generation count after EXPLAIN"),
        generation_count,
        "EXPLAIN must not publish a generation"
    );

    let head = branch_head(&connection, "main").expect("current head");
    let at = ExecutionOptions::parse_text(&format!(r#"{{"at":"commit/{}"}}"#, head.to_hex()))
        .expect("historical options");
    let error = execute_options(
        &connection,
        "CALL lithograph.index.rebuild('metric_value', 'branch/main') YIELD name RETURN name",
        at,
    )
    .expect_err("options.at rebuild must fail");
    assert_eq!(error.kind, QueryErrorKind::ReadOnlySnapshot);

    for options in [
        r#"{"branch":"main"}"#,
        r#"{"author":"phase11"}"#,
        r#"{"message":"phase11"}"#,
        r#"{"graphView":{"requireAllLabels":["Metric"]}}"#,
    ] {
        let error = execute_options(
            &connection,
            "CALL lithograph.index.rebuild('metric_value', 'branch/main') YIELD name RETURN name",
            ExecutionOptions::parse_text(options).expect("options"),
        )
        .expect_err("unsupported rebuild options must fail");
        assert_eq!(error.kind, QueryErrorKind::InvalidArgument, "{options}");
    }

    let error = execute(
        &connection,
        "CALL lithograph.index.rebuild('missing', 'branch/main') YIELD name RETURN name",
    )
    .expect_err("missing index must fail");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    let error = execute(
        &connection,
        "CALL lithograph.index.rebuild(1, 'branch/main') YIELD name RETURN name",
    )
    .expect_err("non-string index name must fail");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);

    execute(
        &connection,
        "CREATE LOOKUP INDEX node_lookup FOR (n) ON EACH labels(n)",
    )
    .expect("Node Lookup index");
    let error = execute(
        &connection,
        "CALL lithograph.index.rebuild('node_lookup', 'branch/main') YIELD name RETURN name",
    )
    .expect_err("Node Lookup rebuild must be rejected");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
}

#[test]
fn persistent_generation_retention_keeps_at_most_two_anchors() {
    let file = NamedTempFile::new().expect("temporary database");
    let path = file.path().to_path_buf();
    drop(file);
    let connection = initialize_database(&path);
    execute(
        &connection,
        "UNWIND range(1,20) AS value CREATE (:Metric {value:value}) FINISH",
    )
    .expect("seed metrics");
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
    )
    .expect("type proof");
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    )
    .expect("range index");
    let first = branch_head(&connection, "main").expect("first anchor");

    execute(&connection, "CREATE (:Metric {value:21}) FINISH").expect("second state");
    let second = branch_head(&connection, "main").expect("second anchor");
    execute(
        &connection,
        "CALL lithograph.index.rebuild('metric_value', 'branch/main') YIELD name RETURN name",
    )
    .expect("second generation");

    execute(&connection, "CREATE (:Metric {value:22}) FINISH").expect("third state");
    let third = branch_head(&connection, "main").expect("third anchor");
    execute(
        &connection,
        "CALL lithograph.index.rebuild('metric_value', 'branch/main') YIELD name RETURN name",
    )
    .expect("third generation");

    let anchors = connection
        .prepare(
            "SELECT anchor_commit FROM main._lithograph_index_generations \
             WHERE complete = 1 ORDER BY created_at, generation_id",
        )
        .expect("generation anchors")
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .expect("anchor rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("anchor values")
        .into_iter()
        .map(|bytes| lithograph_core::storage::HashId::from_slice(&bytes).expect("anchor hash"))
        .collect::<Vec<_>>();
    assert_eq!(anchors.len(), 2);
    assert!(!anchors.contains(&first));
    assert!(anchors.contains(&second));
    assert!(anchors.contains(&third));
}

#[test]
fn canonical_gc_removes_generation_anchored_only_by_deleted_branch() {
    let file = NamedTempFile::new().expect("temporary database");
    let path = file.path().to_path_buf();
    drop(file);
    let connection = initialize_database(&path);
    execute(&connection, "CREATE (:Metric {value:1}) FINISH").expect("seed metric");
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
    )
    .expect("type proof");
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    )
    .expect("range index");
    execute(
        &connection,
        "CALL lithograph.branch.create('side') YIELD name RETURN name",
    )
    .expect("create side branch");
    execute(
        &connection,
        "CALL lithograph.branch.checkout('side') YIELD name RETURN name",
    )
    .expect("checkout side branch");
    execute(&connection, "CREATE (:Metric {value:2}) FINISH").expect("side mutation");
    let side = branch_head(&connection, "side").expect("side head");
    execute(
        &connection,
        "CALL lithograph.index.rebuild('metric_value', 'branch/side') YIELD name RETURN name",
    )
    .expect("side generation");
    let side_generation: i64 = connection
        .query_row(
            "SELECT generation_id FROM main._lithograph_index_generations WHERE anchor_commit = ?1",
            [side.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("side generation id");
    execute(
        &connection,
        "CALL lithograph.branch.checkout('main') YIELD name RETURN name",
    )
    .expect("checkout main");
    execute(
        &connection,
        "CALL lithograph.branch.delete('side') YIELD name RETURN name",
    )
    .expect("delete side branch");
    execute(
        &connection,
        "CALL lithograph.gc() YIELD commits RETURN commits",
    )
    .expect("canonical gc");
    let manifest_exists: i64 = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_index_generations WHERE generation_id = ?1)",
            [side_generation],
            |row| row.get(0),
        )
        .expect("manifest reachability");
    let payload_exists: i64 = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_index_entries WHERE generation_id = ?1)",
            [side_generation],
            |row| row.get(0),
        )
        .expect("payload reachability");
    assert_eq!((manifest_exists, payload_exists), (0, 0));
}
