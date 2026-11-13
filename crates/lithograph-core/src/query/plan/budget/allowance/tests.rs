use super::*;

#[test]
fn local_fallback_history_survives_exit_without_stopping_later_clauses() {
    let mut budget = PlanningBudget::default();
    budget.begin_clause(2);
    assert!(budget.work(MAX_STATES / 2, 0));
    assert!(!budget.work(1, 0));
    assert!(!budget.had_fallback);
    budget.end_clause();
    assert!(budget.had_fallback);
    assert!(!budget.exhausted());
    budget.begin_clause(2);
    assert!(budget.work(1, 1));
    budget.end_clause();
    assert!(budget.had_fallback);
    assert!(!budget.exhausted());
    assert_eq!((budget.states, budget.bytes), (MAX_STATES / 2 + 1, 1));
}

#[test]
fn fully_used_allowances_without_a_refusal_do_not_report_fallback() {
    let mut budget = PlanningBudget::default();
    for _ in 0..2 {
        budget.begin_clause(2);
        assert!(budget.work(MAX_STATES / 2, MAX_BYTES / 2));
        assert_eq!(
            (budget.remaining_states(), budget.remaining_bytes()),
            (0, 0)
        );
        budget.end_clause();
        assert!(!budget.had_fallback);
        assert!(!budget.exhausted());
    }
    assert_eq!((budget.states, budget.bytes), (MAX_STATES, MAX_BYTES));
}

#[cfg(feature = "test-support")]
#[test]
fn dropping_local_fallbacks_reports_once_with_or_without_clause_exit() {
    crate::performance::set_enabled(true);
    for exit_clause in [false, true] {
        crate::performance::reset();
        {
            let mut budget = PlanningBudget::default();
            for _ in 0..2 {
                budget.begin_clause(3);
                assert!(budget.work(MAX_STATES / 3, 0));
                assert!(!budget.work(1, 0));
                if exit_clause {
                    budget.end_clause();
                } else {
                    break;
                }
            }
        }
        let counters = crate::performance::snapshot();
        assert_eq!(counters.planner_fallbacks, 1);
        assert_eq!(counters.planner_candidates, 0);
        assert_eq!(counters.planner_bytes, 0);
        assert_eq!(
            counters.planner_states,
            (MAX_STATES / 3 * if exit_clause { 2 } else { 1 }) as u64
        );
    }
    crate::performance::set_enabled(false);
}

#[test]
fn already_soft_stopped_requests_cannot_poison_the_other_resource() {
    let cases = [
        ((MAX_STATES - 1, 0), (0, MAX_BYTES / 2 + 1), (2, 0)),
        ((0, MAX_BYTES - 1), (MAX_STATES / 2 + 1, 0), (0, 2)),
    ];
    for (spent, soft, later) in cases {
        let mut budget = PlanningBudget::default();
        assert!(budget.work(spent.0, spent.1));
        budget.begin_clause(2);
        assert!(!budget.work(soft.0, soft.1));
        assert!(budget.exhausted());
        assert!(!budget.exhausted);
        assert!(!budget.work(later.0, later.1));
        assert!(
            !budget.exhausted,
            "an already stopped clause executes no later request"
        );
        assert_eq!((budget.states, budget.bytes), spent);
        budget.end_clause();
        assert!(!budget.exhausted());
        budget.begin_clause(2);
        assert!(
            budget.work(1, 1),
            "the next clause can use real remaining global resources"
        );
        budget.end_clause();
        assert_eq!((budget.states, budget.bytes), (spent.0 + 1, spent.1 + 1));
    }
}
use crate::query::expression::Expr;
use crate::query::plan::budget::{MAX_DEPTH, MAX_PARTS};
use crate::query::plan::{MatchStep, PatternPart};
use crate::query::{ExecutionControl, QueryResources};
use crate::{cypher::Value, storage};
use std::collections::BTreeMap;

fn counters(budget: &PlanningBudget) -> (usize, usize, usize) {
    (budget.states, budget.bytes, budget.candidates)
}

#[test]
fn single_match_keeps_the_original_global_limits_and_stop() {
    for matches in [0, 1] {
        let mut budget = PlanningBudget::default();
        assert!(budget.work(7, 11));
        budget.begin_clause(matches);
        assert_eq!(budget.remaining_states(), MAX_STATES - 7);
        assert_eq!(budget.remaining_bytes(), MAX_BYTES - 11);
        assert!(budget.work(MAX_STATES - 7, MAX_BYTES - 11));
        assert!(!budget.exhausted());
        assert!(!budget.work(1, 0));
        budget.end_clause();
        assert!(budget.exhausted());
        assert_eq!(counters(&budget), (MAX_STATES, MAX_BYTES, 0));
    }
}

