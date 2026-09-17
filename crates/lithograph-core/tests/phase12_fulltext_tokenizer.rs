use std::cell::Cell;
use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, QuerySummary, SnapshotSelector,
    prepare,
};
use lithograph_core::storage::{
    STORAGE_FORMAT, branch_head, create_storage_schema, initialize_connection_state,
    initialize_root,
};
use rusqlite::{Connection, OptionalExtension as _};

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite must open");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1),magic TEXT NOT NULL,database_id TEXT NOT NULL,storage_format INTEGER NOT NULL);",
        )
        .expect("metadata table");
    connection
        .execute(
            "INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format) VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000012', ?1)",
            [STORAGE_FORMAT],
        )
        .expect("metadata marker");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    initialize_connection_state(&connection).expect("connection state");
    connection
}

fn execute(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), QueryError> {
    execute_with_params(connection, query, BTreeMap::new(), options)
}

fn execute_with_params(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), QueryError> {
    let prepared = prepare(connection, query, params, options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 32)?;
        rows.extend(batch.rows);
        if batch.done {
            return Ok((rows, cursor.complete(connection)?));
        }
    }
}

fn temp_table_exists(connection: &Connection, name: &str) -> bool {
    connection
        .query_row(
            "SELECT 1 FROM temp.sqlite_schema WHERE type='table' AND name=?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .expect("inspect temp schema")
        .is_some()
}

fn temp_fulltext_table_count(connection: &Connection) -> i64 {
    connection
        .query_row(
            "SELECT count(*) FROM temp.sqlite_schema WHERE type='table' AND name LIKE '_lithograph_fts_%' AND name NOT LIKE '%_data' AND name NOT LIKE '%_idx' AND name NOT LIKE '%_content' AND name NOT LIKE '%_docsize' AND name NOT LIKE '%_config'",
            [],
            |row| row.get(0),
        )
        .expect("count derived FTS tables")
}

fn fulltext_analyzer_from_show(connection: &Connection, index_name: &str) -> String {
    let (rows, _) = execute(
        connection,
        "SHOW INDEXES YIELD name, type, options RETURN name, type, options ORDER BY name",
        ExecutionOptions::default(),
    )
    .expect("SHOW INDEXES");
    let row = rows
        .iter()
        .find(|row| row.first() == Some(&Value::String(index_name.to_owned())))
        .unwrap_or_else(|| panic!("missing index {index_name}: {rows:?}"));
    assert_eq!(row[1], Value::String("FULLTEXT".to_owned()));
    let Value::Map(options) = &row[2] else {
        panic!("FULLTEXT options must be a map: {row:?}");
    };
    let Some(Value::Map(config)) = options.get("indexConfig") else {
        panic!("FULLTEXT options must contain indexConfig: {options:?}");
    };
    let Some(Value::String(analyzer)) = config.get("fulltext.analyzer") else {
        panic!("FULLTEXT config must contain analyzer: {config:?}");
    };
    analyzer.clone()
}

#[test]
fn fulltext_default_and_complete_fts5_specification_are_versioned_verbatim() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc {text:'café alpha-beta graph'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("create document");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX default_text FOR (n:Doc) ON EACH [n.text]",
        ExecutionOptions::default(),
    )
    .expect("create default fulltext index");
    assert_eq!(
        fulltext_analyzer_from_show(&connection, "default_text"),
        "unicode61"
    );
    execute(
        &connection,
        "DROP INDEX default_text",
        ExecutionOptions::default(),
    )
    .expect("drop default index before alternate configuration");

    let specification = "unicode61 remove_diacritics 0 tokenchars '-_'";
    let ddl = format!(
        "CREATE FULLTEXT INDEX configured_text FOR (n:Doc) ON EACH [n.text] OPTIONS {{indexConfig:{{`fulltext.analyzer`:\"{specification}\"}}}}"
    );
    execute(&connection, &ddl, ExecutionOptions::default()).expect("create configured index");
    assert_eq!(
        fulltext_analyzer_from_show(&connection, "configured_text"),
        specification
    );
    let (rows, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('configured_text', '\"alpha-beta\"') YIELD node RETURN node.text",
        ExecutionOptions::default(),
    )
    .expect("query configured tokenizer");
    assert_eq!(rows.len(), 1);
}

