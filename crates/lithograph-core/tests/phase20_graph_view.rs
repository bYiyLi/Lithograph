use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, QuerySummary, SnapshotSelector,
    prepare,
};
use lithograph_core::storage::{
    CommitMetadata, LayerBuilder, OwnerKind, PropertyValue, allocate_node_id, branch_head,
    commit_layer, create_storage_schema, initialize_root, intern_label, intern_property_key,
};
use rusqlite::Connection;

fn storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory database");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1),magic TEXT NOT NULL,database_id TEXT NOT NULL,storage_format INTEGER NOT NULL);",
        )
        .expect("metadata");
    create_storage_schema(&connection).expect("schema");
    initialize_root(&connection).expect("root");
    connection
}

fn options(any: &[&str]) -> ExecutionOptions {
    let mut result = ExecutionOptions::default();
    result.graph_view.require_any_labels = any.iter().map(|s| (*s).to_owned()).collect();
    result
}

fn execute(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), QueryError> {
    let prepared = prepare(connection, query, BTreeMap::new(), options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut output = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 23)?;
        output.extend(batch.rows);
        if batch.done {
            return Ok((output, cursor.complete(connection)?));
        }
    }
}

fn read(connection: &Connection, query: &str, options: ExecutionOptions) -> Vec<Vec<Value>> {
    execute(connection, query, options).expect("read query").0
}

#[test]
fn or_selector_parsing_composition_and_nonempty_view_guards() {
    let connection = storage();
    let empty = ExecutionOptions::parse_text(
        r#"{"graphView":{"requireAllLabels":[],"requireAnyLabels":[],"excludeAnyLabels":[]}}"#,
    )
    .expect("empty arrays");
    assert!(empty.graph_view.is_full_graph());
    let parsed = ExecutionOptions::parse_text(
        r#"{"graphView":{"requireAllLabels":["Scope"],"requireAnyLabels":["A","A","B"],"excludeAnyLabels":["B"]}}"#,
    )
    .expect("OR and NOT overlap");
    assert_eq!(parsed.graph_view.require_any_labels.len(), 2);
    assert!(!parsed.graph_view.is_full_graph());
    for invalid in [
        r#"{"graphView":{"requireAnyLabels":null}}"#,
        r#"{"graphView":{"requireAnyLabels":[1]}}"#,
        r#"{"graphView":{"requireAnyLabels":"A"}}"#,
        r#"{"graphView":{"other":[]}}"#,
        r#"{"graphView":{"requireAllLabels":["Scope"],"excludeAnyLabels":["Scope"]}}"#,
    ] {
        assert_eq!(
            ExecutionOptions::parse_text(invalid)
                .expect_err("invalid selector")
                .kind,
            QueryErrorKind::InvalidArgument,
        );
    }
    for query in [
        "CREATE INDEX a_key FOR (n:A) ON (n.name)",
        "CALL lithograph.branch.list() YIELD name RETURN name",
    ] {
        let error = execute(&connection, query, options(&["A"]))
            .expect_err("OR-only Graph View cannot operate on schema/version metadata");
        assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    }
}

