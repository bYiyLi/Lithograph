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
            && entry
                .neighbors
                .iter()
                .enumerate()
                .all(|(level, neighbors)| neighbors.len() <= if level == 0 { 8 } else { 4 })
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

#[test]
fn persisted_hnsw_search_loads_only_visited_entries_and_detects_corrupt_rows() {
    let connection = Connection::open_in_memory().expect("in-memory sqlite");
    ensure_vector_cache_tables(&connection).expect("vector cache tables");
    let mut entries = (1..=512)
        .map(|owner_id| VectorCacheEntry {
            owner_id,
            vector: vec![owner_id as f32, 1.0],
            level: 0,
            neighbors: vec![Vec::new()],
        })
        .collect::<Vec<_>>();
    build_hnsw_graph(&vector_index(4), &mut entries, &|| false).expect("build HNSW graph");
    persist_vector_cache(&connection, "lazy-test", &entries).expect("persist vector cache");
    let meta = vector_cache_meta(&connection, "lazy-test")
        .expect("cache metadata")
        .expect("complete cache metadata");
    let entry_owner_id = meta.entry_owner_id.expect("entry owner");
    let mut cache = SqlVectorCache {
        connection: &connection,
        cache_key: "lazy-test",
        loaded: BTreeMap::new(),
    };
    let result = hnsw_search_sql(
        &mut cache,
        entry_owner_id,
        meta.max_level,
        &[256.0, 1.0],
        "euclidean",
        8,
        &|| false,
    )
    .expect("lazy HNSW search")
    .expect("valid persisted cache");
    assert!(!result.items.is_empty());
    assert!(result.visited_count < entries.len());
    assert!(cache.loaded.len() < entries.len());

    connection
        .execute(
            "UPDATE temp._lithograph_vector_cache SET neighbors_json='[]' \
             WHERE cache_key='lazy-test' AND owner_id=?1",
            [entry_owner_id],
        )
        .expect("mutate derived cache");
    let mut corrupt_cache = SqlVectorCache {
        connection: &connection,
        cache_key: "lazy-test",
        loaded: BTreeMap::new(),
    };
    assert!(
        hnsw_search_sql(
            &mut corrupt_cache,
            entry_owner_id,
            meta.max_level,
            &[256.0, 1.0],
            "euclidean",
            8,
            &|| false,
        )
        .expect("corrupt cache search")
        .is_none(),
        "corrupt HNSW rows must be detected before they answer a query"
    );
    invalidate_vector_cache(&connection, "lazy-test").expect("invalidate corrupt cache");
    assert!(
        vector_cache_meta(&connection, "lazy-test")
            .expect("metadata after invalidation")
            .is_none()
    );
}

#[test]
fn persisted_lazy_hnsw_matches_in_memory_traversal() {
    let connection = Connection::open_in_memory().expect("in-memory sqlite");
    ensure_vector_cache_tables(&connection).expect("vector cache tables");
    let count = 4_096_i64;
    let mut entries = (1..=count)
        .map(|owner_id| VectorCacheEntry {
            owner_id,
            vector: vec![1.0, owner_id as f32 / count as f32],
            level: 0,
            neighbors: vec![Vec::new()],
        })
        .collect::<Vec<_>>();
    build_hnsw_graph(&vector_index(16), &mut entries, &|| false).expect("build HNSW graph");
    persist_vector_cache(&connection, "equivalence-test", &entries).expect("persist vector cache");
    let graph = entries
        .into_iter()
        .map(|entry| (entry.owner_id, entry))
        .collect::<BTreeMap<_, _>>();
    let meta = vector_cache_meta(&connection, "equivalence-test")
        .expect("cache metadata")
        .expect("complete cache metadata");
    let entry_owner_id = meta.entry_owner_id.expect("entry owner");

    for query in [[1.0, 0.0], [1.0, 0.5], [1.0, 1.0]] {
        for ef_search in [10, 40, 128] {
            let expected = hnsw_search(
                &graph,
                entry_owner_id,
                meta.max_level,
                &query,
                "euclidean",
                ef_search,
                &|| false,
            )
            .expect("in-memory HNSW search");
            let mut cache = SqlVectorCache {
                connection: &connection,
                cache_key: "equivalence-test",
                loaded: BTreeMap::new(),
            };
            let actual = hnsw_search_sql(
                &mut cache,
                entry_owner_id,
                meta.max_level,
                &query,
                "euclidean",
                ef_search,
                &|| false,
            )
            .expect("lazy HNSW search")
            .expect("valid lazy cache");
            assert_eq!(
                actual
                    .items
                    .iter()
                    .map(|item| item.owner_id)
                    .collect::<Vec<_>>(),
                expected
                    .items
                    .iter()
                    .map(|item| item.owner_id)
                    .collect::<Vec<_>>(),
                "query={query:?}, ef={ef_search}"
            );
            assert_eq!(actual.visited_count, expected.visited_count);
        }
    }
}
