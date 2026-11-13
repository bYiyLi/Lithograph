//! Keep pruning of equal-score neighbors from disconnecting the base graph.

use super::*;

/// Completed entries are in owner order. One bounded forward edge per owner
/// forms a directed cycle; upper layers retain their distance-based links.
pub(super) fn retain_base_cycle(
    entries: &mut [VectorCacheEntry],
    maximum: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    check_interrupted(is_interrupted)?;
    if entries.len() < 2 {
        return Ok(());
    }
    for index in 0..entries.len() {
        check_interrupted(is_interrupted)?;
        let next_owner = entries[(index + 1) % entries.len()].owner_id;
        let neighbors = entries[index]
            .neighbors
            .first_mut()
            .ok_or_else(|| QueryError::internal("HNSW base navigation layer is missing"))?;
        if neighbors.contains(&next_owner) {
            continue;
        }
        // Existing neighbor lists are scored best first. Replace only the
        // last choice when full, preserving the public 2*M base-layer bound.
        if neighbors.len() >= maximum {
            neighbors.pop();
        }
        neighbors.push(next_owner);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryResources;
    use std::cell::Cell;

    fn graph(count: i64, maximum: usize) -> (BTreeMap<i64, VectorCacheEntry>, VectorCacheMeta) {
        let mut entries = (1..=count)
            .map(|owner_id| VectorCacheEntry {
                owner_id,
                vector: vec![1.0, 0.0],
                level: 0,
                neighbors: vec![Vec::new()],
            })
            .collect::<Vec<_>>();
        build_hnsw_graph_with_parameters("cosine", maximum, 64, &mut entries, &|| false).unwrap();
        let max_level = entries.iter().map(|entry| entry.level).max().unwrap_or(0);
        let meta = VectorCacheMeta {
            entry_owner_id: entries
                .iter()
                .filter(|entry| entry.level == max_level)
                .map(|entry| entry.owner_id)
                .min(),
            max_level,
            entry_count: entries.len(),
        };
        (
            entries
                .into_iter()
                .map(|entry| (entry.owner_id, entry))
                .collect(),
            meta,
        )
    }

    #[test]
    fn equal_scores_remain_navigable_without_exceeding_the_neighbor_limits() {
        for (count, maximum) in [(0, 2), (1, 2), (64, 2), (128, 4), (512, 8), (128, 16)] {
            let (entries, meta) = graph(count, maximum);
            assert!(
                vector_cache_graph_valid(&entries, meta),
                "count={count} M={maximum}"
            );
            for entry in entries.values() {
                for (level, neighbors) in entry.neighbors.iter().enumerate() {
                    assert!(neighbors.len() <= if level == 0 { 2 * maximum } else { maximum });
                    assert!(!neighbors.contains(&entry.owner_id));
                }
            }
        }
    }

    #[test]
    fn cached_tied_candidates_fill_the_requested_width_without_invalidating_the_cache() {
        let connection = Connection::open_in_memory().unwrap();
        ensure_vector_cache_tables(&connection).unwrap();
        let (entries, meta) = graph(128, 8);
        let entries = entries.into_values().collect::<Vec<_>>();
        persist_vector_cache(&connection, "tied-candidates", &entries, &|| false).unwrap();
        let changes = connection.total_changes();
        let resources = QueryResources::new(Some(512 * 1024));
        let result = query_managed_vector_cache_controlled(
            &connection,
            ("tied-candidates", Some(&resources)),
            &[1.0, 0.0],
            "cosine",
            40,
            &|| false,
            |id| Ok(Some(SemanticEntity::Node(id))),
        )
        .unwrap()
        .expect("ready tied graph must not request another full build");
        assert_eq!(result.hits.len(), 40);
        assert_eq!(connection.total_changes(), changes);
        assert_eq!(
            vector_cache_meta(&connection, "tied-candidates")
                .unwrap()
                .unwrap()
                .entry_count,
            meta.entry_count
        );
        assert!(result.hits.iter().enumerate().all(|(index, hit)| {
            hit.entity == SemanticEntity::Node(index as i64 + 1) && hit.score == 1.0
        }));
        drop(result);
        assert_eq!(resources.used(), 0);
        assert!(resources.peak() <= 512 * 1024);
    }

    #[test]
    fn retaining_navigation_checks_cancellation_between_owners() {
        let (entries, _) = graph(64, 8);
        let mut entries = entries.into_values().collect::<Vec<_>>();
        let calls = Cell::new(0);
        let error = retain_base_cycle(&mut entries, 16, &|| {
            calls.set(calls.get() + 1);
            calls.get() == 4
        })
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(calls.get(), 4);
    }
}
