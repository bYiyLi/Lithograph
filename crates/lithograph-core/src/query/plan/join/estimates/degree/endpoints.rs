//! Degree advice for a small typed producer's actual endpoint population.
//! An unlabeled endpoint is not uniformly distributed over unrelated Nodes.
//! Samples retain parallel-edge weights, never prune rows, and stay local to
//! one pinned prepare. Labels/predicates can select a subset of this population;
//! the expected mean is advice, while the upper bound covers every sampled key.

use super::*;
use crate::storage::NodeId;

const MAX_ENDPOINTS: usize = 32;
const MAX_WORK: usize = 512;
const DEGREE_CAP: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct EndpointKey {
    provider: usize,
    source_target: bool,
    target_type: Option<RelationshipTypeId>,
    incoming: Option<bool>,
}

pub(super) fn key(step: &MatchStep, template: &ParameterTemplate) -> Option<EndpointKey> {
    let ProbeAccess::Adjacency { source_variable } = &template.access else {
        return None;
    };
    if template.source_guard.0.count_ones() != 1 || template.target == ProbeTarget::Relationship {
        return None;
    }
    let provider = template.source_guard.0.trailing_zeros() as usize;
    let source = step.parts.get(provider)?;
    let source_rel = source.relationship.as_ref()?;
    let end = source.end.as_ref()?;
    let target = step.parts.get(template.part)?.relationship.as_ref()?;
    producer_type(source_rel)?;
    if target.impossible {
        return None;
    }
    let start_matches = source.start.variable.as_ref() == Some(source_variable);
    let end_matches = end.variable.as_ref() == Some(source_variable);
    if start_matches == end_matches {
        return None;
    }
    let reversed = template.target == ProbeTarget::EndNode;
    let incoming = match (target.direction, reversed) {
        (Direction::Outgoing, false) | (Direction::Incoming, true) => Some(false),
        (Direction::Incoming, false) | (Direction::Outgoing, true) => Some(true),
        (Direction::Undirected, _) => None,
    };
    Some(EndpointKey {
        provider,
        source_target: end_matches ^ (source_rel.direction == Direction::Incoming),
        target_type: target.type_id,
        incoming,
    })
}

fn producer_type(source: &crate::query::plan::RelationshipSpec) -> Option<RelationshipTypeId> {
    if source.impossible
        || source.direction == Direction::Undirected
        || source.id_seek.is_some()
        || source.index_seek.is_some()
    {
        return None;
    }
    source.type_id
}

pub(super) fn collect(
    step: &MatchStep,
    template: &ParameterTemplate,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    headroom: usize,
    cache: &mut BTreeMap<EndpointKey, Option<CardinalityEstimate>>,
) -> QueryResult<()> {
    let Some(key) = key(step, template) else {
        return Ok(());
    };
    let Some(source_type) = step.parts[key.provider]
        .relationship
        .as_ref()
        .and_then(producer_type)
    else {
        return Ok(());
    };
    let Some(&count) = statistics.type_counts.get(&source_type) else {
        return Ok(());
    };
    if !(1..=MAX_ENDPOINTS as u64).contains(&count) || cache.contains_key(&key) {
        return Ok(());
    }
    // Type-filtered overlay iteration can inspect unrelated overlay entries.
    // Charge its constant-time size bound before every physical page, too.
    let overlay = context.snapshot.bounded_statistics_work(0, 0, true);
    if overlay > MAX_WORK || !admit(budget, headroom, 1, 1024) {
        return Ok(());
    }
    let sample = match super::super::collect::claim(MAX_WORK, budget, headroom, context)? {
        Some(_memory) => {
            let mut work = Work {
                left: MAX_WORK,
                overlay,
            };
            let result = sample(key, source_type, count, context, &mut work);
            // Prepay the full cap before I/O, then retain the work actually
            // charged by this bounded sampler. Unused units remain available
            // to the normal join search; the global cap is never increased.
            budget.states -= work.left;
            result?
        }
        None => None,
    };
    cache.insert(key, sample);
    Ok(())
}

