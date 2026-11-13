use super::*;
use crate::query::plan::{Direction, MatchStep, PatternPart};
use crate::query::schema::StandardIndexSeek;

/// Borrow only the expansion metadata; EXPLAIN fallback must not clone an
/// entire PatternPart or retain a second query-sized description vector.
pub(crate) fn expansion_metadata(
    step: &MatchStep,
    depth: usize,
) -> Option<(Option<&str>, Direction)> {
    if step
        .join
        .as_ref()
        .and_then(|plan| plan.multiway.as_ref())
        .is_some_and(|plan| depth < plan.edges.len())
    {
        return None;
    }
    if let Some(factor) = step.join.as_ref().and_then(|plan| plan.factorized.as_ref()) {
        return factor_expansion(step, factor, depth);
    }
    let stage = step.join.as_ref().and_then(|plan| plan.stages.get(depth));
    let ordinal = stage.map_or(depth, |stage| stage.part);
    let rel = step.parts.get(ordinal)?.relationship.as_ref()?;
    let indexed = match stage.map(|stage| stage.access) {
        Some(StageAccess::Probe(id)) => {
            let template = &step.join.as_ref()?.templates[id];
            template.target == ProbeTarget::Relationship
                && matches!(
                    template.access,
                    ProbeAccess::Identity { .. } | ProbeAccess::IndexEquality { .. }
                )
        }
        _ => rel.id_seek.is_some() || rel.index_seek.is_some(),
    };
    if indexed {
        return None;
    }
    let direction = if stage.is_some_and(|stage| stage.orientation == PartOrientation::Reversed) {
        reverse_direction(rel.direction)
    } else {
        rel.direction
    };
    Some((rel.type_name.as_deref(), direction))
}

fn factor_expansion<'a>(
    step: &'a MatchStep,
    factor: &super::super::FactorizedPlan,
    depth: usize,
) -> Option<(Option<&'a str>, Direction)> {
    let hop = factor
        .accumulate
        .hops
        .iter()
        .chain(&factor.build.hops)
        .nth(depth)?;
    let relationship = step.parts.get(hop.part)?.relationship.as_ref()?;
    let direction = if hop.orientation == PartOrientation::Reversed {
        reverse_direction(relationship.direction)
    } else {
        relationship.direction
    };
    Some((relationship.type_name.as_deref(), direction))
}

fn reverse_direction(direction: Direction) -> Direction {
    match direction {
        Direction::Outgoing => Direction::Incoming,
        Direction::Incoming => Direction::Outgoing,
        Direction::Undirected => Direction::Undirected,
    }
}

/// Build the same oriented producer description for plan display and cursor
/// initialization. Placeholder identities never reach storage: a probe frame
/// replaces them with its real outer key before its first pull.
pub(crate) fn execution_part(step: &MatchStep, depth: usize) -> (usize, PatternPart) {
    let stage = step.join.as_ref().and_then(|plan| plan.stages.get(depth));
    let ordinal = stage.map_or(depth, |stage| stage.part);
    let mut part = step.parts[ordinal].clone();
    if stage.is_some_and(|stage| stage.orientation == PartOrientation::Reversed) {
        if let Some(end) = part.end.as_mut() {
            std::mem::swap(&mut part.start, end);
        }
        if let Some(rel) = part.relationship.as_mut() {
            rel.direction = reverse_direction(rel.direction);
        }
    }
    if let (Some(plan), Some(StageAccess::Probe(template))) =
        (&step.join, stage.map(|stage| stage.access))
    {
        install_probe(&mut part, &plan.templates[template]);
    }
    (ordinal, part)
}

fn install_probe(part: &mut PatternPart, template: &ParameterTemplate) {
    part.start.id_seek = None;
    part.start.index_seek = None;
    if let Some(rel) = part.relationship.as_mut() {
        rel.id_seek = None;
        rel.index_seek = None;
    }
    match &template.access {
        ProbeAccess::Identity { .. } => match template.target {
            ProbeTarget::Relationship => {
                if let Some(rel) = part.relationship.as_mut() {
                    rel.id_seek = Some(1);
                }
            }
            _ => part.start.id_seek = Some(1),
        },
        ProbeAccess::IndexEquality {
            index_name, kind, ..
        } => {
            let seek = Some(StandardIndexSeek {
                index_name: index_name.clone(),
                kind: *kind,
                predicates: Vec::new(),
            });
            match template.target {
                ProbeTarget::Relationship => {
                    if let Some(rel) = part.relationship.as_mut() {
                        rel.index_seek = seek;
                    }
                }
                _ => part.start.index_seek = seek,
            }
        }
        ProbeAccess::Adjacency { .. } => {}
    }
}
