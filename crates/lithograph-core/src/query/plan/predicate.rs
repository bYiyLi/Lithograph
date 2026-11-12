//! Conservative predicate routing and versioned element-ID seeks.
//! The same MatchStep is consumed by read, write, and program cursors.

use std::collections::BTreeMap;

use crate::cypher::Value;
use crate::query::expression::{BinaryOp, Expr, UnaryOp};

use super::MatchStep;

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

fn entity_access_variable(expression: &Expr) -> Option<&str> {
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
    fn visit(expression: &Expr, depth: usize, remaining: &mut usize) -> bool {
        if depth > super::budget::MAX_DEPTH || *remaining == 0 {
            return false;
        }
        *remaining -= 1;
        match expression {
            Expr::Literal(_) | Expr::Parameter(_) => true,
            Expr::Unary(UnaryOp::Positive | UnaryOp::Negative, value) => {
                visit(value, depth + 1, remaining)
            }
            Expr::List(values) => values
                .iter()
                .all(|value| visit(value, depth + 1, remaining)),
            _ => false,
        }
    }
    let mut remaining = super::budget::MAX_STATES;
    visit(expression, 1, &mut remaining)
}

fn routed_variable(expression: &Expr) -> Option<&str> {
    let Expr::Binary(operator, lhs, rhs) = expression else {
        return None;
    };
    match operator {
        BinaryOp::Equal
        | BinaryOp::Less
        | BinaryOp::LessEqual
        | BinaryOp::Greater
        | BinaryOp::GreaterEqual => {
            if constant_expression(rhs) {
                entity_access_variable(lhs)
            } else if constant_expression(lhs) {
                entity_access_variable(rhs)
            } else {
                None
            }
        }
        BinaryOp::In if constant_expression(rhs) => entity_access_variable(lhs),
        _ => None,
    }
}

