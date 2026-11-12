use std::cell::Cell;

use super::*;
use crate::query::expression::Expr;
use crate::query::{QueryErrorKind, QueryResources};

fn producer(control: &ExecutionControl<'_>) -> FactorProducer {
    let base = BindingRow::default();
    let plan_bytes = checked_add(
        allocation_bytes::<FactorizedPlan>(1).unwrap(),
        allocation_bytes::<usize>(2).unwrap() + 1,
    )
    .unwrap();
    let bytes = checked_add(
        allocation_bytes::<FactorProducer>(1).unwrap(),
        checked_add(expression::binding_row_bytes(&base).unwrap(), plan_bytes).unwrap(),
    )
    .unwrap();
    let memory = control
        .reserve(bytes)
        .expect("prepay the private producer fixture");
    FactorProducer {
        step: MatchStep {
            clause_start: 0,
            optional: false,
            parts: Vec::new(),
            predicate: None,
            early_predicates: Vec::new(),
            join: None,
        },
        plan: Arc::new(FactorizedPlan {
            clause_start: 0,
            shared: "h".to_owned(),
            accumulate: FactorArm { hops: Vec::new() },
            build: FactorArm { hops: Vec::new() },
            bindings: Vec::new(),
            single_shared: true,
            shared_predicate: Expr::Literal(Value::Boolean(true)),
            asp: None,
        }),
        base,
        done: false,
        stats: FactorizedRuntimeMetrics::default(),
        _memory: memory,
    }
}

fn input(node: i64, control: &ExecutionControl<'_>) -> (BindingRow, MemoryReservation) {
    let bytes = allocation_bytes::<BindingRow>(1).unwrap()
        + 2 * map_entry_bytes::<BindingValue>().unwrap()
        + allocation_bytes::<String>(2).unwrap()
        + 4;
    let mut memory = control.reserve(bytes).expect("prepay both Node bindings");
    let mut row = BindingRow {
        order: Vec::with_capacity(2),
        ..BindingRow::default()
    };
    row.insert("h".to_owned(), BindingValue::Node(1));
    row.insert("a".to_owned(), BindingValue::Node(node));
    memory
        .resize(expression::binding_row_bytes_controlled(&row, Some(control)).unwrap())
        .unwrap();
    (row, memory)
}

#[test]
fn cancellation_after_eight_accepted_accumulator_rows_is_terminal_and_releases_state() {
    // This exercises the actual accepted-row accumulator, not selector proof,
    // Source EOF, or an entire QueryCursor's execution lifecycle.
    let resources = QueryResources::default();
    let interrupted = Cell::new(false);
    let callback = || interrupted.get();
    let control = ExecutionControl::new(&resources, &callback);
    let mut producer = producer(&control);
    let mut completed = None;
    let mut accepted = 0;
    let mut first_row_bytes = 0;
    for (ordinal, node) in (10..18).enumerate() {
        producer
            .append_accumulated(&mut completed, input(node, &control), &control)
            .expect("the real accumulator accepts the complete correlated delta");
        accepted = ordinal + 1;
        if accepted == 1 {
            first_row_bytes = completed.as_ref().unwrap().0.resident_bytes().unwrap();
        }
    }
    assert_eq!(accepted, 8);
    assert!(first_row_bytes > 0);
    assert!(completed.as_ref().unwrap().0.resident_bytes().unwrap() > first_row_bytes);

    let ninth = input(18, &control);
    interrupted.set(true);
    let error = producer
        .append_accumulated(&mut completed, ninth, &control)
        .expect_err("cancel after eight actual accepted rows, before accepting the ninth");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    interrupted.set(false);
    let (builder, shared) = completed
        .take()
        .expect("the terminal builder remains owned");
    assert_eq!(shared, 1);
    let closed = builder
        .finish(&control)
        .expect_err("terminal accumulation cannot resume or yield a partial group");
    assert_eq!(closed.kind, QueryErrorKind::Internal);
    assert_eq!(closed.message, "factorized builder is closed");
    drop((completed, producer));
    assert_eq!(resources.used(), 0);
}
