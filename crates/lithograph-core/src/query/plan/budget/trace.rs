//! Explicitly enabled private unit evidence; absent from normal artifacts.

use super::PlanningBudget;
use std::cell::RefCell;

#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) clause_start: usize,
    pub(crate) stage: &'static str,
    pub(crate) states: usize,
    pub(crate) bytes: usize,
    pub(crate) candidates: usize,
    pub(crate) exhausted: bool,
    pub(crate) remaining_states: usize,
    pub(crate) remaining_bytes: usize,
}

thread_local! { static TRACE: RefCell<Option<Vec<Entry>>> = const { RefCell::new(None) }; }

pub(crate) fn start() {
    TRACE.set(Some(Vec::new()));
}
pub(crate) fn enabled() -> bool {
    TRACE.with_borrow(Option::is_some)
}
pub(crate) fn event(message: std::fmt::Arguments<'_>) {
    if enabled() {
        eprintln!("{message}");
    }
}
pub(crate) fn take() -> Vec<Entry> {
    TRACE.take().unwrap_or_default()
}

pub(super) fn record(budget: &PlanningBudget, clause_start: usize, stage: &'static str) {
    TRACE.with_borrow_mut(|trace| {
        if let Some(trace) = trace {
            trace.push(Entry {
                clause_start,
                stage,
                states: budget.states,
                bytes: budget.bytes,
                candidates: budget.candidates,
                exhausted: budget.exhausted(),
                remaining_states: budget.remaining_states(),
                remaining_bytes: budget.remaining_bytes(),
            });
        }
    });
}
