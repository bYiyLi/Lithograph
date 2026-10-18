use std::cell::Cell;
use std::collections::BTreeMap;

use lithograph_core::cypher::{
    TimeValue, Value, ZonedDateTimeValue, decode_json, decode_json_checked, decode_parameters,
};
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, prepare};
use lithograph_core::storage::{create_storage_schema, initialize_root};
use rusqlite::Connection;
use serde_json::json;

#[test]
fn time_offsets_preserve_all_supported_forms_and_typed_json() {
    for (offset, canonical) in [
        ("Z", "Z"),
        ("+00", "Z"),
        ("-00:00:00", "Z"),
        ("+08", "+08:00"),
        ("-08", "-08:00"),
        ("+0830", "+08:30"),
        ("-0830", "-08:30"),
        ("+083045", "+08:30:45"),
        ("-083045", "-08:30:45"),
        ("+08:30", "+08:30"),
        ("-08:30", "-08:30"),
        ("+08:30:45", "+08:30:45"),
        ("-08:30:45", "-08:30:45"),
        ("+18:00:00", "+18:00"),
        ("-18:00:00", "-18:00"),
    ] {
        let input = format!("12:34:56.123456789{offset}");
        let expected = format!("12:34:56.123456789{canonical}");
        let value = TimeValue::parse(&input).expect("supported offset");
        assert_eq!(value.as_str(), expected);
        assert_eq!(
            decode_json(&json!({"$type": "Time", "value": input})).expect("typed Time"),
            Value::Time(value)
        );
    }
}

#[test]
fn time_offsets_reject_unicode_and_invalid_fields_without_panicking() {
    for offset in [
        "+€x",
        "+€abc",
        "+é00",
        "-界abc",
        "+1",
        "+01:02:",
        "+01:02:03:04",
        "+18:00:01",
        "-18:01",
        "+19",
        "+01:60",
        "+01:02:60",
    ] {
        let input = format!("12:34:56{offset}");
        let error = TimeValue::parse(&input).expect_err("invalid offset");
        assert!(error.message.len() < 160);
        let error = decode_parameters(&json!({"t": {"$type": "Time", "value": input}}))
            .expect_err("invalid typed Time");
        assert!(error.message.len() < 160);
    }
}

#[test]
fn overlong_time_and_fixed_zone_errors_remain_bounded() {
    let offset = format!("+{}", ":".repeat(4096));
    let input = format!("12:34:56{offset}");
    let error = TimeValue::parse(&input).expect_err("overlong Time");
    assert!(error.message.len() < 160);
    let error = decode_parameters(&json!({"t": {"$type": "Time", "value": input}}))
        .expect_err("overlong typed Time");
    assert!(error.message.len() < 160);
    let error = ZonedDateTimeValue::parse("2026-10-18T12:34:56Z", &offset)
        .expect_err("overlong fixed zone");
    assert!(error.message.len() < 160);
}

#[test]
fn typed_time_decode_checks_cancellation_before_parsing() {
    let checks = Cell::new(0);
    let raw = json!({"$type": "Time", "value": "12:34:56+€x"});
    let error = decode_json_checked(&raw, &mut |_| {
        checks.set(checks.get() + 1);
        Err::<(), _>(QueryError::interrupted())
    })
    .expect_err("cancellation takes precedence over invalid Time");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(checks.get(), 1);
}

#[test]
fn time_constructor_rejects_bad_offsets_with_bounded_errors_and_releases_budget() {
    let connection = Connection::open_in_memory().expect("SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    for input in [
        "12:34:56+€x".to_owned(),
        format!("12:34:56+{}", ":".repeat(4096)),
    ] {
        let options =
            ExecutionOptions::parse(&json!({"memoryLimitBytes": 1024 * 1024})).expect("budget");
        let prepared = prepare(
            &connection,
            "RETURN time($input)",
            BTreeMap::from([("input".to_owned(), Value::String(input))]),
            options,
        )
        .expect("prepare");
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        let error = cursor.next_batch(&connection, 1).expect_err("invalid Time");
        assert_eq!(error.kind, QueryErrorKind::Type);
        assert!(error.message.len() < 256);
        cursor.cancel(&connection).expect("cleanup");
        drop(cursor);
        assert_eq!(resources.used(), 0);
        assert!(resources.peak() <= 1024 * 1024);
    }
}