#[test]
fn soft_state_refusal_preserves_paid_work_and_the_next_clause() {
    let mut budget = PlanningBudget::default();
    assert!(budget.work(13, 17));
    budget.begin_clause(2);
    assert!(budget.work(MAX_STATES / 2, 19));
    let paid = counters(&budget);
    assert!(!budget.work(1, 0));
    assert!(budget.exhausted());
    assert_eq!(budget.remaining_states(), 0);
    assert_eq!(budget.remaining_bytes(), 0);
    assert_eq!(counters(&budget), paid);
    budget.end_clause();
    assert!(!budget.exhausted());
    assert_eq!(counters(&budget), paid);
    budget.begin_clause(2);
    assert_eq!(budget.remaining_states(), MAX_STATES / 2 - 13);
    assert!(budget.work(1, 1));
}

#[test]
fn unused_first_share_does_not_enlarge_the_last_clause_or_reset_bytes() {
    let mut budget = PlanningBudget::default();
    budget.begin_clause(2);
    assert!(budget.work(23, 29));
    budget.end_clause();
    budget.begin_clause(2);
    assert_eq!(budget.remaining_states(), MAX_STATES / 2);
    assert_eq!(budget.remaining_bytes(), MAX_BYTES / 2);
    assert!(budget.work(1, MAX_BYTES / 2));
    let paid = counters(&budget);
    assert!(!budget.work(0, 1));
    assert_eq!(counters(&budget), paid);
    budget.end_clause();
    assert!(!budget.exhausted());
    assert_eq!(budget.bytes, MAX_BYTES / 2 + 29);
    assert_eq!(budget.remaining_bytes(), MAX_BYTES / 2 - 29);
}

#[test]
fn retained_join_advice_and_its_bytes_survive_clause_exit() {
    let mut budget = PlanningBudget::default();
    let snapshot = storage::HashId::from_bytes([1; 32]);
    let definition = storage::HashId::from_bytes([2; 32]);
    budget.begin_clause(2);
    assert!(budget.remember_join_distribution(snapshot, definition, None));
    let paid = counters(&budget);
    assert!(paid.1 > 0);
    budget.end_clause();
    budget.begin_clause(2);
    assert_eq!(
        budget.cached_join_distribution(snapshot, definition),
        Some(None)
    );
    assert_eq!(counters(&budget), paid);
    assert_eq!(budget.remaining_bytes(), MAX_BYTES / 2);
}

#[test]
fn manual_and_global_hard_stops_survive_clause_exit() {
    let mut manual = PlanningBudget::default();
    manual.begin_clause(3);
    manual.stop();
    manual.end_clause();
    manual.begin_clause(3);
    assert!(manual.exhausted());
    assert!(!manual.work(0, 0));
    let mut full = PlanningBudget::default();
    assert!(full.work(MAX_STATES - 1, MAX_BYTES - 1));
    full.begin_clause(2);
    assert!(full.work(1, 1));
    assert!(!full.work(1, 0));
    full.end_clause();
    assert!(full.exhausted());
    assert_eq!(counters(&full), (MAX_STATES, MAX_BYTES, 0));
}

#[test]
fn coincident_global_and_clause_limits_remain_hard_after_exit() {
    for (states, bytes) in [(MAX_STATES / 2, 0), (0, MAX_BYTES / 2)] {
        let mut budget = PlanningBudget::default();
        budget.begin_clause(2);
        assert!(budget.work(states, bytes));
        budget.end_clause();
        budget.begin_clause(2);
        assert!(budget.work(states, bytes));
        let paid = counters(&budget);
        assert!(!budget.work(usize::from(states > 0), usize::from(bytes > 0)));
        budget.end_clause();
        assert!(budget.exhausted());
        assert_eq!(counters(&budget), paid);
    }
}

#[test]
fn oversized_optional_request_stops_only_its_smaller_local_share() {
    let mut budget = PlanningBudget::default();
    budget.begin_clause(2);
    assert!(!budget.work(MAX_STATES + 1, MAX_BYTES + 1));
    assert_eq!(counters(&budget), (0, 0, 0));
    budget.end_clause();
    assert!(!budget.exhausted());
    budget.begin_clause(2);
    assert!(budget.work(1, 1));
}

#[test]
fn candidate_count_is_global_and_soft_rejection_does_not_increment_it() {
    let mut budget = PlanningBudget::default();
    budget.begin_clause(MAX_STATES);
    assert!(budget.work(1, 0));
    assert!(!budget.candidate());
    assert_eq!(budget.candidates, 0);
    budget.end_clause();
    for _ in 0..MAX_CANDIDATES {
        assert!(budget.candidate());
    }
    assert_eq!(budget.remaining_candidates(), 0);
    assert!(!budget.candidate());
    budget.end_clause();
    assert!(budget.exhausted());
    assert_eq!(budget.candidates, MAX_CANDIDATES);
}

