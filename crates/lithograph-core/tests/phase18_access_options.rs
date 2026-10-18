use std::collections::BTreeMap;

use lithograph_core::cypher::{NodeValue, PathValue, RelationshipValue, Value};
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryErrorKind, prepare};
use lithograph_core::storage::{create_storage_schema, initialize_root};
use rusqlite::Connection;
use serde_json::json;

fn storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    create_storage_schema(&connection).expect("storage");
    initialize_root(&connection).expect("root");
    connection
}

#[test]
fn omitted_empty_and_explicit_true_access_preserve_defaults() {
    for options in [
        json!({}),
        json!({"readAccess":{},"writeAccess":{}}),
        json!({"readAccess":{"query":{},"result":{},"denyFunctions":[]}}),
        json!({"writeAccess":{"denyProperties":[],"denyPropertyPrefixes":[],"denyPropertyPatterns":[]}}),
        json!({"readAccess":{"allowNamedProcedures":true,"allowShow":true,"allowExplain":true,"allowProfile":true,"allowExternalIO":true},"writeAccess":{"allowCreate":true,"allowDelete":true,"allowLabelChanges":true,"allowSchemaChanges":true,"allowVersionChanges":true,"allowMaintenance":true}}),
    ] {
        assert_eq!(
            ExecutionOptions::parse(&options).expect("valid"),
            ExecutionOptions::default(),
            "{options}"
        );
    }
}

#[test]
fn property_rules_are_exact_literal_union_and_full_regex_matches() {
    let options = ExecutionOptions::parse(&json!({"readAccess":{"query":{
        "denyProperties":["Secret","Secret"],
        "denyPropertyPrefixes":["[internal]"],
        "denyPropertyPatterns":["audit_[0-9]+","(?i)private","私密_[0-9]+"]
    }}}))
    .expect("compiled rules");
    let rules = &options.access.read.query;
    for name in ["Secret", "[internal]key", "audit_42", "PRIVATE", "私密_42"] {
        assert!(rules.denies(name), "{name}");
    }
    for name in [
        "secret",
        "internalkey",
        "xaudit_42",
        "audit_42x",
        "audit_42\n",
        "private_note",
        "前私密_42",
        "私密_42后",
    ] {
        assert!(!rules.denies(name), "{name}");
    }
    assert_eq!(options, options.clone());
    assert!(!rules.is_empty());
}

#[test]
fn invalid_access_configuration_is_an_argument_error() {
    for invalid in [
        json!({"readAccess":null}),
        json!({"writeAccess":[]}),
        json!({"readAccess":{"unknown":true}}),
        json!({"readAccess":{"query":null}}),
        json!({"readAccess":{"result":{"unknown":[]}}}),
        json!({"writeAccess":{"allowDelete":null}}),
        json!({"readAccess":{"allowExternalIO":1}}),
        json!({"readAccess":{"allowShow":"false"}}),
        json!({"readAccess":{"denyFunctions":[""]}}),
        json!({"readAccess":{"denyFunctions":["element"]}}),
        json!({"writeAccess":{"denyProperties":[1]}}),
        json!({"writeAccess":{"denyProperties":[""]}}),
        json!({"writeAccess":{"denyPropertyPrefixes":[""]}}),
        json!({"writeAccess":{"denyPropertyPatterns":[""]}}),
        json!({"writeAccess":{"denyPropertyPatterns":["("]}}),
        json!({"readAccess":{"query":{"denyPropertyPatterns":["(?=x)"]}}}),
        json!({"readAccess":{"result":{"denyProperties":null}}}),
        json!({"writeAccess":{"denyPropertyPatterns":"secret"}}),
    ] {
        assert_eq!(
            ExecutionOptions::parse(&invalid)
                .expect_err("invalid configuration")
                .kind,
            QueryErrorKind::InvalidArgument,
            "{invalid}"
        );
    }
}

