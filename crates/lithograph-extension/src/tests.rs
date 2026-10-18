use super::*;

#[test]
fn uuid_validation_accepts_rfc9562_v4_shape() {
    assert!(is_canonical_uuid("550e8400-e29b-41d4-a716-446655440000"));
    assert!(!is_canonical_uuid("550e8400-e29b-11d4-a716-446655440000"));
    assert!(!is_canonical_uuid("550E8400-E29B-41D4-A716-446655440000"));
}

#[test]
fn error_json_has_stable_shape() {
    let value = LithographError::not_initialized().to_json_value();
    assert_eq!(value["category"], "NOT_INITIALIZED");
    assert_eq!(value["sqliteCode"], ffi::SQLITE_ERROR);
    assert!(value["line"].is_null());
    assert!(value["column"].is_null());
}

#[test]
fn summary_json_exposes_execution_metrics_on_shared_adapter_surface() {
    let mut metrics = query::QueryMetrics::default();
    metrics.rows = 3;
    metrics.db_hits = 7;
    metrics.elapsed_micros = 11;
    let summary = query::QuerySummary {
        query_type: query::QueryType::Read,
        commit: Some("commit/probe".to_owned()),
        merge_session: None,
        counters: query::QueryCounters::default(),
        metrics,
    };
    let value = execution::summary_json(&summary);
    assert_eq!(value["metrics"]["rows"], 3);
    assert_eq!(value["metrics"]["dbHits"], 7);
    assert_eq!(value["metrics"]["elapsedMicros"], 11);
    assert!(value.get("profile").is_none());
}

#[test]
fn json_adapter_requires_object_inputs() {
    assert!(validate_json_object("{}", "params").is_ok());
    assert_eq!(
        validate_json_object("[]", "params")
            .expect_err("array must be rejected")
            .category,
        ErrorCategory::InvalidArgument
    );
}

#[test]
fn sqlite_runtime_version_guard_matches_supported_baseline() {
    assert!(!sqlite_version_number_supported(3_044_999));
    assert!(sqlite_version_number_supported(3_045_000));
    assert!(sqlite_version_number_supported(3_051_000));
}

#[test]
fn sqlite_error_mapping_preserves_stable_primary_categories() {
    let cases = [
        (ffi::SQLITE_BUSY, ErrorCategory::Busy),
        (ffi::SQLITE_LOCKED, ErrorCategory::Busy),
        (ffi::SQLITE_NOMEM, ErrorCategory::Resource),
        (ffi::SQLITE_TOOBIG, ErrorCategory::Resource),
        (ffi::SQLITE_FULL, ErrorCategory::Resource),
        (ffi::SQLITE_IOERR, ErrorCategory::Io),
        (ffi::SQLITE_CANTOPEN, ErrorCategory::Io),
        (ffi::SQLITE_READONLY, ErrorCategory::Io),
        (ffi::SQLITE_ERROR, ErrorCategory::Storage),
    ];
    for (code, expected) in cases {
        let error = SqliteError::SqliteFailure(ffi::Error::new(code), None);
        assert_eq!(map_sqlite_error(error, "probe").category, expected);
    }
}

#[test]
fn query_error_mapping_preserves_busy_and_io_categories() {
    for (kind, code) in [
        (query::QueryErrorKind::Busy, ffi::SQLITE_BUSY),
        (query::QueryErrorKind::Busy, ffi::SQLITE_LOCKED),
    ] {
        let query_error = query::QueryError {
            kind,
            message: "probe".to_owned(),
            line: None,
            column: None,
            sqlite_code: Some(code),
        };
        let mapped = execution::map_query_error(query_error);
        assert_eq!(mapped.category, ErrorCategory::Busy);
        assert_eq!(mapped.message, "query storage operation is busy");
        assert_eq!(mapped.sqlite_code, code);
    }
    for (code, expected) in [
        (ffi::SQLITE_BUSY, ErrorCategory::Busy),
        (ffi::SQLITE_LOCKED, ErrorCategory::Busy),
        (ffi::SQLITE_IOERR, ErrorCategory::Io),
        (ffi::SQLITE_CANTOPEN, ErrorCategory::Io),
        (ffi::SQLITE_READONLY, ErrorCategory::Io),
    ] {
        let query_error = query::QueryError {
            kind: query::QueryErrorKind::Storage,
            message: "probe".to_owned(),
            line: None,
            column: None,
            sqlite_code: Some(code),
        };
        assert_eq!(execution::map_query_error(query_error).category, expected);
    }
}

#[test]
fn query_error_mapping_preserves_interrupted_category_and_sqlite_code() {
    let mapped = execution::map_query_error(query::QueryError {
        kind: query::QueryErrorKind::Interrupted,
        message: "probe".to_owned(),
        line: None,
        column: None,
        sqlite_code: Some(ffi::SQLITE_INTERRUPT),
    });
    assert_eq!(mapped.category, ErrorCategory::Interrupted);
    assert_eq!(mapped.sqlite_code, ffi::SQLITE_INTERRUPT);
    assert_eq!(mapped.public_message(), "LITHOGRAPH_INTERRUPTED: probe");
}

#[test]
fn public_sql_error_preserves_source_location_suffix() {
    let mapped = execution::map_query_error(query::QueryError {
        kind: query::QueryErrorKind::Parse,
        message: "probe".to_owned(),
        line: Some(2),
        column: Some(7),
        sqlite_code: None,
    });
    assert_eq!(
        mapped.public_message(),
        "LITHOGRAPH_PARSE_ERROR: probe [line=2,column=7]"
    );
}

