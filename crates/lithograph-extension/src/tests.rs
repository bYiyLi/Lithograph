use super::*;

#[test]
fn uuid_validation_accepts_rfc9562_v4_shape() {
    assert!(is_canonical_uuid("550e8400-e29b-41d4-a716-446655440000"));
    assert!(!is_canonical_uuid("550e8400-e29b-11d4-a716-446655440000"));
    assert!(!is_canonical_uuid("550E8400-E29B-41D4-A716-446655440000"));
}

#[test]
fn error_json_has_stable_shape() {
    let value: Value = serde_json::from_str(&LithographError::not_initialized().to_json())
        .expect("error JSON must be valid");
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
fn native_input_utf8_copies_and_validates_input() {
    let bytes = b"RETURN 1";
    // SAFETY: `bytes` is a live local buffer for the full call.
    let value = unsafe {
        input_utf8(bytes.as_ptr().cast::<c_char>(), bytes.len(), "query")
            .expect("valid UTF-8 must be accepted")
    };
    assert_eq!(value, "RETURN 1");

    let invalid = [0xff_u8];
    // SAFETY: `invalid` is a live one-byte local buffer for the full call.
    let error = unsafe {
        input_utf8(invalid.as_ptr().cast::<c_char>(), invalid.len(), "query")
            .expect_err("invalid UTF-8 must be rejected")
    };
    assert_eq!(error.category, ErrorCategory::InvalidArgument);

    // SAFETY: NULL is intentionally supplied with non-zero length to
    // exercise misuse validation without dereferencing it.
    let error = unsafe {
        input_utf8(ptr::null(), 1, "query").expect_err("NULL plus length must be misuse")
    };
    assert_eq!(error.sqlite_code, ffi::SQLITE_MISUSE);
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