#[test]
fn capabilities_are_checked_on_the_complete_ast_before_execution() {
    let connection = storage();
    for (options, query) in [
        (json!({"readAccess":{"allowShow":false}}), "SHOW FUNCTIONS"),
        (
            json!({"readAccess":{"allowExplain":false}}),
            "EXPLAIN RETURN 1",
        ),
        (
            json!({"readAccess":{"allowProfile":false}}),
            "PROFILE RETURN 1",
        ),
        (
            json!({"readAccess":{"allowNamedProcedures":false}}),
            "CALL lithograph.branch.list() YIELD name RETURN name",
        ),
        (
            json!({"readAccess":{"allowExternalIO":false}}),
            "LOAD CSV FROM 'file:///__phase18_denied__.csv' AS row RETURN row",
        ),
        (
            json!({"readAccess":{"allowExternalIO":false}}),
            "CALL db.index.fulltext.queryNodes('missing','text') YIELD node RETURN node",
        ),
        (
            json!({"readAccess":{"allowExternalIO":false}}),
            "CALL db.index.semantic.queryNodes('missing','text',{limit:1}) YIELD node RETURN node",
        ),
        (
            json!({"writeAccess":{"allowSchemaChanges":false}}),
            "CREATE RANGE INDEX blocked FOR (n:Blocked) ON (n.v)",
        ),
        (
            json!({"writeAccess":{"allowVersionChanges":false}}),
            "CALL lithograph.branch.create('blocked') YIELD name RETURN name",
        ),
        (
            json!({"writeAccess":{"allowMaintenance":false}}),
            "CALL lithograph.gc() YIELD commits RETURN commits",
        ),
        (
            json!({"writeAccess":{"allowMaintenance":false}}),
            "CALL db.index.semantic.rebuild('missing','branch/main') YIELD name RETURN name",
        ),
        (
            json!({"readAccess":{"denyFunctions":["ID"]}}),
            "MATCH (n) RETURN id(n)",
        ),
        (
            json!({"readAccess":{"denyFunctions":["elementId"]}}),
            "MATCH (n) RETURN CASE WHEN false THEN elementId(n) ELSE 'ok' END",
        ),
        (
            json!({"readAccess":{"denyFunctions":["toString"]}}),
            "RETURN s\"value={toString(1)}\"",
        ),
        (
            json!({"readAccess":{"allowExternalIO":false}}),
            "RETURN s\"{COUNT { LOAD CSV FROM 'file:///__phase18_denied__.csv' AS row RETURN row }}\" AS x",
        ),
    ] {
        let options = ExecutionOptions::parse(&options).expect("valid configuration");
        let error = prepare(&connection, query, BTreeMap::new(), options).expect_err(query);
        assert_eq!(error.kind, QueryErrorKind::AccessDenied, "{query}: {error}");
        assert_eq!(
            error.sqlite_code,
            Some(rusqlite::ffi::SQLITE_AUTH),
            "{query}"
        );
    }
}

#[test]
fn operation_permissions_do_not_block_other_read_or_write_capabilities() {
    let connection = storage();
    for (options, query) in [
        (
            json!({"readAccess":{"allowNamedProcedures":false}}),
            "CALL { RETURN 1 AS x } RETURN x",
        ),
        (
            json!({"writeAccess":{"allowVersionChanges":false}}),
            "CALL lithograph.branch.list() YIELD name RETURN name",
        ),
        (
            json!({"writeAccess":{"allowMaintenance":false}}),
            "SHOW INDEXES",
        ),
        (
            json!({"writeAccess":{"allowSchemaChanges":false}}),
            "CREATE (:Allowed {v:1}) FINISH",
        ),
        (
            json!({"readAccess":{"denyFunctions":["elementId"]}}),
            "RETURN 'elementId' AS elementId",
        ),
    ] {
        prepare(
            &connection,
            query,
            BTreeMap::new(),
            ExecutionOptions::parse(&options).expect("valid"),
        )
        .expect(query);
    }
    let error = prepare(
        &connection,
        "CREATE (:Blocked) FINISH",
        BTreeMap::new(),
        ExecutionOptions::parse(
            &json!({"readOnlyNoExternalIO":true,"writeAccess":{"allowCreate":true}}),
        )
        .expect("valid"),
    )
    .expect_err("old read-only policy still applies");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    assert!(error.message.contains("readOnlyNoExternalIO"));
}