#[test]
fn union_read_uses_all_labels_without_duplicate_nodes_or_cross_view_paths() {
    let connection = storage();
    execute(
        &connection,
        "CREATE (:A {name:'a'}), (:B {name:'b'}), (:A:B {name:'ab'}), (:C {name:'c'}), (:A:Hidden {name:'hidden'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("fixture nodes");
    execute(
        &connection,
        "MATCH (a:A {name:'a'}), (b:B {name:'b'}) CREATE (a)-[:LINK]->(b) FINISH",
        ExecutionOptions::default(),
    )
    .expect("fixture relationship");
    let mut filtered = options(&["A", "B", "DoesNotExist"]);
    filtered
        .graph_view
        .exclude_any_labels
        .insert("Hidden".to_owned());
    assert_eq!(
        read(
            &connection,
            "MATCH (n) RETURN n.name ORDER BY n.name",
            filtered.clone(),
        ),
        vec![
            vec![Value::String("a".to_owned())],
            vec![Value::String("ab".to_owned())],
            vec![Value::String("b".to_owned())],
        ]
    );
    assert_eq!(
        read(&connection, "MATCH (n) RETURN count(n)", filtered.clone()),
        vec![vec![Value::Integer(3)]],
    );
    assert_eq!(
        read(
            &connection,
            "MATCH (a)-[:LINK]->(b) RETURN a.name, b.name",
            filtered.clone(),
        ),
        vec![vec![
            Value::String("a".to_owned()),
            Value::String("b".to_owned()),
        ]],
    );
    filtered
        .graph_view
        .exclude_any_labels
        .insert("B".to_owned());
    assert!(
        read(
            &connection,
            "MATCH ()-[r:LINK]->() RETURN r",
            filtered.clone()
        )
        .is_empty()
    );
    assert_eq!(
        read(
            &connection,
            "MATCH (n) RETURN n.name ORDER BY n.name",
            filtered
        ),
        vec![vec![Value::String("a".to_owned())]],
    );
}

#[test]
fn union_scan_paginates_multiple_label_domains_with_overlap() {
    let connection = storage();
    let head = branch_head(&connection, "main").expect("head");
    let a = intern_label(&connection, "A").expect("A");
    let b = intern_label(&connection, "B").expect("B");
    let c = intern_label(&connection, "C").expect("C");
    let key = intern_property_key(&connection, "id").expect("key");
    let mut layer = LayerBuilder::default();
    for i in 0..900 {
        let id = allocate_node_id(&connection).expect("id");
        layer.add_node(id).expect("node");
        let label = match i % 3 {
            0 => a,
            1 => b,
            _ => c,
        };
        layer.add_label(id, label).expect("label");
        if i % 15 == 0 {
            layer.add_label(id, b).expect("second label");
        }
        layer
            .set_property(OwnerKind::Node, id, key, PropertyValue::Integer(i))
            .expect("property");
    }
    commit_layer(
        &connection,
        "main",
        head,
        None,
        &layer,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 1,
        },
    )
    .expect("commit");
    let result = read(
        &connection,
        "MATCH (n) RETURN n.id ORDER BY n.id",
        options(&["A", "B"]),
    );
    let expected = (0..900)
        .filter(|i| i % 3 != 2)
        .map(|i| vec![Value::Integer(i)])
        .collect::<Vec<_>>();
    assert_eq!(result, expected);
    assert_eq!(
        read(
            &connection,
            "MATCH (n) RETURN count(n)",
            options(&["A", "B"])
        ),
        vec![vec![Value::Integer(expected.len() as i64)]]
    );
}

#[test]
fn or_view_preserves_writes_rollback_and_history() {
    let connection = storage();
    execute(
        &connection,
        "CREATE (:A {name:'one'}), (:B {name:'two'}), (:C {name:'hidden'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed");
    let historical = branch_head(&connection, "main").expect("historical");
    execute(
        &connection,
        "CREATE (:A:B {name:'three'}) FINISH",
        options(&["A", "B"]),
    )
    .expect("valid create");
    let valid_head = branch_head(&connection, "main").expect("valid head");
    let failure = execute(
        &connection,
        "MATCH (n:A {name:'one'}) REMOVE n:A FINISH",
        options(&["A", "B"]),
    )
    .expect_err("invisible after clause");
    assert_eq!(failure.kind, QueryErrorKind::GraphViewViolation);
    assert_eq!(
        branch_head(&connection, "main").expect("unchanged"),
        valid_head
    );
    let mut old = options(&["A", "B"]);
    old.snapshot = SnapshotSelector::Commit(historical.to_hex());
    assert_eq!(
        read(&connection, "MATCH (n) RETURN count(n)", old),
        vec![vec![Value::Integer(2)]]
    );
    assert_eq!(
        read(
            &connection,
            "MATCH (n) RETURN count(n)",
            options(&["A", "B"]),
        ),
        vec![vec![Value::Integer(3)]],
    );
}

