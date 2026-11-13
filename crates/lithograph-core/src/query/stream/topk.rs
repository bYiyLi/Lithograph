//! A narrow ordered Range consumer, with a storage binding before Sort removal.
//! Every rejected shape retains the original projection / Sort error boundary.

use std::collections::BTreeMap;

use crate::cypher::{AstKind, AstNode, Value};
use crate::query::expression::{BinaryOp, Expr};
use crate::query::plan::{NodeSpec, PhysicalOperator, PlanningBudget, PreparedQuery};
use crate::query::schema::{OrderedNumericIndexRead, StandardIndexPredicate, StandardIndexSeek};
use crate::query::{ExecutionControl, QueryResult};
use crate::storage::{
    ConstraintDefinitionKind, IndexTarget, PropertyType, SchemaState, SchemaTarget, Snapshot,
    StandardIndexKind,
};

#[cfg(test)]
mod tests;

struct Source<'a> {
    node: &'a NodeSpec,
    label: &'a str,
    property: &'a str,
    variable: &'a str,
}

/// Replace Sort only after obtaining a query/connection/state-bound token.
/// Execution uses its forced numeric entry, never an owner-order fallback.
pub(crate) fn install(
    prepared: &mut PreparedQuery,
    snapshot: &Snapshot<'_>,
    single: &AstNode,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    if let Some(proof) = try_admit(prepared, snapshot, single, budget, control)? {
        for operator in &mut prepared.physical.operators {
            if matches!(operator, PhysicalOperator::ExternalSort) {
                *operator = PhysicalOperator::OrderedTopK;
            }
        }
        prepared.ordered_topk = Some(proof);
    }
    Ok(())
}

fn try_admit(
    prepared: &PreparedQuery,
    snapshot: &Snapshot<'_>,
    single: &AstNode,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<OrderedNumericIndexRead>> {
    control.check()?;
    if budget.exhausted() {
        return Ok(None);
    }
    let Some(source) = source(prepared) else {
        return Ok(None);
    };
    let Some(limit) = prepared.limit.filter(|limit| *limit > 0) else {
        return Ok(None);
    };
    if prepared.skip.checked_add(limit).is_none()
        || !inspect_shape(single, 0, budget, control)?
        || prepared.order[0].expression != prepared.projections[0].expression
        || source.node.variable.as_deref() != Some(source.variable)
    {
        return Ok(None);
    }
    let Some(seek) = source.node.index_seek.as_ref() else {
        return Ok(None);
    };
    if !index_proves_source(prepared, &source, seek, snapshot, budget, control)? {
        return Ok(None);
    }
    crate::query::schema::try_bind_topk_numeric_index(snapshot, seek, control, budget)
}

fn index_proves_source(
    prepared: &PreparedQuery,
    source: &Source<'_>,
    seek: &StandardIndexSeek,
    snapshot: &Snapshot<'_>,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let Some(schema) = snapshot.query_schema_state() else {
        return Ok(false);
    };
    if !inspect(budget, control, seek.index_name.len())? {
        return Ok(false);
    }
    let Some(index) = schema.indexes.get(&seek.index_name) else {
        return Ok(false);
    };
    let IndexTarget::NodeProperties { label, properties } = &index.target else {
        return Ok(false);
    };
    let [property] = properties.as_slice() else {
        return Ok(false);
    };
    if !inspect(budget, control, label.len().saturating_add(property.len()))?
        || seek.kind != StandardIndexKind::Range
        || index.kind != StandardIndexKind::Range
        || label != source.label
        || property != source.property
        || !bounds_match(prepared, seek.predicates.as_slice())
        || !schema_proves_key(schema, source, budget, control)?
    {
        return Ok(false);
    }
    Ok(true)
}

fn source(prepared: &PreparedQuery) -> Option<Source<'_>> {
    if prepared.distinct
        || prepared.candidate.is_some()
        || prepared.write.is_some()
        || prepared.schema.is_some()
        || prepared.program.is_some()
        || !prepared.graph_view.is_full_graph()
        || !prepared.graph_view.query_access.is_empty()
    {
        return None;
    }
    let [projection] = prepared.projections.as_slice() else {
        return None;
    };
    let Expr::Property(base, property) = &projection.expression else {
        return None;
    };
    let Expr::Variable(variable) = base.as_ref() else {
        return None;
    };
    let [order] = prepared.order.as_slice() else {
        return None;
    };
    if order.descending {
        return None;
    }
    source_pattern(prepared, property, variable)
}