#[test]
fn schema_runtime_probe_is_atomic_and_legacy_aliases_are_removed() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc {text:'graph'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("create fixture");
    let before = branch_head(&connection, "main").expect("branch head");

    for (ordinal, analyzer) in ["english", "standard-no-stop-words", "phase12_missing"]
        .into_iter()
        .enumerate()
    {
        let ddl = format!(
            "CREATE FULLTEXT INDEX bad_{ordinal} FOR (n:Doc) ON EACH [n.text] OPTIONS {{indexConfig:{{`fulltext.analyzer`:'{analyzer}'}}}}"
        );
        let error = execute(&connection, &ddl, ExecutionOptions::default())
            .expect_err("unregistered tokenizer must fail schema execution");
        assert_eq!(error.kind, QueryErrorKind::Schema, "analyzer={analyzer}");
        assert_eq!(
            branch_head(&connection, "main").expect("branch head after failure"),
            before
        );
    }
    let probe_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM temp.sqlite_schema WHERE name LIKE '_lithograph_fts_probe_%'",
            [],
            |row| row.get(0),
        )
        .expect("probe cleanup");
    assert_eq!(probe_count, 0);

    let error = execute(
        &connection,
        "CREATE FULLTEXT INDEX blank_text FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'   '}}",
        ExecutionOptions::default(),
    )
    .expect_err("blank analyzer must fail statically");
    assert_eq!(error.kind, QueryErrorKind::Schema);

    let nul_ddl = "CREATE FULLTEXT INDEX nul_text FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:\"unicode61\0token\"}}";
    let error = execute(&connection, nul_ddl, ExecutionOptions::default())
        .expect_err("NUL analyzer must fail statically");
    assert_eq!(error.kind, QueryErrorKind::Schema);
}

#[test]
fn explain_and_if_not_exists_noop_do_not_require_new_tokenizer_construction() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc {text:'graph', title:'other graph'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("fixture");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text]",
        ExecutionOptions::default(),
    )
    .expect("create index");
    let before = branch_head(&connection, "main").expect("head");

    execute(
        &connection,
        "EXPLAIN CREATE FULLTEXT INDEX unavailable_text FOR (n:Doc) ON EACH [n.title] OPTIONS {indexConfig:{`fulltext.analyzer`:'phase12_missing'}}",
        ExecutionOptions::default(),
    )
    .expect("EXPLAIN must not construct tokenizer");
    assert_eq!(branch_head(&connection, "main").expect("head"), before);

    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text IF NOT EXISTS FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'phase12_missing'}}",
        ExecutionOptions::default(),
    )
    .expect("true IF NOT EXISTS no-op must not probe replacement analyzer");
    assert_ne!(
        branch_head(&connection, "main").expect("head after write-intent no-op"),
        before,
        "successful schema no-op must retain the global write-intent Commit contract"
    );
    assert_eq!(
        fulltext_analyzer_from_show(&connection, "doc_text"),
        "unicode61"
    );
}