#[test]
fn soft_parameter_and_expression_rejections_do_not_become_hard() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut parameters = PlanningBudget::default();
    parameters.begin_clause(2);
    let params = BTreeMap::from([("large".into(), Value::List(vec![Value::Null; 2048]))]);
    assert!(
        parameters
            .parameter_bytes(&params, &control)
            .unwrap()
            .is_none()
    );
    parameters.end_clause();
    assert!(!parameters.exhausted());
    let mut expression = PlanningBudget::default();
    expression.begin_clause(2);
    assert!(expression.work(MAX_STATES / 2, 0));
    assert!(!expression.expression(&Expr::Literal(Value::Integer(1)), 1));
    expression.end_clause();
    assert!(!expression.exhausted());
    expression.begin_clause(2);
    assert!(!expression.expression(&Expr::Literal(Value::Integer(1)), MAX_DEPTH + 1));
    expression.end_clause();
    assert!(expression.exhausted());
}

#[test]
fn minimum_parameter_preflight_is_local_and_parameter_depth_is_hard() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut budget = PlanningBudget::default();
    let params = BTreeMap::from([("one".into(), Value::Null)]);
    budget.begin_clause(MAX_STATES);
    assert!(budget.parameter_bytes(&params, &control).unwrap().is_none());
    assert_eq!(counters(&budget), (0, 0, 0));
    budget.end_clause();
    assert!(!budget.exhausted());
    budget.begin_clause(2);
    assert!(
        !budget
            .parameter_value(&Value::Null, MAX_DEPTH + 1, &control)
            .unwrap()
    );
    budget.end_clause();
    assert!(budget.exhausted());
}

#[test]
fn soft_step_refusal_does_not_hide_real_part_limit_failures() {
    let empty = MatchStep {
        clause_start: 0,
        optional: false,
        parts: Vec::new(),
        predicate: None,
        early_predicates: Vec::new(),
        join: None,
    };
    let mut budget = PlanningBudget::default();
    budget.begin_clause(2);
    assert!(budget.work(MAX_STATES / 2, 0));
    let schema = storage::SchemaState::default();
    let mut step = empty.clone();
    step.predicate = Some(Expr::Literal(Value::Boolean(true)));
    assert!(!budget.step(&step, &schema));
    budget.end_clause();
    assert!(!budget.exhausted());
    let node = crate::query::plan::NodeSpec {
        variable: Some("n".into()),
        labels: Vec::new(),
        label_names: Vec::new(),
        scan_label: None,
        scan_label_name: None,
        index_seek: None,
        id_seek: None,
        impossible: false,
    };
    step.parts = vec![
        PatternPart {
            path_variable: None,
            start: node,
            relationship: None,
            end: None,
        };
        MAX_PARTS + 1
    ];
    budget.begin_clause(2);
    assert!(!budget.step(&step, &schema));
    budget.end_clause();
    assert!(budget.exhausted());
}

#[test]
fn single_match_advice_reservation_does_not_change_any_limit_or_counter() {
    for matches in [0, 1] {
        let mut budget = PlanningBudget::default();
        assert!(budget.work(5, 7));
        budget.begin_clause(matches);
        let paid = counters(&budget);
        assert!(!budget.clause_scoped());
        budget.reserve_advice_bytes(MAX_BYTES);
        assert_eq!(budget.remaining_advice_bytes(), MAX_BYTES - 7);
        assert_eq!(budget.remaining_bytes(), MAX_BYTES - 7);
        assert_eq!(counters(&budget), paid);
        assert!(!budget.exhausted());
    }
}

#[test]
fn advice_tail_is_reserved_without_rejecting_real_plan_construction() {
    let mut budget = PlanningBudget::default();
    budget.begin_clause(2);
    assert!(budget.clause_scoped());
    let total = budget.remaining_bytes();
    let tail = 4096;
    budget.reserve_advice_bytes(tail);
    assert_eq!(counters(&budget), (0, 0, 0));
    assert_eq!(budget.remaining_bytes(), total);
    assert_eq!(budget.remaining_advice_bytes(), total - tail);
    assert!(budget.work(1, total - tail));
    assert_eq!(budget.remaining_advice_bytes(), 0);
    assert_eq!(budget.remaining_bytes(), tail);
    assert!(!budget.exhausted());
    assert!(budget.work(1, tail));
    assert_eq!(budget.bytes, total);
    assert_eq!(budget.remaining_advice_bytes(), 0);
}

#[test]
fn advice_reservation_expires_without_resetting_retained_counters() {
    let mut budget = PlanningBudget::default();
    budget.begin_clause(2);
    assert!(budget.work(3, 17));
    budget.reserve_advice_bytes(4096);
    let paid = counters(&budget);
    budget.end_clause();
    assert!(!budget.clause_scoped());
    assert_eq!(counters(&budget), paid);
    assert_eq!(budget.remaining_advice_bytes(), MAX_BYTES - 17);
    budget.begin_clause(2);
    assert_eq!(counters(&budget), paid);
    assert_eq!(budget.remaining_advice_bytes(), MAX_BYTES / 2);
    assert_eq!(budget.remaining_bytes(), MAX_BYTES / 2);
    budget.reserve_advice_bytes(usize::MAX);
    assert_eq!(budget.remaining_advice_bytes(), 0);
    assert_eq!(counters(&budget), paid);
    assert_eq!(budget.remaining_bytes(), MAX_BYTES / 2);
}