fn source_pattern<'a>(
    prepared: &'a PreparedQuery,
    property: &'a str,
    variable: &'a str,
) -> Option<Source<'a>> {
    let [step] = prepared.matches.as_slice() else {
        return None;
    };
    let [part] = step.parts.as_slice() else {
        return None;
    };
    let [label] = part.start.label_names.as_slice() else {
        return None;
    };
    if step.optional
        || step.join.is_some()
        || part.path_variable.is_some()
        || part.relationship.is_some()
        || part.end.is_some()
        || part.start.impossible
        || part.start.id_seek.is_some()
        || !prepared
            .physical
            .operators
            .iter()
            .any(|operator| matches!(operator, PhysicalOperator::ExternalSort))
    {
        return None;
    }
    Some(Source {
        node: &part.start,
        label,
        property,
        variable,
    })
}

fn inspect_shape(
    node: &AstNode,
    depth: usize,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    // AstNode::descendants owns a growing stack. This optional inspection uses
    // bounded borrowed recursion, and pays text before any compiled-name Eq.
    if depth > crate::query::plan::MAX_DEPTH
        || !inspect(budget, control, node.text.as_ref().map_or(0, String::len))?
        || node.kind == AstKind::ProjectionAlias
    {
        return Ok(false);
    }
    for child in &node.children {
        if !inspect_shape(child, depth + 1, budget, control)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Bound {
    lower: bool,
    value: i64,
    inclusive: bool,
}

fn bound(expression: &Expr, property: &Expr, params: &BTreeMap<String, Value>) -> Option<Bound> {
    let Expr::Binary(operator, left, right) = expression else {
        return None;
    };
    let (value, reversed) = if left.as_ref() == property {
        (integer(right, params)?, false)
    } else if right.as_ref() == property {
        (integer(left, params)?, true)
    } else {
        return None;
    };
    let (lower, inclusive) = match operator {
        BinaryOp::Greater => (!reversed, false),
        BinaryOp::GreaterEqual => (!reversed, true),
        BinaryOp::Less => (reversed, false),
        BinaryOp::LessEqual => (reversed, true),
        _ => return None,
    };
    Some(Bound {
        lower,
        value,
        inclusive,
    })
}

fn integer(expression: &Expr, params: &BTreeMap<String, Value>) -> Option<i64> {
    match expression {
        Expr::Literal(Value::Integer(value)) => Some(*value),
        Expr::Parameter(name) => match params.get(name) {
            Some(Value::Integer(value)) => Some(*value),
            _ => None,
        },
        _ => None,
    }
}

fn bounds_match(prepared: &PreparedQuery, predicates: &[(usize, StandardIndexPredicate)]) -> bool {
    let Some(Expr::Binary(BinaryOp::And, left, right)) = &prepared.matches[0].predicate else {
        return false;
    };
    let property = &prepared.projections[0].expression;
    let (Some(left), Some(right)) = (
        bound(left, property, &prepared.params),
        bound(right, property, &prepared.params),
    ) else {
        return false;
    };
    let (lower, upper) = if left.lower && !right.lower {
        (left, right)
    } else if right.lower && !left.lower {
        (right, left)
    } else {
        return false;
    };
    if lower.value < 0 || upper.value > 1_000_000 || lower.value > upper.value {
        return false;
    }
    matches!(predicates, [(0, StandardIndexPredicate::Bounds {
        lower: Some((Value::Integer(low), low_inclusive)),
        upper: Some((Value::Integer(high), high_inclusive)),
    })] if (*low, *low_inclusive) == (lower.value, lower.inclusive)
        && (*high, *high_inclusive) == (upper.value, upper.inclusive))
}

fn schema_proves_key(
    schema: &SchemaState,
    source: &Source<'_>,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let (mut integer, mut unique, mut present) = (false, false, false);
    for constraint in schema.constraints.values() {
        if !inspect(budget, control, 0)? {
            return Ok(false);
        }
        let SchemaTarget::Node { label } = &constraint.target else {
            continue;
        };
        let [property] = constraint.properties.as_slice() else {
            continue;
        };
        let bytes = label
            .len()
            .saturating_add(property.len())
            .saturating_add(source.label.len())
            .saturating_add(source.property.len());
        if !inspect(budget, control, bytes)? {
            return Ok(false);
        }
        if property != source.property || label != source.label {
            continue;
        }
        let (typed, key, required) = constraint_proofs(&constraint.kind);
        integer |= typed;
        unique |= key;
        present |= required;
        if integer && unique && present {
            return Ok(true);
        }
    }
    Ok(false)
}

fn constraint_proofs(kind: &ConstraintDefinitionKind) -> (bool, bool, bool) {
    match kind {
        ConstraintDefinitionKind::Key => (false, true, true),
        ConstraintDefinitionKind::Unique => (false, true, false),
        ConstraintDefinitionKind::NotNull => (false, false, true),
        ConstraintDefinitionKind::Type { rule } => (
            rule.property_type == PropertyType::Integer,
            false,
            rule.required,
        ),
    }
}

fn inspect(
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
    bytes: usize,
) -> QueryResult<bool> {
    control.work(1)?;
    Ok(budget.work(1, bytes))
}
