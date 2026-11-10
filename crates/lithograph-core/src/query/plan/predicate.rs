//! Conservative predicate routing and versioned Node element-ID seek.
//! The same MatchStep is consumed by read, write, and program cursors.

use std::collections::BTreeMap;

use crate::cypher::Value;
use crate::query::expression::{BinaryOp, Expr};

use super::MatchStep;

fn split_conjuncts<'a>(expression: &'a Expr, output: &mut Vec<&'a Expr>) {
    if let Expr::Binary(BinaryOp::And, lhs, rhs) = expression {
        split_conjuncts(lhs, output);
        split_conjuncts(rhs, output);
    } else {
        output.push(expression);
    }
}

fn unique_node_owner(step: &MatchStep, variable: &str) -> Option<usize> {
    let mut owner = None;
    for (index, part) in step.parts.iter().enumerate() {
        let start = part.start.variable.as_deref() == Some(variable);
        let end = part.end.as_ref().and_then(|end| end.variable.as_deref()) == Some(variable);
        let relationship = part
            .relationship
            .as_ref()
            .and_then(|relationship| relationship.variable.as_deref())
            == Some(variable);
        let path = part.path_variable.as_deref() == Some(variable);
        let uses =
            usize::from(start) + usize::from(end) + usize::from(relationship) + usize::from(path);
        if uses > 1 || (uses == 1 && (relationship || path || owner.is_some())) {
            return None;
        }
        if uses == 1 {
            owner = Some(index);
        }
    }
    owner
}

fn element_id_variable(expression: &Expr) -> Option<&str> {
    if let Expr::Function {
        name,
        args,
        distinct: false,
        star: false,
    } = expression
        && name.eq_ignore_ascii_case("elementId")
        && let [Expr::Variable(variable)] = args.as_slice()
    {
        return Some(variable);
    }
    None
}

fn safe_node_access_variable(expression: &Expr) -> Option<&str> {
    match expression {
        Expr::Property(base, _) => {
            if let Expr::Variable(variable) = base.as_ref() {
                Some(variable)
            } else {
                None
            }
        }
        _ => element_id_variable(expression),
    }
}

fn constant_expression(expression: &Expr) -> bool {
    matches!(expression, Expr::Literal(_) | Expr::Parameter(_))
}

/// An AND-conjunct may reject only after every earlier conjunct has been
/// evaluated in the same order. We therefore route a left-to-right prefix
/// whose binding Parts are monotonically available. Errors and NULL must
/// remain deferred to the full WHERE clause (see StepCursor).
/// Do not move expressions through OR, subqueries, volatile functions, or
/// unknown variable scopes.
pub(super) fn route_safe_predicates(step: &mut MatchStep) {
    step.early_predicates = vec![Vec::new(); step.parts.len()];
    if step.optional || step.parts.len() < 2 {
        return;
    }
    let Some(predicate) = step.predicate.as_ref() else {
        return;
    };
    let mut conjuncts = Vec::new();
    split_conjuncts(predicate, &mut conjuncts);
    let mut prefix: Vec<(usize, Expr)> = Vec::new();
    let mut last_owner = 0;
    for conjunct in conjuncts {
        let Expr::Binary(BinaryOp::Equal, lhs, rhs) = conjunct else {
            break;
        };
        let variable = if constant_expression(rhs) {
            safe_node_access_variable(lhs)
        } else if constant_expression(lhs) {
            safe_node_access_variable(rhs)
        } else {
            None
        };
        let Some(owner) = variable.and_then(|variable| unique_node_owner(step, variable)) else {
            break;
        };
        if owner < last_owner {
            break;
        }
        last_owner = owner;
        prefix.push((owner, conjunct.clone()));
    }
    // The final Part is already followed by the original WHERE; a redundant
    // evaluation there can only change work/error timing, not avoid expansion.
    for depth in 0..step.parts.len() - 1 {
        for (owner, conjunct) in &prefix {
            if *owner > depth {
                break;
            }
            step.early_predicates[depth].push(conjunct.clone());
        }
    }
}

fn constant_node_id(expression: &Expr, params: &BTreeMap<String, Value>) -> Option<i64> {
    let value = match expression {
        Expr::Literal(value) => value,
        Expr::Parameter(name) => params.get(name)?,
        _ => return None,
    };
    let Value::String(text) = value else {
        return None;
    };
    let id = text.strip_prefix("n:")?.parse::<i64>().ok()?;
    (id > 0 && text == &format!("n:{id}")).then_some(id)
}

fn node_id_equality<'a>(
    expression: &'a Expr,
    params: &BTreeMap<String, Value>,
) -> Option<(&'a str, i64)> {
    let Expr::Binary(BinaryOp::Equal, lhs, rhs) = expression else {
        return None;
    };
    element_id_variable(lhs)
        .and_then(|variable| constant_node_id(rhs, params).map(|id| (variable, id)))
        .or_else(|| {
            element_id_variable(rhs)
                .and_then(|variable| constant_node_id(lhs, params).map(|id| (variable, id)))
        })
}

/// Select one direct Snapshot lookup for an unambiguously owned start Node.
/// Malformed, null and non-String values stay on the original scan path.
/// A residual WHERE still validates the real Cypher comparison.
pub(crate) fn select_node_id_seeks(step: &mut MatchStep, params: &BTreeMap<String, Value>) {
    let Some(predicate) = step.predicate.as_ref() else {
        return;
    };
    let mut conjuncts = Vec::new();
    split_conjuncts(predicate, &mut conjuncts);
    // Restricting access can also suppress errors in predicates that would
    // precede the identity check in the original left-to-right AND. Only a
    // leading run of provably total Node elementId/STRING comparisons may
    // select seeks. A preceding property comparison is not guaranteed total.
    for conjunct in conjuncts {
        let Some((variable, id)) = node_id_equality(conjunct, params) else {
            break;
        };
        if unique_node_owner(step, variable).is_none() {
            break;
        }
        if let Some(part) = step
            .parts
            .iter_mut()
            .find(|part| part.start.variable.as_deref() == Some(variable))
        {
            part.start.id_seek.get_or_insert(id);
        }
    }
}
