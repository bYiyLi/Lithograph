//! Bounded arm descriptions and moved binding deltas for the ASP pipelines.

use crate::query::expression::{BindingRow, BindingValue};
use crate::query::plan::join::PartOrientation;
use crate::query::plan::{Direction, FactorArm, FactorHop, MatchStep, PatternPart};
use crate::query::resources::memory_error;
use crate::query::{ExecutionControl, QueryError, QueryErrorKind, QueryResult};

pub(super) fn arm_step(
    step: &MatchStep,
    arm: &FactorArm,
    predicate: bool,
) -> QueryResult<MatchStep> {
    if !(1..=2).contains(&arm.hops.len()) {
        return Err(QueryError::new(
            QueryErrorKind::Internal,
            "factorized arm must contain one or two hops",
        ));
    }
    let mut parts = Vec::new();
    parts
        .try_reserve_exact(arm.hops.len())
        .map_err(|_| memory_error("unable to allocate factorized arm metadata"))?;
    for hop in &arm.hops {
        parts.push(arm_part(step, hop)?);
    }
    Ok(MatchStep {
        clause_start: step.clause_start,
        optional: step.optional,
        parts,
        predicate: if predicate {
            step.predicate.clone()
        } else {
            None
        },
        early_predicates: Vec::new(),
        join: None,
    })
}

fn arm_part(step: &MatchStep, hop: &FactorHop) -> QueryResult<PatternPart> {
    let mut part = step.parts.get(hop.part).cloned().ok_or_else(|| {
        QueryError::new(QueryErrorKind::Internal, "factorized arm part is missing")
    })?;
    let end = part.end.as_mut().ok_or_else(|| {
        QueryError::new(
            QueryErrorKind::Internal,
            "factorized arm endpoint is missing",
        )
    })?;
    let relationship = part.relationship.as_mut().ok_or_else(|| {
        QueryError::new(
            QueryErrorKind::Internal,
            "factorized arm relationship is missing",
        )
    })?;
    if relationship.direction == Direction::Undirected {
        return Err(QueryError::new(
            QueryErrorKind::Internal,
            "factorized arm relationship must be directed",
        ));
    }
    if hop.orientation == PartOrientation::Reversed {
        std::mem::swap(&mut part.start, end);
        relationship.direction = match relationship.direction {
            Direction::Outgoing => Direction::Incoming,
            Direction::Incoming => Direction::Outgoing,
            Direction::Undirected => Direction::Undirected,
        };
    }
    Ok(part)
}

pub(super) fn shared_node(row: &BindingRow, name: &str) -> QueryResult<i64> {
    match row.values.get(name) {
        Some(BindingValue::Node(id)) => Ok(*id),
        _ => Err(QueryError::new(
            QueryErrorKind::Internal,
            "factorized shared binding is not a Node",
        )),
    }
}

pub(super) fn remove_prefix(
    delta: &mut BindingRow,
    prefix: &BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    for (name, value) in &prefix.values {
        control.work(1)?;
        if delta
            .values
            .get(name)
            .is_some_and(|existing| existing != value)
        {
            return Err(QueryError::new(
                QueryErrorKind::Internal,
                "factorized arm conflicts with its shared prefix",
            ));
        }
    }
    for name in prefix.values.keys() {
        control.work(1)?;
        delta.values.remove(name);
    }
    let mut retained = 0;
    for index in 0..delta.order.len() {
        control.work(1)?;
        if !prefix.values.contains_key(&delta.order[index]) {
            delta.order.swap(retained, index);
            retained += 1;
        }
    }
    delta.order.truncate(retained);
    Ok(())
}
