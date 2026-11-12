use super::*;
use crate::cypher::Span;
use crate::query::{QueryErrorKind, QueryResources};

fn node() -> AstNode {
    AstNode {
        kind: AstKind::QueryBody,
        span: Span { start: 0, end: 0 },
        text: None,
        children: Vec::new(),
    }
}

#[test]
fn long_identifier_is_rejected_before_optional_name_comparisons() {
    let mut syntax = node();
    syntax.text = Some("v".repeat(300 * 1024));
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut budget = PlanningBudget::default();
    assert!(!inspect_shape(&syntax, 0, &mut budget, &control).unwrap());
    assert!(budget.exhausted());
    assert_eq!(resources.used(), 0);
}

#[test]
fn optional_shape_uses_remaining_global_states_and_a_bounded_stack() {
    let mut syntax = node();
    syntax.children.push(node());
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut budget = PlanningBudget::default();
    assert!(budget.work(4095, 0));
    assert!(!inspect_shape(&syntax, 0, &mut budget, &control).unwrap());
    assert!(budget.exhausted());
    for _ in 0..=crate::query::plan::MAX_DEPTH {
        let mut parent = node();
        parent.children.push(syntax);
        syntax = parent;
    }
    assert!(!inspect_shape(&syntax, 0, &mut PlanningBudget::default(), &control).unwrap());
    assert_eq!(resources.used(), 0);
}

#[test]
fn shape_cancellation_remains_an_error_instead_of_optional_fallback() {
    let mut syntax = node();
    syntax.children = (0..300).map(|_| node()).collect();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| true);
    let error = inspect_shape(&syntax, 0, &mut PlanningBudget::default(), &control)
        .expect_err("cooperative AST checkpoint must preserve interruption");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert_eq!(resources.used(), 0);
}