#[test]
fn graph_filter_recurses_without_matching_map_keys_or_structure() {
    let options = ExecutionOptions::parse(&json!({"readAccess":{"result":{"denyProperties":["private","elementId","labels","type","start","end"]}}})).expect("rules");
    let properties = BTreeMap::from([
        ("private".to_owned(), Value::Integer(1)),
        ("public".to_owned(), Value::Integer(2)),
    ]);
    let node = NodeValue {
        element_id: "n:1".to_owned(),
        labels: vec!["Person".to_owned()],
        properties: properties.clone(),
    };
    let relationship = RelationshipValue {
        element_id: "r:1".to_owned(),
        relationship_type: "KNOWS".to_owned(),
        start: "n:1".to_owned(),
        end: "n:2".to_owned(),
        properties,
    };
    let value = Value::Map(BTreeMap::from([
        ("private".to_owned(), Value::Integer(99)),
        (
            "nested".to_owned(),
            Value::List(vec![
                Value::Node(node.clone()),
                Value::Relationship(relationship.clone()),
                Value::Path(PathValue {
                    nodes: vec![node],
                    relationships: vec![relationship],
                }),
            ]),
        ),
    ]));
    let filtered = options.access.read.result.filter_graph_properties(&value);
    let Value::Map(map) = &filtered else {
        panic!("map");
    };
    assert_eq!(map["private"], Value::Integer(99));
    let Value::List(nested) = &map["nested"] else {
        panic!("list");
    };
    let Value::Node(node) = &nested[0] else {
        panic!("node");
    };
    assert_eq!(node.element_id, "n:1");
    assert_eq!(node.labels, ["Person"]);
    assert_eq!(node.properties.len(), 1);
    assert_eq!(node.properties["public"], Value::Integer(2));
    let Value::Relationship(relationship) = &nested[1] else {
        panic!("relationship");
    };
    assert_eq!(relationship.element_id, "r:1");
    assert_eq!(relationship.relationship_type, "KNOWS");
    assert_eq!(relationship.start, "n:1");
    assert_eq!(relationship.end, "n:2");
    assert!(!relationship.properties.contains_key("private"));
    let Value::Path(path) = &nested[2] else {
        panic!("path");
    };
    assert!(!path.nodes[0].properties.contains_key("private"));
    assert!(!path.relationships[0].properties.contains_key("private"));
    assert_ne!(value, filtered);
}

#[test]
fn keyword_function_syntax_and_interpolation_do_not_bypass_function_denials() {
    let connection = storage();
    for (name, expression) in [
        ("vector", "vector([1.0],1,FLOAT64)"),
        ("all", "all(x IN [1] WHERE true)"),
        ("any", "any(x IN [1] WHERE true)"),
        ("none", "none(x IN [1] WHERE true)"),
        ("single", "single(x IN [1] WHERE true)"),
        ("reduce", "reduce(s=0,x IN [1]|s+x)"),
        ("allReduce", "allReduce(s=0,x IN [1]|s+x,s<2)"),
    ] {
        let options = ExecutionOptions::parse(&json!({"readAccess":{"denyFunctions":[name]}}))
            .expect("known function");
        for query in [
            format!("RETURN {expression}"),
            format!("RETURN s\"{{{expression}}}\""),
        ] {
            let error =
                prepare(&connection, &query, BTreeMap::new(), options.clone()).expect_err(&query);
            assert_eq!(error.kind, QueryErrorKind::AccessDenied, "{query}: {error}");
        }
    }
}

fn default_rows(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect(query);
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 16).expect(query);
        rows.extend(batch.rows);
        if batch.done {
            break;
        }
    }
    cursor.complete(connection).expect(query);
    rows
}

fn assert_function_denied_after_successful_baseline(
    connection: &Connection,
    function: &str,
    query: &str,
    expected: Vec<Vec<Value>>,
) {
    assert_eq!(default_rows(connection, query), expected, "{query}");
    let options = ExecutionOptions::parse(&json!({"readAccess":{"denyFunctions":[function]}}))
        .expect("registered function");
    let error = prepare(connection, query, BTreeMap::new(), options).expect_err(query);
    assert_eq!(error.kind, QueryErrorKind::AccessDenied, "{query}: {error}");
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_AUTH));
}

#[test]
fn qualified_and_special_function_forms_are_denied_after_valid_baselines() {
    let connection = storage();
    default_rows(&connection, "CREATE (:Person {x:1}) FINISH");
    for (function, query, expected) in [
        (
            "db.nameFromElementId",
            "RETURN db.nameFromElementId('n:1')",
            Value::String("main".to_owned()),
        ),
        (
            "property_exists",
            "MATCH (n:Person) RETURN PROPERTY_EXISTS(n,x)",
            Value::Boolean(true),
        ),
        (
            "trim",
            "RETURN trim(LEADING 'x' FROM 'xxxA')",
            Value::String("A".to_owned()),
        ),
        (
            "normalize",
            "RETURN normalize('abc',NFC)",
            Value::String("abc".to_owned()),
        ),
        (
            "vector_distance",
            "RETURN vector_distance(vector([1.0],1,FLOAT64),vector([1.0],1,FLOAT64),EUCLIDEAN)",
            Value::Float(0.0),
        ),
        (
            "vector_norm",
            "RETURN vector_norm(vector([1.0],1,FLOAT64),EUCLIDEAN)",
            Value::Float(1.0),
        ),
    ] {
        assert_function_denied_after_successful_baseline(
            &connection,
            function,
            query,
            vec![vec![expected]],
        );
    }
}

