use super::*;

#[test]
fn linear_program_pipeline_exposes_early_rows_before_late_runtime_failure() {
    let connection = fresh_storage();
    let prepared = prepare(
        &connection,
        "UNWIND [1, 0] AS step RETURN range(1, 2, step) AS values",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare streamable program");
    let mut cursor = QueryCursor::new(prepared);

    let first = cursor
        .next_batch(&connection, 1)
        .expect("first stream row must be available before evaluating the second input");
    assert_eq!(
        first.rows,
        vec![vec![Value::List(vec![
            Value::Integer(1),
            Value::Integer(2),
        ])]]
    );
    assert!(!first.done);

    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("zero range step must fail only when the second input is pulled");
    assert!(
        error.to_string().contains("step") || error.to_string().contains("zero"),
        "unexpected late runtime error: {error}"
    );
}
