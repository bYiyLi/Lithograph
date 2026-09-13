use super::*;

fn vector_index(hnsw_m: u64) -> IndexDefinition {
    IndexDefinition {
        name: "test_vector".to_owned(),
        kind: StandardIndexKind::Vector,
        target: IndexTarget::NodeProperties {
            label: "Doc".to_owned(),
            properties: vec!["embedding".to_owned()],
        },
        owning_constraint: None,
        labels_or_types: Vec::new(),
        additional_properties: Vec::new(),
        configuration: Some(IndexConfiguration::Vector {
            dimensions: Some(2),
            similarity_function: "euclidean".to_owned(),
            quantization_type: "none".to_owned(),
            default_search_expansion_factor: "1".to_owned(),
            hnsw_m,
            hnsw_ef_construction: 32,
        }),
    }
}

#[test]
fn hnsw_builds_hierarchy_and_search_is_not_an_exact_scan() {
    let mut entries = (1..=512)
        .map(|owner_id| VectorCacheEntry {
            owner_id,
            vector: vec![owner_id as f32, 1.0],
            level: 0,
            neighbors: vec![Vec::new()],
        })
        .collect::<Vec<_>>();
    build_hnsw_graph(&vector_index(4), &mut entries, &|| false).expect("build HNSW graph");

    assert!(entries.iter().any(|entry| entry.level > 0));
    assert!(entries.iter().all(|entry| {
        entry.neighbors.len() == entry.level + 1
            && entry.neighbors.iter().all(|neighbors| neighbors.len() <= 4)
    }));

    let graph = entries
        .iter()
        .cloned()
        .map(|entry| (entry.owner_id, entry))
        .collect::<BTreeMap<_, _>>();
    let max_level = entries.iter().map(|entry| entry.level).max().unwrap_or(0);
    let entry_owner_id = entries
        .iter()
        .filter(|entry| entry.level == max_level)
        .map(|entry| entry.owner_id)
        .min()
        .expect("HNSW entry point");
    let result = hnsw_search(
        &graph,
        entry_owner_id,
        max_level,
        &[256.0, 1.0],
        "euclidean",
        8,
        &|| false,
    )
    .expect("search HNSW graph");

    assert!(!result.items.is_empty());
    assert!(result.items.len() <= 8);
    assert!(
        result.visited_count < entries.len(),
        "ANN search unexpectedly visited every cached vector"
    );

    let exhaustive = hnsw_search(
        &graph,
        entry_owner_id,
        max_level,
        &[256.0, 1.0],
        "euclidean",
        entries.len(),
        &|| false,
    )
    .expect("exhaust HNSW graph");
    assert_eq!(exhaustive.visited_count, entries.len());
}

#[test]
fn hnsw_minimum_connection_count_keeps_level_zero_reachable() {
    let mut entries = (1..=128)
        .map(|owner_id| VectorCacheEntry {
            owner_id,
            vector: vec![owner_id as f32, 1.0],
            level: 0,
            neighbors: vec![Vec::new()],
        })
        .collect::<Vec<_>>();
    build_hnsw_graph(&vector_index(1), &mut entries, &|| false)
        .expect("build minimum-M HNSW graph");

    let graph = entries
        .iter()
        .cloned()
        .map(|entry| (entry.owner_id, entry))
        .collect::<BTreeMap<_, _>>();
    let max_level = entries.iter().map(|entry| entry.level).max().unwrap_or(0);
    let entry_owner_id = entries
        .iter()
        .filter(|entry| entry.level == max_level)
        .map(|entry| entry.owner_id)
        .min()
        .expect("minimum-M HNSW entry point");
    assert!(vector_cache_graph_valid(
        &graph,
        VectorCacheMeta {
            entry_owner_id: Some(entry_owner_id),
            max_level,
            entry_count: entries.len(),
        }
    ));
    let exhaustive = hnsw_search(
        &graph,
        entry_owner_id,
        max_level,
        &[64.0, 1.0],
        "euclidean",
        entries.len(),
        &|| false,
    )
    .expect("exhaust minimum-M HNSW graph");
    assert_eq!(exhaustive.visited_count, entries.len());
}