struct Work {
    left: usize,
    overlay: usize,
}

impl Work {
    fn spend(&mut self, units: usize, control: &ExecutionControl<'_>) -> QueryResult<bool> {
        if units > self.left {
            return Ok(false);
        }
        self.left -= units;
        control.work(units as u64)?;
        control.check()?;
        Ok(true)
    }
}

#[derive(Default)]
struct Endpoints {
    weights: [(NodeId, u64); MAX_ENDPOINTS],
    len: usize,
    total: u64,
}

impl Endpoints {
    fn insert(&mut self, node: NodeId) -> bool {
        if let Some((_, weight)) = self.weights[..self.len]
            .iter_mut()
            .find(|(id, _)| *id == node)
        {
            *weight += 1;
        } else {
            if self.len == MAX_ENDPOINTS {
                return false;
            }
            self.weights[self.len] = (node, 1);
            self.len += 1;
        }
        self.total += 1;
        true
    }
}

fn population(
    key: EndpointKey,
    source_type: RelationshipTypeId,
    count: u64,
    context: &JoinPlanningContext<'_, '_, '_>,
    work: &mut Work,
) -> QueryResult<Option<Endpoints>> {
    let mut result = Endpoints::default();
    let mut after = 0;
    loop {
        if !work.spend(20 + work.overlay, context.control)? {
            return Ok(None);
        }
        let page = context
            .snapshot
            .scan_relationship_type_after(source_type, after, 8)?;
        for relationship in page.items {
            if result.total == count || !work.spend(result.len + 1, context.control)? {
                return Ok(None);
            }
            let node = if key.source_target {
                relationship.target
            } else {
                relationship.source
            };
            if !result.insert(node) {
                return Ok(None);
            }
        }
        let Some(next) = page.next_after else {
            return Ok((result.total == count).then_some(result));
        };
        after = next;
    }
}

fn sample(
    key: EndpointKey,
    source_type: RelationshipTypeId,
    count: u64,
    context: &JoinPlanningContext<'_, '_, '_>,
    work: &mut Work,
) -> QueryResult<Option<CardinalityEstimate>> {
    let Some(endpoints) = population(key, source_type, count, context, work)? else {
        return Ok(None);
    };
    let mut sum = 0;
    let mut upper = 0;
    for &(node, weight) in &endpoints.weights[..endpoints.len] {
        let Some((degree, truncated)) = degree(node, key, context, work)? else {
            return Ok(None);
        };
        sum += degree * weight;
        upper = if truncated {
            u64::MAX
        } else {
            upper.max(degree)
        };
    }
    Ok((sum > 0).then(|| expectation(sum.div_ceil(endpoints.total), upper)))
}

fn degree(
    node: NodeId,
    key: EndpointKey,
    context: &JoinPlanningContext<'_, '_, '_>,
    work: &mut Work,
) -> QueryResult<Option<(u64, bool)>> {
    let mut count = 0;
    let mut cursor = None;
    loop {
        let limit = 8.min(DEGREE_CAP + 1 - count);
        let sources = if key.incoming.is_none() { 4 } else { 2 };
        if !work.spend(sources * limit + 4 + work.overlay, context.control)? {
            return Ok((count > 0).then_some((count as u64, true)));
        }
        let page = match key.incoming {
            Some(false) => {
                context
                    .snapshot
                    .scan_outgoing_page(node, key.target_type, cursor, limit)?
            }
            Some(true) => {
                context
                    .snapshot
                    .scan_incoming_page(node, key.target_type, cursor, limit)?
            }
            None => context
                .snapshot
                .scan_incident_page(node, key.target_type, cursor, limit)?,
        };
        count += page.items.len();
        if count > DEGREE_CAP {
            return Ok(Some((count as u64, true)));
        }
        let Some(next) = page.next_cursor else {
            return Ok(Some((count as u64, false)));
        };
        cursor = Some(next);
    }
}

#[cfg(test)]
mod tests;