#[test]
fn query_override_preserves_phrase_boolean_prefix_and_query_time_validation() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc:Visible {name:'ordered', text:'run graph'}), (:Doc:Hidden {name:'reversed', text:'graph run'}), (:Doc:Visible {name:'database', text:'run xyz'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("create documents");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'unicode61'}}",
        ExecutionOptions::default(),
    )
    .expect("create index");

    let (phrase, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('doc_text', '\"running graph\"', {analyzer:'porter unicode61'}) YIELD node RETURN node.name",
        ExecutionOptions::default(),
    )
    .expect("phrase override");
    assert_eq!(phrase, vec![vec![Value::String("ordered".to_owned())]]);

    let (boolean, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('doc_text', 'running NOT xyz', {analyzer:'porter unicode61'}) YIELD node RETURN node.name ORDER BY node.name",
        ExecutionOptions::default(),
    )
    .expect("boolean override");
    assert_eq!(
        boolean,
        vec![
            vec![Value::String("ordered".to_owned())],
            vec![Value::String("reversed".to_owned())],
        ]
    );

    let (and_rows, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('doc_text', 'running AND graph', {analyzer:'porter unicode61'}) YIELD node RETURN node.name ORDER BY node.name",
        ExecutionOptions::default(),
    )
    .expect("AND override");
    assert_eq!(
        and_rows,
        vec![
            vec![Value::String("ordered".to_owned())],
            vec![Value::String("reversed".to_owned())],
        ]
    );

    let (or_rows, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('doc_text', 'running OR graph', {analyzer:'porter unicode61'}) YIELD node RETURN node.name ORDER BY node.name",
        ExecutionOptions::default(),
    )
    .expect("OR override");
    assert_eq!(or_rows.len(), 3);

    let (property_rows, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('doc_text', 'text:\"running graph\"', {analyzer:'porter unicode61'}) YIELD node RETURN node.name",
        ExecutionOptions::default(),
    )
    .expect("property-qualified override");
    assert_eq!(
        property_rows,
        vec![vec![Value::String("ordered".to_owned())]]
    );

    let (prefix, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('doc_text', 'running*', {analyzer:'porter unicode61'}) YIELD node RETURN node.name ORDER BY node.name",
        ExecutionOptions::default(),
    )
    .expect("prefix override");
    assert_eq!(prefix.len(), 3);

    let mut visible = ExecutionOptions::default();
    visible.graph_view.require_all_labels = ["Visible".to_owned()].into_iter().collect();
    let (visible_rows, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('doc_text', 'running', {analyzer:'porter unicode61', limit:2}) YIELD node, score RETURN node.name, score ORDER BY node.name",
        visible,
    )
    .expect("override Graph View-before-limit");
    assert_eq!(visible_rows.len(), 2);
    assert_eq!(visible_rows[0][0], Value::String("database".to_owned()));
    assert_eq!(visible_rows[1][0], Value::String("ordered".to_owned()));
    assert!(visible_rows.iter().all(|row| {
        matches!(row.get(1), Some(Value::Float(score)) if score.is_finite() && *score >= 0.0)
    }));
    let override_roots: i64 = connection
        .query_row(
            "SELECT count(*) FROM temp.sqlite_schema WHERE type='table' AND name GLOB '_lithograph_fts_override_*' AND sql LIKE 'CREATE VIRTUAL TABLE%'",
            [],
            |row| row.get(0),
        )
        .expect("inspect override derived tables");
    assert_eq!(
        override_roots, 1,
        "different query analyzers must reuse one Snapshot/Index override corpus"
    );

    assert_invalid_override_analyzers(&connection);
}

fn assert_invalid_override_analyzers(connection: &Connection) {
    for options in [
        "{analyzer:'phase12_missing'}",
        "{analyzer:'phase12_missing', limit:0}",
    ] {
        let query = format!(
            "CALL db.index.fulltext.queryNodes('doc_text', '', {options}) YIELD node RETURN node"
        );
        let error = execute(connection, &query, ExecutionOptions::default())
            .expect_err("override analyzer must validate even for empty/zero-result queries");
        assert_eq!(error.kind, QueryErrorKind::Semantic);
    }
}

