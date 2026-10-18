//! Leases for the actually visited lazy-HNSW search state.

use super::*;

impl HnswSearchState {
    pub(super) fn new(
        resources: Option<&super::super::super::resources::QueryResources>,
    ) -> QueryResult<Self> {
        Ok(Self {
            visited: BTreeSet::new(),
            candidates: BinaryHeap::new(),
            nearest: BinaryHeap::new(),
            memory: reserve_hits(resources)?,
        })
    }

    pub(super) fn visit(&mut self, owner_id: i64) -> QueryResult<bool> {
        if self.visited.contains(&owner_id) {
            return Ok(false);
        }
        if let Some(memory) = &mut self.memory {
            memory.grow(16 * std::mem::size_of::<i64>() as u64 + 256)?;
        }
        self.visited.insert(owner_id);
        Ok(true)
    }

    pub(super) fn enqueue(&mut self, item: VectorQueueItem) -> QueryResult<()> {
        reserve_heap_slot(&mut self.candidates, &mut self.memory)?;
        reserve_heap_slot(&mut self.nearest, &mut self.memory)?;
        self.candidates.push(item);
        self.nearest.push(Reverse(item));
        Ok(())
    }

    pub(super) fn keep(&mut self, item: VectorQueueItem, ef: usize) -> QueryResult<()> {
        let should_keep = self.nearest.len() < ef
            || self
                .nearest
                .peek()
                .is_none_or(|Reverse(worst)| item > *worst);
        if should_keep {
            self.enqueue(item)?;
            if self.nearest.len() > ef {
                self.nearest.pop();
            }
        }
        Ok(())
    }
}

fn reserve_heap_slot<T: Ord>(
    heap: &mut BinaryHeap<T>,
    memory: &mut Option<MemoryReservation>,
) -> QueryResult<()> {
    if heap.len() < heap.capacity() {
        return Ok(());
    }
    let capacity = heap
        .capacity()
        .checked_mul(2)
        .ok_or_else(|| memory_error("HNSW queue capacity overflow"))?
        .max(4);
    if let Some(memory) = memory {
        memory.grow(allocation_bytes::<T>(capacity - heap.capacity())?)?;
    }
    heap.try_reserve_exact(capacity - heap.len())
        .map_err(|_| memory_error("cannot allocate HNSW queue"))?;
    Ok(())
}

pub(super) fn reserve_hits(
    resources: Option<&super::super::super::resources::QueryResources>,
) -> QueryResult<Option<MemoryReservation>> {
    resources.map(|resources| resources.reserve(0)).transpose()
}

pub(super) fn reserve_hit_slot(
    hits: &mut Vec<SemanticHit>,
    memory: &mut Option<MemoryReservation>,
) -> QueryResult<()> {
    reserve_search_slot(hits, memory)
}

pub(super) fn sort_hnsw_hits(
    hits: &mut [SemanticHit],
    resources: Option<&crate::query::resources::QueryResources>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let _sorting = resources
        .map(|resources| resources.reserve(allocation_bytes::<SemanticHit>(hits.len())?))
        .transpose()?;
    check_interrupted(is_interrupted)?;
    sort_semantic_hits_with_interrupt(hits, is_interrupted)
}

pub(super) fn ordered_hnsw_items_reserved(
    nearest: BinaryHeap<Reverse<VectorQueueItem>>,
    is_interrupted: &dyn Fn() -> bool,
    resources: Option<&super::super::super::resources::QueryResources>,
) -> QueryResult<(Vec<VectorQueueItem>, Option<MemoryReservation>)> {
    let bytes = allocation_bytes::<VectorQueueItem>(nearest.len())?;
    let memory = resources
        .map(|resources| resources.reserve(bytes))
        .transpose()?;
    let _sorting = resources
        .map(|resources| resources.reserve(bytes))
        .transpose()?;
    let items = ordered_hnsw_items(nearest, is_interrupted)?;
    Ok((items, memory))
}

pub(super) fn ordered_hnsw_items(
    nearest: BinaryHeap<Reverse<VectorQueueItem>>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<VectorQueueItem>> {
    let mut items = Vec::with_capacity(nearest.len());
    for Reverse(item) in nearest {
        check_interrupted(is_interrupted)?;
        items.push(item);
    }
    check_interrupted(is_interrupted)?;
    let mut work = 0_u16;
    let mut interrupted = false;
    items.sort_by(|left, right| {
        if interrupted {
            return Ordering::Equal;
        }
        work += 1;
        if work == 256 {
            work = 0;
            interrupted = is_interrupted();
        }
        if interrupted {
            Ordering::Equal
        } else {
            right.cmp(left)
        }
    });
    if interrupted {
        return Err(QueryError::interrupted());
    }
    check_interrupted(is_interrupted)?;
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{QueryErrorKind, QueryResources};

    #[test]
    fn visited_and_queue_growth_reserve_before_inserting() {
        let resources = QueryResources::new(Some(512));
        let mut state = HnswSearchState::new(Some(&resources)).unwrap();
        assert!(state.visit(1).unwrap());
        assert!(!state.visit(1).unwrap());
        for owner_id in 1..=4 {
            state
                .enqueue(VectorQueueItem {
                    owner_id,
                    score: 1.0,
                })
                .unwrap();
        }
        assert_eq!(resources.used(), 512);
        assert_eq!(
            state
                .enqueue(VectorQueueItem {
                    owner_id: 5,
                    score: 1.0
                })
                .unwrap_err()
                .kind,
            QueryErrorKind::Resource
        );
        assert_eq!(state.candidates.len(), 4);
        assert_eq!(state.nearest.len(), 4);
        assert_eq!(state.visit(2).unwrap_err().kind, QueryErrorKind::Resource);
        assert_eq!(state.visited.len(), 1);
        drop(state);
        assert_eq!(resources.used(), 0);
    }
}