#[test]
fn virtual_table_panic_guard_converts_panics_to_internal_errors() {
    let error = catch_sqlite_boundary::<()>(|| panic!("boundary probe"))
        .expect_err("panic must become a SQLite error");
    match error {
        SqliteError::SqliteFailure(_, Some(message)) => {
            assert!(message.contains("LITHOGRAPH_INTERNAL_ERROR"));
        }
        other => panic!("unexpected panic guard result: {other:?}"),
    }
}

#[test]
fn scalar_panic_guard_converts_panics_to_internal_errors() {
    let error = scalar::catch_scalar_operation::<()>(|| panic!("scalar boundary probe"))
        .expect_err("panic must become a Lithograph error");
    assert_eq!(error.category, ErrorCategory::Internal);
    assert!(error.message.contains("SQLite scalar callback"));
}

#[test]
fn query_access_denied_preserves_category_and_sqlite_auth() {
    for sqlite_code in [None, Some(ffi::SQLITE_AUTH)] {
        let mapped = execution::map_query_error(query::QueryError {
            kind: query::QueryErrorKind::AccessDenied,
            message: "protected operation".to_owned(),
            line: None,
            column: None,
            sqlite_code,
        });
        assert_eq!(mapped.category, ErrorCategory::AccessDenied);
        assert_eq!(mapped.sqlite_code, ffi::SQLITE_AUTH);
        assert_eq!(
            mapped.public_message(),
            "LITHOGRAPH_ACCESS_DENIED: protected operation"
        );
        let json = mapped.to_json_value();
        assert_eq!(json["category"], "ACCESS_DENIED");
        assert_eq!(json["sqliteCode"], ffi::SQLITE_AUTH);
    }
}

#[test]
fn explicit_transaction_forwards_per_execution_access_rules() {
    let options = json!({
        "graphView": {"nodeLabels": ["Person"]},
        "readOnlyNoExternalIO": false,
        "readAccess": {"result": {"denyProperties": ["secret"]}},
        "writeAccess": {"allowDelete": false},
    });
    let forwarded: Value = serde_json::from_str(
        &transaction::execution_options(&options.to_string(), "main").expect("valid options"),
    )
    .expect("forwarded JSON");
    assert_eq!(forwarded["branch"], "main");
    for key in [
        "graphView",
        "readOnlyNoExternalIO",
        "readAccess",
        "writeAccess",
    ] {
        assert_eq!(forwarded[key], options[key]);
    }
    let empty: Value =
        serde_json::from_str(&transaction::execution_options("{}", "main").expect("empty options"))
            .expect("forwarded empty JSON");
    assert_eq!(empty, json!({"branch": "main"}));
    let error = transaction::execution_options(r#"{"branch":"other"}"#, "main")
        .expect_err("execution cannot change the explicit transaction Branch");
    assert_eq!(error.category, ErrorCategory::InvalidArgument);
}

#[test]
fn output_encoding_filters_only_graph_properties_without_changing_runtime_values() {
    use std::collections::BTreeMap;

    use cypher::Value as CypherValue;

    let node = cypher::NodeValue {
        element_id: "n:1".to_owned(),
        labels: vec!["Person".to_owned()],
        properties: BTreeMap::from([
            ("name".to_owned(), CypherValue::String("Alice".to_owned())),
            ("secret".to_owned(), CypherValue::Integer(7)),
            ("hidden".to_owned(), CypherValue::Integer(9)),
        ]),
    };
    let relationship = cypher::RelationshipValue {
        element_id: "r:1".to_owned(),
        relationship_type: "KNOWS".to_owned(),
        start: "n:1".to_owned(),
        end: "n:2".to_owned(),
        properties: node.properties.clone(),
    };
    let path = cypher::PathValue {
        nodes: vec![node.clone()],
        relationships: vec![relationship.clone()],
    };
    let row = vec![
        CypherValue::Node(node.clone()),
        CypherValue::Relationship(relationship),
        CypherValue::Path(path),
        CypherValue::Map(BTreeMap::from([
            ("secret".to_owned(), CypherValue::Integer(7)),
            (
                "nested".to_owned(),
                CypherValue::List(vec![CypherValue::Node(node)]),
            ),
        ])),
        CypherValue::Integer(7),
    ];
    let original = row.clone();
    let options = query::ExecutionOptions::parse_text(
        r#"{
        "readAccess": {
            "query": {"denyProperties": ["hidden"]},
            "result": {"denyProperties": ["secret"]}
        }
    }"#,
    )
    .expect("access options");
    let encoded = execution::row_json(&row, &options.access);
    let expected_properties = json!({"name": "Alice"});
    assert_eq!(encoded[0]["properties"], expected_properties);
    assert_eq!(encoded[1]["properties"], expected_properties);
    assert_eq!(encoded[2]["nodes"][0]["properties"], expected_properties);
    assert_eq!(
        encoded[2]["relationships"][0]["properties"],
        expected_properties
    );
    assert_eq!(encoded[3]["secret"], 7);
    assert_eq!(encoded[3]["nested"][0]["properties"], expected_properties);
    assert_eq!(encoded[4], 7);
    assert_eq!(encoded[0]["elementId"], "n:1");
    assert_eq!(encoded[0]["labels"], json!(["Person"]));
    assert_eq!(encoded[1]["type"], "KNOWS");
    assert_eq!(encoded[1]["start"], "n:1");
    assert_eq!(encoded[1]["end"], "n:2");
    assert_eq!(row, original);
    assert_eq!(
        execution::row_json(&row, &query::AccessPolicy::default()),
        Value::Array(row.iter().map(cypher::encode_json).collect()),
    );
}