#[test]
fn or_view_uses_label_added_in_same_execution_without_stale_resolution() {
    let connection = storage();
    let result = execute(
        &connection,
        "CREATE (:NewLabel {name:'created'}) FINISH",
        options(&["NewLabel"]),
    );
    assert!(result.is_ok(), "created label must be visible: {result:?}");
    assert_eq!(
        read(
            &connection,
            "MATCH (n) RETURN n.name",
            options(&["NewLabel"]),
        ),
        vec![vec![Value::String("created".to_owned())]],
    );
}

#[test]
fn fulltext_and_vector_rank_inside_the_or_view_before_limit() {
    let connection = storage();
    execute(
        &connection,
        "CREATE (:Doc:Outside {name:'outside', text:'needle', embedding:vector([1.0,0.0],2,FLOAT64)}), (:Doc:A {name:'a', text:'needle elongated text', embedding:vector([0.9,0.1],2,FLOAT64)}), (:Doc:B {name:'b', text:'needle long description', embedding:vector([0.7,0.3],2,FLOAT64)}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("source data");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX phase20_text FOR (n:Doc) ON EACH [n.text]",
        ExecutionOptions::default(),
    )
    .expect("fulltext index");
    execute(
        &connection,
        "CREATE VECTOR INDEX phase20_vector FOR (n:Doc) ON (n.embedding)",
        ExecutionOptions::default(),
    )
    .expect("vector index");
    let text = "CALL db.index.fulltext.queryNodes('phase20_text','needle',{limit:1}) YIELD node RETURN node.name";
    let global_text = read(&connection, text, ExecutionOptions::default());
    assert_eq!(global_text, vec![vec![Value::String("outside".to_owned())]]);
    let within_text = read(&connection, text, options(&["A", "B"]));
    assert_eq!(within_text.len(), 1);
    assert!(matches!(&within_text[0][0], Value::String(name) if name == "a" || name == "b"));
    let vector = "MATCH (n:Doc) SEARCH n IN (VECTOR INDEX phase20_vector FOR vector([1.0,0.0],2,FLOAT64) LIMIT 1) RETURN n.name";
    assert_eq!(
        read(&connection, vector, ExecutionOptions::default()),
        vec![vec![Value::String("outside".to_owned())]]
    );
    let expected = vec![vec![Value::String("a".to_owned())]];
    assert_eq!(read(&connection, vector, options(&["A", "B"])), expected);
    assert_eq!(
        read(&connection, vector, options(&["A", "B"])),
        expected,
        "warm HNSW must preserve the scoped ranking"
    );
    let historical = branch_head(&connection, "main").expect("snapshot");
    execute(
        &connection,
        "CREATE (:Doc:A {name:'new', text:'needle', embedding:vector([0.999,0.001],2,FLOAT64)}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("future data");
    let mut old = options(&["A", "B"]);
    old.snapshot = SnapshotSelector::Commit(historical.to_hex());
    assert_eq!(read(&connection, vector, old), expected);
    assert_eq!(
        read(&connection, vector, options(&["A", "B"])),
        vec![vec![Value::String("new".to_owned())]]
    );
}

#[test]
fn relationship_search_checks_both_or_visible_endpoints_before_top_k() {
    let connection = storage();
    execute(
        &connection,
        "CREATE (a:A {name:'a'}), (b:B {name:'b'}), (x:Outside {name:'x'}), (a)-[:LINK {name:'allowed', text:'needle allowed', embedding:vector([0.9,0.1],2,FLOAT64)}]->(b), (a)-[:LINK {name:'hidden', text:'needle', embedding:vector([1.0,0.0],2,FLOAT64)}]->(x) FINISH",
        ExecutionOptions::default(),
    )
    .expect("relationship data");
    execute(
        &connection,
        "CREATE FULLTEXT INDEX phase20_rel_text FOR ()-[r:LINK]-() ON EACH [r.text]",
        ExecutionOptions::default(),
    )
    .expect("relationship fulltext");
    execute(
        &connection,
        "CREATE VECTOR INDEX phase20_rel_vector FOR ()-[r:LINK]-() ON (r.embedding)",
        ExecutionOptions::default(),
    )
    .expect("relationship vector");
    let text = "CALL db.index.fulltext.queryRelationships('phase20_rel_text','needle',{limit:1}) YIELD relationship RETURN relationship.name";
    assert_eq!(
        read(&connection, text, ExecutionOptions::default()),
        vec![vec![Value::String("hidden".to_owned())]]
    );
    let expected = vec![vec![Value::String("allowed".to_owned())]];
    assert_eq!(read(&connection, text, options(&["A", "B"])), expected);
    let vector = "MATCH ()-[r:LINK]->() SEARCH r IN (VECTOR INDEX phase20_rel_vector FOR vector([1.0,0.0],2,FLOAT64) LIMIT 1) RETURN r.name";
    assert_eq!(
        read(&connection, vector, ExecutionOptions::default()),
        vec![vec![Value::String("hidden".to_owned())]]
    );
    assert_eq!(read(&connection, vector, options(&["A", "B"])), expected);
}

#[test]
fn or_view_composes_with_and_not_index_seek_and_read_access() {
    let connection = storage();
    execute(
        &connection,
        "CREATE (:Scope:A {name:'a', secret:'x'}), (:Scope:B {name:'b', secret:'y'}), (:Scope:A:B {name:'ab', secret:'z'}), (:Scope:C {name:'c', secret:'z'}), (:A {name:'other', secret:'z'}), (:Scope:A:Hidden {name:'hidden', secret:'z'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("fixture");
    execute(
        &connection,
        "CREATE RANGE INDEX phase20_name FOR (n:A) ON (n.name)",
        ExecutionOptions::default(),
    )
    .expect("index");
    let opts = ExecutionOptions::parse_text(
        r#"{"graphView":{"requireAllLabels":["Scope"],"requireAnyLabels":["A","B"],"excludeAnyLabels":["Hidden"]},"readAccess":{"query":{"denyProperties":["secret"]}}}"#,
    )
    .expect("scope+OR+NOT+masked property");
    assert_eq!(
        read(
            &connection,
            "MATCH (n) RETURN n.name, n.secret ORDER BY n.name",
            opts.clone(),
        ),
        vec![
            vec![Value::String("a".to_owned()), Value::Null],
            vec![Value::String("ab".to_owned()), Value::Null],
            vec![Value::String("b".to_owned()), Value::Null],
        ]
    );
    assert_eq!(
        read(&connection, "MATCH (n:A {name:'a'}) RETURN n.name", opts,),
        vec![vec![Value::String("a".to_owned())]],
    );
    let empty = options(&["Missing", "AlsoMissing"]);
    let before: i64 = connection
        .query_row("SELECT count(*) FROM main._lithograph_labels", [], |r| {
            r.get(0)
        })
        .expect("labels before");
    assert_eq!(
        read(&connection, "MATCH (n) RETURN count(n)", empty),
        vec![vec![Value::Integer(0)]]
    );
    let after: i64 = connection
        .query_row("SELECT count(*) FROM main._lithograph_labels", [], |r| {
            r.get(0)
        })
        .expect("labels after");
    assert_eq!(before, after, "unknown OR labels must not be interned");
    let no_or =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAnyLabels":[]}}"#).expect("empty OR");
    assert_eq!(
        read(&connection, "MATCH (n) RETURN count(n)", no_or),
        vec![vec![Value::Integer(6)]]
    );
}

#[test]
fn or_view_blocks_cross_view_detach_and_label_mutation_without_publishing() {
    let connection = storage();
    execute(
        &connection,
        "CREATE (a:A {name:'a'}), (b:B {name:'b'}), (x:Outside {name:'x'}), (a)-[:LINK]->(x) FINISH",
        ExecutionOptions::default(),
    )
    .expect("fixture");
    let mut opts = options(&["A", "B"]);
    opts.graph_view
        .exclude_any_labels
        .insert("Hidden".to_owned());
    let before = branch_head(&connection, "main").expect("before");
    for write in [
        "MATCH (n:A {name:'a'}) SET n:Hidden FINISH",
        "MATCH (n:A {name:'a'}) REMOVE n:A FINISH",
        "MATCH (n:A {name:'a'}) DETACH DELETE n FINISH",
    ] {
        let error = execute(&connection, write, opts.clone()).expect_err("view violation");
        assert_eq!(error.kind, QueryErrorKind::GraphViewViolation);
        assert_eq!(
            branch_head(&connection, "main").expect("unchanged"),
            before,
            "{write} partially committed"
        );
    }
    execute(
        &connection,
        "MATCH (n:B {name:'b'}) SET n:A REMOVE n:B FINISH",
        opts.clone(),
    )
    .expect("final Label remains in OR view");
    assert_eq!(
        read(&connection, "MATCH (n) RETURN count(n)", opts.clone()),
        vec![vec![Value::Integer(2)]]
    );
    execute(&connection, "MERGE (n:B {name:'new'}) FINISH", opts.clone())
        .expect("MERGE within OR view");
    assert_eq!(
        read(&connection, "MATCH (n) RETURN count(n)", opts),
        vec![vec![Value::Integer(3)]]
    );
}

#[test]
fn selective_or_scan_skips_large_unrelated_label_population() {
    let connection = storage();
    let head = branch_head(&connection, "main").expect("head");
    let a = intern_label(&connection, "A").expect("A");
    let b = intern_label(&connection, "B").expect("B");
    let outside = intern_label(&connection, "Outside").expect("Outside");
    let mut layer = LayerBuilder::default();
    for ordinal in 0..4000 {
        let id = allocate_node_id(&connection).expect("node id");
        layer.add_node(id).expect("node");
        layer
            .add_label(
                id,
                match ordinal {
                    100 => a,
                    3900 => b,
                    _ => outside,
                },
            )
            .expect("label");
    }
    commit_layer(
        &connection,
        "main",
        head,
        None,
        &layer,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 1,
        },
    )
    .expect("write synthetic fixture");
    let (rows, summary) = execute(
        &connection,
        "MATCH (n) RETURN count(n)",
        options(&["A", "B"]),
    )
    .expect("scoped count");
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    assert!(
        summary.metrics.db_hits < 100,
        "nonmatching population should not be fully scanned: {} hits",
        summary.metrics.db_hits
    );
    let (profile, _) = execute(
        &connection,
        "PROFILE MATCH (n) RETURN count(n)",
        options(&["A", "B"]),
    )
    .expect("profile must preserve OR result");
    assert_eq!(profile, rows);
    let (explain, _) = execute(
        &connection,
        "EXPLAIN MATCH (n) RETURN count(n)",
        options(&["A", "B"]),
    )
    .expect("explain must support OR selector");
    assert_eq!(explain.len(), 1);
}

#[test]
fn unicode_or_labels_and_full_exclusion_use_exact_membership() {
    let connection = storage();
    execute(
        &connection,
        "CREATE (:规则 {name:'rule'}), (:决策 {name:'decision'}), (:規則 {name:'variant'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("Unicode labels");
    assert_eq!(
        read(
            &connection,
            "MATCH (n) RETURN n.name ORDER BY n.name",
            options(&["规则", "决策"]),
        ),
        vec![
            vec![Value::String("decision".to_owned())],
            vec![Value::String("rule".to_owned())],
        ]
    );
    let mut excluded = options(&["规则", "决策"]);
    excluded.graph_view.exclude_any_labels =
        ["规则", "决策"].into_iter().map(str::to_owned).collect();
    assert!(read(&connection, "MATCH (n) RETURN n", excluded).is_empty());
}
