//! Ordered node domains intersect before any relationship product is expanded.

use super::adjacency::AdjacentList;
use super::*;

#[derive(Debug)]
pub(super) struct NodeIntersection {
    lists: Vec<AdjacentList>,
    minimum: Option<i64>,
    memory: MemoryReservation,
}

impl NodeIntersection {
    pub(super) fn new(
        plan: &MultiwayPlan,
        depth: usize,
        nodes: &[i64],
        stats: &mut MultiwayRuntimeMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let constraints = &plan.nodes[depth].constraints;
        let memory = control.reserve(allocation_bytes::<AdjacentList>(constraints.len())?)?;
        let mut lists = Vec::with_capacity(constraints.len());
        for index in constraints {
            control.work(1)?;
            let edge = plan.edges[*index];
            if edge.source == edge.target {
                // A self-loop is tested by its exact relationship domain, not
                // used as a domain before this node variable has been bound.
                continue;
            }
            let (other, outgoing) = if edge.target == depth {
                (edge.source, true)
            } else if edge.source == depth {
                (edge.target, false)
            } else {
                return Err(QueryError::internal(
                    "multiway constraint is not incident to its variable",
                ));
            };
            if other >= depth {
                return Err(QueryError::internal(
                    "multiway constraint uses an unbound variable",
                ));
            }
            lists.push(AdjacentList::new(
                nodes[other],
                edge.type_id,
                outgoing,
                stats,
                control,
            )?);
        }
        if lists.is_empty() {
            return Err(QueryError::internal(
                "multiway variable has no bound adjacency domain",
            ));
        }
        Ok(Self {
            lists,
            minimum: Some(1),
            memory,
        })
    }

    pub(super) fn next(
        &mut self,
        snapshot: &Snapshot<'_>,
        metrics: &mut QueryMetrics,
        stats: &mut MultiwayRuntimeMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<i64>> {
        let Some(mut target) = self.minimum else {
            return Ok(None);
        };
        'intersect: loop {
            for list in &mut self.lists {
                control.work(1)?;
                let Some(candidate) =
                    list.advance_at_least(target, snapshot, metrics, stats, control)?
                else {
                    self.minimum = None;
                    return Ok(None);
                };
                stats.intersection_steps = stats.intersection_steps.saturating_add(1);
                if candidate > target {
                    target = candidate;
                    continue 'intersect;
                }
            }
            self.minimum = target.checked_add(1);
            return Ok(Some(target));
        }
    }

    pub(super) fn retained_bytes(&self) -> u64 {
        self.memory.bytes().saturating_add(
            self.lists
                .iter()
                .map(AdjacentList::retained_bytes)
                .sum::<u64>(),
        )
    }
}