#[test]
fn nested_function_denials_cover_union_subqueries_and_list_expressions() {
    let connection = storage();
    default_rows(&connection, "CREATE (:Person) FINISH");
    let element_id = Value::String("n:1".to_owned());
    for (query, expected) in [
        (
            "RETURN 'left' AS x UNION ALL MATCH(n:Person) RETURN elementId(n) AS x",
            vec![
                vec![Value::String("left".to_owned())],
                vec![element_id.clone()],
            ],
        ),
        (
            "RETURN COLLECT { MATCH(n:Person) RETURN elementId(n) }",
            vec![vec![Value::List(vec![element_id.clone()])]],
        ),
        (
            "RETURN COUNT { MATCH(n:Person) WHERE elementId(n) = 'n:1' RETURN n }",
            vec![vec![Value::Integer(1)]],
        ),
        (
            "CALL { MATCH(n:Person) RETURN elementId(n) AS x } RETURN x",
            vec![vec![element_id.clone()]],
        ),
        (
            "MATCH(n:Person) RETURN all(x IN [n] WHERE elementId(x) IS NOT NULL)",
            vec![vec![Value::Boolean(true)]],
        ),
        (
            "MATCH(n:Person) RETURN [x IN [n] | elementId(x)]",
            vec![vec![Value::List(vec![element_id.clone()])]],
        ),
    ] {
        assert_function_denied_after_successful_baseline(&connection, "elementId", query, expected);
    }
}

#[test]
fn keyword_functions_keep_their_identity_with_nested_function_arguments() {
    let connection = storage();
    default_rows(&connection, "CREATE (:Person) FINISH");
    for (function, query, expected) in [
        (
            "elementId",
            "MATCH(n:Person) RETURN reduce(s='',x IN [n] | s+elementId(x))",
            Value::String("n:1".to_owned()),
        ),
        (
            "toInteger",
            "RETURN allReduce(s=0,x IN [1,2] | s+toInteger(x),s<4)",
            Value::Boolean(true),
        ),
        (
            "toInteger",
            "RETURN all(x IN [1,2] WHERE toInteger(x)>0)",
            Value::Boolean(true),
        ),
    ] {
        assert_function_denied_after_successful_baseline(
            &connection,
            function,
            query,
            vec![vec![expected]],
        );
    }
}