#[test]
fn fulltext_query_cache_is_warm_rebuildable_and_history_scoped() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc {name:'old', text:'legacy graph'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("old document");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text]",
        ExecutionOptions::default(),
    )
    .expect("create index");
    let historical = branch_head(&connection, "main").expect("historical commit");
    let query =
        "CALL db.index.fulltext.queryNodes('doc_text', 'graph') YIELD node RETURN node.name";
    execute(&connection, query, ExecutionOptions::default()).expect("build current cache");
    let warm_count = temp_fulltext_table_count(&connection);
    execute(&connection, query, ExecutionOptions::default()).expect("reuse warm cache");
    assert_eq!(temp_fulltext_table_count(&connection), warm_count);

    execute(
        &connection,
        "CREATE (:Doc {name:'new', text:'future graph'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("new document");
    let (current, _) =
        execute(&connection, query, ExecutionOptions::default()).expect("current query");
    assert_eq!(current.len(), 2);
    let mut old = ExecutionOptions::default();
    old.snapshot = SnapshotSelector::Commit(historical.to_hex());
    let (historical_rows, _) = execute(&connection, query, old).expect("historical query");
    assert_eq!(historical_rows, vec![vec![Value::String("old".to_owned())]]);
    assert!(temp_fulltext_table_count(&connection) > warm_count);

    let table = connection
        .query_row(
            "SELECT name FROM temp.sqlite_schema WHERE type='table' AND name LIKE '_lithograph_fts_%' AND name NOT LIKE '%_data' AND name NOT LIKE '%_idx' AND name NOT LIKE '%_content' AND name NOT LIKE '%_docsize' AND name NOT LIKE '%_config' ORDER BY name LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("derived FTS table");
    connection
        .execute_batch(&format!("DROP TABLE temp.{table}"))
        .expect("drop derived cache");
    assert!(!temp_table_exists(&connection, &table));
    execute(&connection, query, ExecutionOptions::default()).expect("rebuild after deletion");
}

#[test]
fn malformed_specification_and_query_expression_use_public_error_categories_without_sql_injection()
{
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc {text:'graph'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("fixture");
    let malicious = "unicode61 '); CREATE TABLE main.phase12_injected(value); --";
    let encoded = malicious.replace('\\', "\\\\").replace('"', "\\\"");
    let ddl = format!(
        "CREATE FULLTEXT INDEX malicious_text FOR (n:Doc) ON EACH [n.text] OPTIONS {{indexConfig:{{`fulltext.analyzer`:\"{encoded}\"}}}}"
    );
    let error = execute(&connection, &ddl, ExecutionOptions::default())
        .expect_err("malformed specification must fail");
    assert_eq!(error.kind, QueryErrorKind::Schema);
    let injected: i64 = connection
        .query_row(
            "SELECT count(*) FROM main.sqlite_schema WHERE name='phase12_injected'",
            [],
            |row| row.get(0),
        )
        .expect("inspect main schema");
    assert_eq!(injected, 0);

    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text]",
        ExecutionOptions::default(),
    )
    .expect("create valid index");
    let error = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('doc_text', '\"unterminated') YIELD node RETURN node",
        ExecutionOptions::default(),
    )
    .expect_err("invalid FTS5 query expression must fail");
    assert_eq!(error.kind, QueryErrorKind::Semantic);
}

