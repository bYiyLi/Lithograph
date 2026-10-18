use std::cell::Cell;

use super::*;

fn interrupted_after<'a>(checks: &'a Cell<u64>, threshold: u64) -> impl Fn() -> bool + 'a {
    move || {
        let next = checks.get() + 1;
        checks.set(next);
        next >= threshold
    }
}

#[test]
fn token_scan_interrupts_a_single_large_serialized_string_before_decoding() {
    let resources = QueryResources::default();
    let checks = Cell::new(0);
    let interrupted = interrupted_after(&checks, 3);
    let control = ExecutionControl::new(&resources, &interrupted);
    let encoded = format!("[\"{}\"]", "x".repeat(256 * 1024));
    let error = decoding_bytes(&encoded, &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(checks.get(), 3);
    assert_eq!(
        resources.peak(),
        0,
        "token scan must not allocate a DOM first"
    );
}

#[test]
fn distinct_key_interrupts_inside_one_nested_collection_element() {
    let resources = QueryResources::default();
    let memory = resources.reserve(4 * 1024 * 1024).unwrap();
    let nested = Value::List((0..20_000).map(Value::Integer).collect());
    let checks = Cell::new(0);
    let interrupted = interrupted_after(&checks, 3);
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = distinct_row_key(&[nested], Some(&control)).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(checks.get(), 3);
    drop(memory);
    assert_eq!(resources.used(), 0);
}

#[test]
fn row_codec_checks_large_owned_text_and_long_collection_decoding() {
    let resources = QueryResources::default();
    let row = vec![Value::String("x".repeat(256 * 1024))];
    let checks = Cell::new(0);
    let interrupted = interrupted_after(&checks, 3);
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = encode_row(&row, &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    let plain = ExecutionControl::new(&resources, &|| false);
    let nested = vec![Value::List((0..20_000).map(Value::Integer).collect())];
    let encoded = encode_row(&nested, &plain).unwrap();
    let checks = Cell::new(0);
    let interrupted = interrupted_after(&checks, 4);
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = decode_row(&encoded, &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(resources.used(), 0);
}

#[test]
fn long_sort_key_interrupt_releases_chunk_claims_and_all_temp_tables() {
    let connection = open_spill_connection().unwrap();
    let resources = QueryResources::default();
    let plain = ExecutionControl::new(&resources, &|| false);
    let mut spill =
        SortSpill::create_with_resources(&connection, false, vec![false], resources.clone())
            .unwrap();
    let key = Value::List((0..20_000).map(Value::Integer).collect());
    spill
        .push(&connection, vec![Value::Integer(1)], vec![key], &plain)
        .unwrap();
    let checks = Cell::new(0);
    let interrupted = interrupted_after(&checks, 3);
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = spill.finish(&connection, &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(checks.get(), 3);
    spill.abort(&connection).unwrap();
    drop(spill);
    assert_eq!(resources.used(), 0);
    let tables: i64 = connection
        .query_row(
            "SELECT count(*) FROM temp.sqlite_schema WHERE name LIKE '_lithograph_query_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
}

#[test]
fn cancelled_binding_encoding_does_not_publish_a_partial_row() {
    let connection = open_spill_connection().unwrap();
    let resources = QueryResources::default();
    let mut spill = BindingSpill::create_with_resources(&connection, resources.clone()).unwrap();
    let mut row = BindingRow::default();
    row.insert(
        "value".to_owned(),
        BindingValue::Scalar(Value::List((0..20_000).map(Value::Integer).collect())),
    );
    let checks = Cell::new(0);
    let interrupted = interrupted_after(&checks, 3);
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = spill.push(&connection, &row, &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(spill.len(), 0);
    assert_eq!(resources.used(), 0);
    spill.abort(&connection).unwrap();
}