#[test]
fn simultaneous_prepared_queries_keep_separate_access_policies() {
    let connection = storage();
    let created = prepare(
        &connection,
        "CREATE (:Person {secret:1,public:2}),(:Person {secret:1,public:2}) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("create");
    let mut created = QueryCursor::new(created);
    while !created
        .next_batch(&connection, 16)
        .expect("create rows")
        .done
    {}
    created.complete(&connection).expect("create complete");
    let restricted = ExecutionOptions::parse(&json!({"readAccess":{"query":{"denyProperties":["secret"]},"result":{"denyProperties":["public"]}}})).expect("policy");
    let query = "MATCH (n:Person) RETURN n.secret AS secret,n.public AS public,n";
    let restricted = prepare(&connection, query, BTreeMap::new(), restricted).expect("restricted");
    let unrestricted = prepare(
        &connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("unrestricted");
    assert!(restricted.access_policy().read.result.denies("public"));
    assert!(unrestricted.access_policy().is_unrestricted());
    let mut restricted = QueryCursor::new(restricted);
    let mut unrestricted = QueryCursor::new(unrestricted);
    for _ in 0..2 {
        let hidden = restricted
            .next_batch(&connection, 1)
            .expect("restricted batch");
        let real = unrestricted
            .next_batch(&connection, 1)
            .expect("unrestricted batch");
        assert_eq!(hidden.rows[0][0], Value::Null);
        assert_eq!(hidden.rows[0][1], Value::Integer(2));
        assert_eq!(real.rows[0][0], Value::Integer(1));
        let Value::Node(hidden_node) = &hidden.rows[0][2] else {
            panic!("node");
        };
        let Value::Node(real_node) = &real.rows[0][2] else {
            panic!("node");
        };
        assert!(!hidden_node.properties.contains_key("secret"));
        assert!(real_node.properties.contains_key("secret"));
    }
    assert!(
        restricted
            .next_batch(&connection, 1)
            .expect("end restricted")
            .done
    );
    assert!(
        unrestricted
            .next_batch(&connection, 1)
            .expect("end unrestricted")
            .done
    );
    restricted
        .complete(&connection)
        .expect("restricted complete");
    unrestricted
        .complete(&connection)
        .expect("unrestricted complete");
}

#[test]
fn every_registered_procedure_checks_named_and_effect_permissions_before_invocation() {
    let connection = storage();
    let prepared = prepare(
        &connection,
        "SHOW PROCEDURES YIELD name RETURN name",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("actual Procedure inventory");
    let mut cursor = QueryCursor::new(prepared);
    let mut names = std::collections::BTreeSet::new();
    loop {
        let batch = cursor.next_batch(&connection, 16).expect("inventory rows");
        for row in batch.rows {
            let [Value::String(name)] = row.as_slice() else {
                panic!("Procedure name");
            };
            names.insert(name.clone());
        }
        if batch.done {
            break;
        }
    }
    cursor.complete(&connection).expect("inventory complete");
    assert_eq!(names.len(), 38);
    let named = json!({"readAccess":{"allowNamedProcedures":false}});
    for name in &names {
        assert_procedure_denied_before_invocation(&connection, name, &named);
    }

    let groups = [
        (
            "readAccess",
            "allowExternalIO",
            "db.index.fulltext.queryNodes db.index.fulltext.queryRelationships db.index.semantic.queryNodes db.index.semantic.queryRelationships db.index.semantic.createNodeIndex db.index.semantic.createRelationshipIndex db.index.semantic.rebuild",
        ),
        (
            "writeAccess",
            "allowSchemaChanges",
            "db.index.semantic.createNodeIndex db.index.semantic.createRelationshipIndex",
        ),
        (
            "writeAccess",
            "allowVersionChanges",
            "lithograph.branch.create lithograph.branch.checkout lithograph.branch.delete lithograph.commit.create lithograph.commit.data.set lithograph.commit.data.clear lithograph.tag.create lithograph.tag.move lithograph.tag.delete lithograph.patch.apply lithograph.merge.start lithograph.merge.resolve lithograph.merge.finalize lithograph.merge.abort lithograph.rebase lithograph.squash lithograph.reset lithograph.revert",
        ),
        (
            "writeAccess",
            "allowMaintenance",
            "db.index.semantic.rebuild lithograph.index.rebuild lithograph.gc",
        ),
    ];
    let mut classified = std::collections::BTreeSet::new();
    for (section, permission, procedures) in groups {
        let options = json!({(section):{(permission):false}});
        for name in procedures.split_ascii_whitespace() {
            assert!(
                names.contains(name),
                "unregistered Procedure in effect matrix: {name}"
            );
            assert_procedure_denied_before_invocation(&connection, name, &options);
            classified.insert(name.to_owned());
        }
    }
    let read_only = "db.labels db.propertyKeys db.relationshipTypes lithograph.branch.list lithograph.commit.get lithograph.tag.list lithograph.log lithograph.diff lithograph.merge.get lithograph.merge.list lithograph.merge.conflicts";
    for name in read_only.split_ascii_whitespace() {
        assert!(names.contains(name), "unregistered read Procedure: {name}");
        assert!(
            classified.insert(name.to_owned()),
            "read Procedure has an unexpected effect: {name}"
        );
    }
    assert_eq!(
        classified, names,
        "every registered Procedure needs an explicit effect or read classification"
    );
}

fn assert_procedure_denied_before_invocation(
    connection: &Connection,
    name: &str,
    options: &serde_json::Value,
) {
    // Most registered Procedures require arguments. ACCESS_DENIED on this
    // zero-argument call proves authorization precedes invocation/argument use.
    let query = format!("CALL {name}()");
    let options = ExecutionOptions::parse(options).expect("valid permission policy");
    let error = prepare(connection, &query, BTreeMap::new(), options).expect_err(name);
    assert_eq!(error.kind, QueryErrorKind::AccessDenied, "{name}: {error}");
    assert_eq!(
        error.sqlite_code,
        Some(rusqlite::ffi::SQLITE_AUTH),
        "{name}"
    );
}