/// An AND-conjunct may reject only after every earlier conjunct has been
/// evaluated in the same order. We therefore route a left-to-right prefix
/// whose binding Parts are monotonically available. Errors and NULL must
/// remain deferred to the full WHERE clause (see StepCursor).
/// Do not move expressions through OR, subqueries, volatile functions, or
/// unknown variable scopes.
pub(super) fn route_safe_predicates(step: &mut MatchStep) {
    step.early_predicates.clear();
    if step.optional || step.parts.len() < 2 || step.parts.len() > super::budget::MAX_PARTS {
        return;
    }
    step.early_predicates = vec![Vec::new(); step.parts.len()];
    let Some(predicate) = step.predicate.as_ref() else {
        return;
    };
    let Some(conjuncts) = super::budget::conjuncts(predicate) else {
        return;
    };
    if step.parts.len().saturating_mul(conjuncts.len()) > super::budget::MAX_STATES {
        return;
    }
    let mut prefix: Vec<(usize, Expr)> = Vec::new();
    let mut clone_bytes = 0_u64;
    let mut last_owner = 0;
    for conjunct in conjuncts {
        let Some(owner) = routed_variable(conjunct).and_then(|variable| {
            unique_node_owner(step, variable).or_else(|| unique_relationship_owner(step, variable))
        }) else {
            break;
        };
        if owner < last_owner {
            break;
        }
        let Some(bytes) = routing_clone_bytes(conjunct) else {
            break;
        };
        clone_bytes = clone_bytes.saturating_add(bytes.saturating_mul(step.parts.len() as u64));
        if clone_bytes > super::budget::MAX_BYTES as u64 {
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

fn routing_clone_bytes(expression: &Expr) -> Option<u64> {
    let mut remaining = super::budget::MAX_STATES;
    routing_clone_bytes_inner(expression, 1, &mut remaining)
}

fn routing_clone_bytes_inner(
    expression: &Expr,
    depth: usize,
    remaining: &mut usize,
) -> Option<u64> {
    if depth > super::budget::MAX_DEPTH || *remaining == 0 {
        return None;
    }
    *remaining -= 1;
    let overhead = std::mem::size_of::<Expr>() as u64;
    let mut child = |expression: &Expr| routing_clone_bytes_inner(expression, depth + 1, remaining);
    match expression {
        Expr::Binary(_, left, right) => Some(
            overhead
                .saturating_add(child(left)?)
                .saturating_add(child(right)?),
        ),
        Expr::Property(base, name) => Some(
            overhead
                .saturating_add(name.len() as u64)
                .saturating_add(child(base)?),
        ),
        Expr::Variable(name) | Expr::Parameter(name) => {
            Some(overhead.saturating_add(name.len() as u64))
        }
        Expr::Function { args, .. } if args.len() == 1 => {
            Some(overhead.saturating_add(child(&args[0])?))
        }
        Expr::Unary(UnaryOp::Positive | UnaryOp::Negative, value) => {
            Some(overhead.saturating_add(child(value)?))
        }
        Expr::List(values) => values.iter().try_fold(overhead, |bytes, value| {
            Some(bytes.saturating_add(child(value)?))
        }),
        Expr::Literal(value) => {
            Some(overhead.saturating_add(crate::query::resources::value_bytes(value).ok()?))
        }
        _ => None,
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
    if text.len() > 21 {
        return None;
    }
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
    if step.parts.len() > super::budget::MAX_PARTS {
        return;
    }
    let Some(predicate) = step.predicate.as_ref() else {
        return;
    };
    let Some(conjuncts) = super::budget::conjuncts(predicate) else {
        return;
    };
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

fn unique_relationship_owner(step: &MatchStep, variable: &str) -> Option<usize> {
    let mut owner = None;
    for (index, part) in step.parts.iter().enumerate() {
        if part.path_variable.as_deref() == Some(variable)
            || part.start.variable.as_deref() == Some(variable)
            || part.end.as_ref().and_then(|end| end.variable.as_deref()) == Some(variable)
        {
            return None;
        }
        if part
            .relationship
            .as_ref()
            .and_then(|rel| rel.variable.as_deref())
            == Some(variable)
            && owner.replace(index).is_some()
        {
            return None;
        }
    }
    owner
}

fn relationship_id_equality<'a>(
    expression: &'a Expr,
    params: &BTreeMap<String, Value>,
) -> Option<(&'a str, i64)> {
    let Expr::Binary(BinaryOp::Equal, left, right) = expression else {
        return None;
    };
    let id = |candidate: &Expr| -> Option<i64> {
        let value = match candidate {
            Expr::Literal(value) => value,
            Expr::Parameter(name) => params.get(name)?,
            _ => return None,
        };
        let Value::String(text) = value else {
            return None;
        };
        if text.len() > 21 {
            return None;
        }
        let id = text.strip_prefix("r:")?.parse::<i64>().ok()?;
        (id > 0 && text == &format!("r:{id}")).then_some(id)
    };
    element_id_variable(left)
        .and_then(|variable| id(right).map(|id| (variable, id)))
        .or_else(|| {
            element_id_variable(right).and_then(|variable| id(left).map(|id| (variable, id)))
        })
}

/// Use only a leading, total equality prefix; the original WHERE still
/// validates Cypher NULL, TYPE_ERROR, and any remaining conditions.
pub(crate) fn select_relationship_id_seeks(step: &mut MatchStep, params: &BTreeMap<String, Value>) {
    if step.parts.len() > super::budget::MAX_PARTS {
        return;
    }
    let Some(predicate) = step.predicate.as_ref() else {
        return;
    };
    let Some(conjuncts) = super::budget::conjuncts(predicate) else {
        return;
    };
    for conjunct in conjuncts {
        let Some((variable, id)) = relationship_id_equality(conjunct, params) else {
            break;
        };
        let Some(index) = unique_relationship_owner(step, variable) else {
            break;
        };
        if let Some(spec) = step.parts[index].relationship.as_mut() {
            spec.id_seek.get_or_insert(id);
            spec.index_seek = None;
        }
    }
}