#[test]
fn fulltext_configuration_and_query_options_reject_unknown_null_and_wrong_types() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc {text:'graph'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("fixture");

    for query in [
        "CREATE FULLTEXT INDEX bad1 FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:null}}",
        "CREATE FULLTEXT INDEX bad2 FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:42}}",
        "CREATE FULLTEXT INDEX bad3 FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.eventually_consistent`:'false'}}",
        "CREATE FULLTEXT INDEX bad4 FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{unknown:true}}",
        "CREATE FULLTEXT INDEX bad5 FOR (n:Doc) ON EACH [n.text] OPTIONS {unknown:{}}",
        "CREATE FULLTEXT INDEX bad6 FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:''}}",
        "CREATE FULLTEXT INDEX bad7 FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'   '}}",
    ] {
        let error = execute(&connection, query, ExecutionOptions::default())
            .expect_err("invalid full-text Index configuration must fail");
        assert_eq!(error.kind, QueryErrorKind::Schema, "query={query}");
    }

    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.eventually_consistent`:true}}",
        ExecutionOptions::default(),
    )
    .expect("valid boolean metadata");
    for options in [
        "{analyzer:null}",
        "{analyzer:42}",
        "{analyzer:''}",
        "{skip:null}",
        "{limit:null}",
        "{skip:-1}",
        "{limit:-1}",
        "{unknown:1}",
    ] {
        let query = format!(
            "CALL db.index.fulltext.queryNodes('doc_text', 'graph', {options}) YIELD node RETURN node"
        );
        let error = execute(&connection, &query, ExecutionOptions::default())
            .expect_err("invalid full-text query options must fail");
        assert_eq!(error.kind, QueryErrorKind::Semantic, "options={options}");
    }
}

#[test]
fn fulltext_text_extraction_preserves_string_list_and_missing_property_semantics() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc {name:'string', text:'alpha'}), (:Doc {name:'list', text:['beta','gamma']}), (:Doc {name:'missing'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("text extraction fixtures");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text]",
        ExecutionOptions::default(),
    )
    .expect("create index");

    for (query, expected) in [
        ("alpha", vec!["string"]),
        ("beta", vec!["list"]),
        ("gamma", vec!["list"]),
        ("missing", Vec::<&str>::new()),
    ] {
        let cypher = format!(
            "CALL db.index.fulltext.queryNodes('doc_text', '{query}') YIELD node RETURN node.name ORDER BY node.name"
        );
        let (rows, _) = execute(&connection, &cypher, ExecutionOptions::default())
            .unwrap_or_else(|error| panic!("query {query:?} failed: {error}"));
        let names = rows
            .into_iter()
            .map(|row| match &row[0] {
                Value::String(value) => value.clone(),
                value => panic!("expected String name, got {value:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            expected.into_iter().map(str::to_owned).collect::<Vec<_>>()
        );
    }
}

#[test]
fn interrupted_fulltext_cache_build_is_not_reused_and_recovers_on_retry() {
    let connection = fresh_storage();
    let nodes = (0..64)
        .map(|ordinal| format!("(:Doc {{name:'n{ordinal}', text:'graph'}})"))
        .collect::<Vec<_>>()
        .join(", ");
    execute(
        &connection,
        &format!("CREATE {nodes} FINISH"),
        ExecutionOptions::default(),
    )
    .expect("interrupt fixtures");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text]",
        ExecutionOptions::default(),
    )
    .expect("create index");

    let prepared = prepare(
        &connection,
        "CALL db.index.fulltext.queryNodes('doc_text', 'graph') YIELD node RETURN node.name",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare interrupted query");
    let mut cursor = QueryCursor::new(prepared);
    let calls = Cell::new(0_u32);
    let interrupt = || {
        let next = calls.get() + 1;
        calls.set(next);
        next > 20
    };
    let error = cursor
        .next_batch_with_interrupt(&connection, 32, &interrupt)
        .expect_err("cache build should be interrupted");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    let root_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM temp.sqlite_schema WHERE type='table' AND name GLOB '_lithograph_fts_*' AND sql LIKE 'CREATE VIRTUAL TABLE%'",
            [],
            |row| row.get(0),
        )
        .expect("inspect interrupted cache root");
    assert_eq!(
        root_count, 1,
        "interrupted build keeps one quarantined root"
    );
    let ready_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM temp._lithograph_semantic_fts_ready",
            [],
            |row| row.get(0),
        )
        .expect("inspect interrupted cache readiness");
    assert_eq!(ready_count, 0);

    let (rows, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('doc_text', 'graph') YIELD node RETURN node.name",
        ExecutionOptions::default(),
    )
    .expect("retry after interruption");
    assert_eq!(rows.len(), 64);
    let ready_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM temp._lithograph_semantic_fts_ready",
            [],
            |row| row.get(0),
        )
        .expect("inspect rebuilt cache readiness");
    assert_eq!(ready_count, 1);
}

#[test]
fn fulltext_definition_change_round_trips_through_diff_and_patch() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc {text:'running graph'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("fixture");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'unicode61'}}",
        ExecutionOptions::default(),
    )
    .expect("unicode61 definition");
    let before = branch_head(&connection, "main").expect("before definition change");
    execute(
        &connection,
        "DROP INDEX doc_text",
        ExecutionOptions::default(),
    )
    .expect("drop old definition");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'porter unicode61'}}",
        ExecutionOptions::default(),
    )
    .expect("porter definition");
    let after = branch_head(&connection, "main").expect("after definition change");

    let (rows, _) = execute(
        &connection,
        &format!(
            "CALL lithograph.diff('commit/{}', 'commit/{}') YIELD patch RETURN patch",
            before.to_hex(),
            after.to_hex()
        ),
        ExecutionOptions::default(),
    )
    .expect("diff definition change");
    let patch = rows[0][0].clone();
    assert_fulltext_definition_patch(&patch);

    execute(
        &connection,
        &format!(
            "CALL lithograph.reset('commit/{}') YIELD to RETURN to",
            before.to_hex()
        ),
        ExecutionOptions::default(),
    )
    .expect("reset to old definition");
    assert_eq!(
        branch_head(&connection, "main").expect("reset head"),
        before
    );
    let mut params = BTreeMap::new();
    params.insert("patch".to_owned(), patch);
    execute_with_params(
        &connection,
        "CALL lithograph.patch.apply($patch) YIELD commit RETURN commit",
        params,
        ExecutionOptions::default(),
    )
    .expect("apply definition patch");
    let (show, _) = execute(
        &connection,
        "SHOW FULLTEXT INDEXES YIELD name, options WHERE name='doc_text' RETURN options",
        ExecutionOptions::default(),
    )
    .expect("show patched definition");
    assert!(value_contains_string(&show[0][0], "porter unicode61"));

    let patched = branch_head(&connection, "main").expect("patched definition commit");
    execute(
        &connection,
        &format!(
            "CALL lithograph.revert('commit/{}') YIELD commit RETURN commit",
            patched.to_hex()
        ),
        ExecutionOptions::default(),
    )
    .expect("revert definition patch");
    let (show, _) = execute(
        &connection,
        "SHOW FULLTEXT INDEXES YIELD name, options WHERE name='doc_text' RETURN options",
        ExecutionOptions::default(),
    )
    .expect("show reverted definition");
    assert!(value_contains_string(&show[0][0], "unicode61"));
}

fn assert_fulltext_definition_patch(patch: &Value) {
    let Value::Map(patch_map) = &patch else {
        panic!("patch must be a Map: {patch:?}");
    };
    let Some(Value::List(operations)) = patch_map.get("operations") else {
        panic!("patch must contain operations: {patch:?}");
    };
    assert_eq!(
        operations.len(),
        1,
        "definition change must be one Index slot"
    );
    let Value::Map(operation) = &operations[0] else {
        panic!("Index operation must be a Map: {:?}", operations[0]);
    };
    assert_eq!(
        operation.get("slot"),
        Some(&Value::String("index/doc_text".to_owned()))
    );
    assert!(value_contains_string(&operations[0], "unicode61"));
    assert!(value_contains_string(&operations[0], "porter unicode61"));
}

#[test]
fn fulltext_definition_change_survives_rebase() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Doc {text:'running graph'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("fixture");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'unicode61'}}",
        ExecutionOptions::default(),
    )
    .expect("base Full-text definition");
    execute(
        &connection,
        "CALL lithograph.branch.create('phase12-rebase') YIELD name RETURN name",
        ExecutionOptions::default(),
    )
    .expect("create rebase branch");
    execute(
        &connection,
        "CALL lithograph.branch.checkout('phase12-rebase') YIELD name RETURN name",
        ExecutionOptions::default(),
    )
    .expect("checkout rebase branch");
    execute(
        &connection,
        "DROP INDEX doc_text",
        ExecutionOptions::default(),
    )
    .expect("drop old definition on topic");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'porter unicode61'}}",
        ExecutionOptions::default(),
    )
    .expect("create changed definition on topic");
    execute(
        &connection,
        "CALL lithograph.branch.checkout('main') YIELD name RETURN name",
        ExecutionOptions::default(),
    )
    .expect("checkout main");
    execute(
        &connection,
        "CREATE (:MainAdvance {v:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("advance main independently");
    execute(
        &connection,
        "CALL lithograph.branch.checkout('phase12-rebase') YIELD name RETURN name",
        ExecutionOptions::default(),
    )
    .expect("return to topic");

    let (rows, _) = execute(
        &connection,
        "CALL lithograph.rebase('branch/main') YIELD status RETURN status",
        ExecutionOptions::default(),
    )
    .expect("rebase Full-text definition change");
    assert_eq!(rows, vec![vec![Value::String("rebased".to_owned())]]);
    assert_eq!(
        fulltext_analyzer_from_show(&connection, "doc_text"),
        "porter unicode61"
    );
    let (hits, _) = execute(
        &connection,
        "CALL db.index.fulltext.queryNodes('doc_text', 'run') YIELD node RETURN node.text",
        ExecutionOptions::default(),
    )
    .expect("query rebased Full-text definition");
    assert_eq!(hits, vec![vec![Value::String("running graph".to_owned())]]);
}

fn value_contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value == expected,
        Value::List(values) => values
            .iter()
            .any(|value| value_contains_string(value, expected)),
        Value::Map(values) => values
            .values()
            .any(|value| value_contains_string(value, expected)),
        _ => false,
    }
}
