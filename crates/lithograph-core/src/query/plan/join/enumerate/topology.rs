//! Connected hypergraph pairs, independently of the physical left-deep memo.
//! The traversal follows neighborhood/exclusion/canonical-seed rules; it never
//! tests every subset of the query or enumerates permutations.

use std::collections::BTreeSet;

use super::super::Hyperedge;
use crate::query::plan::budget::PlanningBudget;
use crate::query::{ExecutionControl, QueryResult};

pub(super) const EXACT_PARTS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ConnectedPair {
    pub(super) left: u128,
    pub(super) right: u128,
}

pub(super) struct Graph {
    pub(super) parts: usize,
    simple: Vec<u128>,
    complex: Vec<Hyperedge>,
}

pub(super) fn charge(
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
    states: usize,
    bytes: usize,
) -> QueryResult<bool> {
    control.work(states as u64 + 1)?;
    Ok(budget.work(states, bytes))
}

impl Graph {
    pub(super) fn new(
        parts: usize,
        edges: &[Hyperedge],
        budget: &mut PlanningBudget,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<Self>> {
        if parts == 0 || parts > 128 {
            return Ok(None);
        }
        let bytes = parts
            .saturating_mul(16)
            .saturating_add(edges.len().saturating_mul(std::mem::size_of::<Hyperedge>()));
        if !charge(budget, control, parts, bytes)? {
            return Ok(None);
        }
        let mut graph = Self {
            parts,
            simple: vec![0; parts],
            complex: Vec::with_capacity(edges.len()),
        };
        let all = if parts == 128 {
            u128::MAX
        } else {
            (1_u128 << parts) - 1
        };
        for edge in edges {
            if !charge(budget, control, 1, 0)? {
                return Ok(None);
            }
            let (left, right) = (edge.left.0, edge.right.0);
            if left == 0 || right == 0 || left & right != 0 || (left | right) & !all != 0 {
                return Ok(None);
            }
            if left.is_power_of_two() && right.is_power_of_two() {
                graph.simple[left.trailing_zeros() as usize] |= right;
                graph.simple[right.trailing_zeros() as usize] |= left;
            } else {
                graph.complex.push(*edge);
            }
        }
        Ok(Some(graph))
    }

    pub(super) fn singleton_work(&self) -> usize {
        self.complex.len().saturating_add(1)
    }

    pub(super) fn connects_singleton(&self, outer: u128, part: usize) -> bool {
        let inner = 1_u128 << part;
        self.simple[part] & outer != 0
            || self.complex.iter().any(|edge| {
                (edge.right.0 == inner && edge.left.0 & outer == edge.left.0)
                    || (edge.left.0 == inner && edge.right.0 & outer == edge.right.0)
            })
    }

    fn connects(
        &self,
        left: u128,
        right: u128,
        budget: &mut PlanningBudget,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<bool>> {
        let (mut small, other) = if left.count_ones() <= right.count_ones() {
            (left, right)
        } else {
            (right, left)
        };
        while small != 0 {
            if !charge(budget, control, 1, 0)? {
                return Ok(None);
            }
            let vertex = small.trailing_zeros() as usize;
            small &= small - 1;
            if self.simple[vertex] & other != 0 {
                return Ok(Some(true));
            }
        }
        for edge in &self.complex {
            if !charge(budget, control, 1, 0)? {
                return Ok(None);
            }
            if (edge.left.0 & left == edge.left.0 && edge.right.0 & right == edge.right.0)
                || (edge.right.0 & left == edge.right.0 && edge.left.0 & right == edge.left.0)
            {
                return Ok(Some(true));
            }
        }
        Ok(Some(false))
    }

    fn neighborhood(
        &self,
        set: u128,
        excluded: u128,
        budget: &mut PlanningBudget,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<u128>> {
        let mut simple = 0;
        let mut vertices = set;
        while vertices != 0 {
            if !charge(budget, control, 1, 0)? {
                return Ok(None);
            }
            simple |= self.simple[vertices.trailing_zeros() as usize];
            vertices &= vertices - 1;
        }
        simple &= !(set | excluded);
        let mut neighborhood = simple;
        for edge in &self.complex {
            for (from, target) in [(edge.left.0, edge.right.0), (edge.right.0, edge.left.0)] {
                if !charge(budget, control, 1, 0)? {
                    return Ok(None);
                }
                if from & set != from || target & (set | excluded) != 0 || target & simple != 0 {
                    continue;
                }
                let Some(subsumed) = self.subsumed(target, set, excluded, budget, control)? else {
                    return Ok(None);
                };
                if !subsumed {
                    neighborhood |= target.isolate_lowest_one();
                }
            }
        }
        Ok(Some(neighborhood))
    }

    fn subsumed(
        &self,
        target: u128,
        set: u128,
        excluded: u128,
        budget: &mut PlanningBudget,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<bool>> {
        for edge in &self.complex {
            for (from, other) in [(edge.left.0, edge.right.0), (edge.right.0, edge.left.0)] {
                if !charge(budget, control, 1, 0)? {
                    return Ok(None);
                }
                if from & set == from
                    && other & (set | excluded) == 0
                    && other != target
                    && other & target == other
                {
                    return Ok(Some(true));
                }
            }
        }
        Ok(Some(false))
    }
}

struct NeighborhoodSubsets {
    mask: u128,
    current: u128,
    done: bool,
}
impl NeighborhoodSubsets {
    fn new(mask: u128) -> Self {
        Self {
            mask,
            current: 0,
            done: mask == 0,
        }
    }
}
impl Iterator for NeighborhoodSubsets {
    type Item = u128;
    fn next(&mut self) -> Option<u128> {
        if self.done {
            return None;
        }
        self.current = self.current.wrapping_sub(self.mask) & self.mask;
        self.done = self.current == 0;
        (!self.done).then_some(self.current)
    }
}

struct Enumeration<'a, 'b, 'ctl> {
    graph: &'a Graph,
    connected: BTreeSet<u128>,
    pairs: Vec<ConnectedPair>,
    budget: &'b mut PlanningBudget,
    control: &'b ExecutionControl<'ctl>,
}

impl Enumeration<'_, '_, '_> {
    fn pair(&mut self, left: u128, right: u128) -> QueryResult<bool> {
        if !charge(self.budget, self.control, 1, 0)? {
            return Ok(false);
        }
        let union = left | right;
        if !self.connected.contains(&union) {
            if !charge(self.budget, self.control, 1, 128)? {
                return Ok(false);
            }
            self.connected.insert(union);
        }
        if self.pairs.len() == self.pairs.capacity() {
            let extra = self.pairs.capacity().max(4);
            if !charge(
                self.budget,
                self.control,
                0,
                extra.saturating_mul(std::mem::size_of::<ConnectedPair>()),
            )? {
                return Ok(false);
            }
            if self.pairs.try_reserve_exact(extra).is_err() {
                self.budget.stop();
                return Ok(false);
            }
        }
        self.pairs.push(ConnectedPair { left, right });
        #[cfg(feature = "test-support")]
        crate::performance::record_join_search(1, false, false);
        Ok(true)
    }

    fn emit_csg(&mut self, set: u128) -> QueryResult<bool> {
        let minimum = set.trailing_zeros();
        let excluded = set | low_through(minimum);
        let Some(neighbors) = self
            .graph
            .neighborhood(set, excluded, self.budget, self.control)?
        else {
            return Ok(false);
        };
        let mut remaining = neighbors;
        while remaining != 0 {
            if !charge(self.budget, self.control, 1, 0)? {
                return Ok(false);
            }
            let vertex = 127 - remaining.leading_zeros();
            let seed = 1_u128 << vertex;
            remaining &= !seed;
            let Some(linked) = self.graph.connects(set, seed, self.budget, self.control)? else {
                return Ok(false);
            };
            if linked && !self.pair(set, seed)? {
                return Ok(false);
            }
            let before = seed - 1;
            if !self.grow_cmp(set, seed, excluded | (neighbors & before))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn grow_csg(&mut self, set: u128, excluded: u128) -> QueryResult<bool> {
        let Some(neighbors) = self
            .graph
            .neighborhood(set, excluded, self.budget, self.control)?
        else {
            return Ok(false);
        };
        for added in NeighborhoodSubsets::new(neighbors) {
            if !charge(self.budget, self.control, 1, 0)? {
                return Ok(false);
            }
            if self.connected.contains(&(set | added)) && !self.emit_csg(set | added)? {
                return Ok(false);
            }
        }
        for added in NeighborhoodSubsets::new(neighbors) {
            if !charge(self.budget, self.control, 1, 0)?
                || !self.grow_csg(set | added, excluded | neighbors)?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn grow_cmp(&mut self, left: u128, right: u128, excluded: u128) -> QueryResult<bool> {
        let Some(neighbors) =
            self.graph
                .neighborhood(right, excluded, self.budget, self.control)?
        else {
            return Ok(false);
        };
        for added in NeighborhoodSubsets::new(neighbors) {
            if !charge(self.budget, self.control, 1, 0)? {
                return Ok(false);
            }
            let grown = right | added;
            if self.connected.contains(&grown) {
                let Some(linked) = self
                    .graph
                    .connects(left, grown, self.budget, self.control)?
                else {
                    return Ok(false);
                };
                if linked && !self.pair(left, grown)? {
                    return Ok(false);
                }
            }
        }
        for added in NeighborhoodSubsets::new(neighbors) {
            if !charge(self.budget, self.control, 1, 0)?
                || !self.grow_cmp(left, right | added, excluded | neighbors)?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn low_through(vertex: u32) -> u128 {
    if vertex == 127 {
        u128::MAX
    } else {
        (1_u128 << (vertex + 1)) - 1
    }
}

pub(super) fn enumerate(
    graph: &Graph,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<Vec<ConnectedPair>>> {
    control.check()?;
    if graph.parts > EXACT_PARTS {
        return Ok(None);
    }
    let mut search = Enumeration {
        graph,
        connected: BTreeSet::new(),
        pairs: Vec::new(),
        budget,
        control,
    };
    for part in 0..graph.parts {
        if !charge(search.budget, control, 1, 128)? {
            return Ok(None);
        }
        search.connected.insert(1_u128 << part);
    }
    for part in (0..graph.parts).rev() {
        let seed = 1_u128 << part;
        if !search.emit_csg(seed)? || !search.grow_csg(seed, low_through(part as u32))? {
            return Ok(None);
        }
    }
    bucket_pairs(search.pairs, graph.parts, control, search.budget)
}

fn bucket_pairs(
    pairs: Vec<ConnectedPair>,
    parts: usize,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<Vec<ConnectedPair>>> {
    let bytes = pairs
        .len()
        .saturating_mul(std::mem::size_of::<ConnectedPair>())
        .saturating_add(2 * std::mem::size_of::<[usize; EXACT_PARTS + 1]>());
    if !charge(
        budget,
        control,
        pairs.len().saturating_mul(3).saturating_add(parts),
        bytes,
    )? {
        return Ok(None);
    }
    let mut offsets = [0; EXACT_PARTS + 1];
    for pair in &pairs {
        offsets[(pair.left | pair.right).count_ones() as usize] += 1;
    }
    let mut start = 0;
    for count in offsets.iter_mut().take(parts + 1) {
        let length = *count;
        *count = start;
        start += length;
    }
    let mut sorted = Vec::new();
    if sorted.try_reserve_exact(pairs.len()).is_err() {
        budget.stop();
        return Ok(None);
    }
    sorted.resize(pairs.len(), ConnectedPair { left: 0, right: 0 });
    for pair in pairs {
        control.work(1)?;
        let size = (pair.left | pair.right).count_ones() as usize;
        sorted[offsets[size]] = pair;
        offsets[size] += 1;
    }
    control.check()?;
    Ok(Some(sorted))
}

#[cfg(test)]
mod tests;
