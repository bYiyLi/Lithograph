use super::*;
use crate::query::QueryResources;
use crate::query::plan::join::PartSet;

fn edges(pairs: &[(usize, usize)]) -> Vec<Hyperedge> {
    pairs
        .iter()
        .map(|(left, right)| Hyperedge {
            left: PartSet(1_u128 << left),
            right: PartSet(1_u128 << right),
        })
        .collect()
}

fn linked(left: u128, right: u128, edges: &[Hyperedge]) -> bool {
    edges.iter().any(|edge| {
        (edge.left.0 & left == edge.left.0 && edge.right.0 & right == edge.right.0)
            || (edge.right.0 & left == edge.right.0 && edge.left.0 & right == edge.left.0)
    })
}

// A complex edge cannot create a child plan whose own vertices are disconnected.
// Buildability comes from independent split-DP, not one flood expansion.
fn connected_sets(parts: usize, edges: &[Hyperedge]) -> Vec<bool> {
    let mut connected = vec![false; 1 << parts];
    for mask in 1usize..(1 << parts) {
        if mask.is_power_of_two() {
            connected[mask] = true;
            continue;
        }
        let mut left = (mask - 1) & mask;
        while left != 0 {
            let right = mask ^ left;
            if connected[left] && connected[right] && linked(left as u128, right as u128, edges) {
                connected[mask] = true;
                break;
            }
            left = (left - 1) & mask;
        }
    }
    connected
}

/// Deliberately exhaustive *test oracle*, independent of production traversal.
fn oracle(parts: usize, edges: &[Hyperedge]) -> BTreeSet<ConnectedPair> {
    let mut expected = BTreeSet::new();
    let connected = connected_sets(parts, edges);
    for left in 1_u128..(1 << parts) {
        if !connected[left as usize] {
            continue;
        }
        for right in 1_u128..(1 << parts) {
            if left & right != 0
                || left.trailing_zeros() >= right.trailing_zeros()
                || !connected[right as usize]
            {
                continue;
            }
            if linked(left, right, edges) {
                expected.insert(ConnectedPair { left, right });
            }
        }
    }
    expected
}

fn check(parts: usize, pairs: &[(usize, usize)]) {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut budget = PlanningBudget::default();
    let edges = edges(pairs);
    let graph = Graph::new(parts, &edges, &mut budget, &control)
        .unwrap()
        .unwrap();
    let actual = enumerate(&graph, &control, &mut budget).unwrap().unwrap();
    assert_eq!(
        actual.len(),
        actual.iter().copied().collect::<BTreeSet<_>>().len(),
        "duplicate pair"
    );
    assert_eq!(
        actual.iter().copied().collect::<BTreeSet<_>>(),
        oracle(parts, &edges)
    );
    assert!(actual.windows(2).all(|p| (p[0].left|p[0].right).count_ones() <= (p[1].left|p[1].right).count_ones()));
}

#[test]
fn canonical_pairs_match_independent_graph_oracle() {
    check(1, &[]);
    check(4, &[(0, 1), (1, 2), (2, 3)]);
    check(5, &[(0, 1), (0, 2), (0, 3), (0, 4)]);
    check(4, &[(0, 1), (1, 2), (2, 3), (3, 0)]);
    check(4, &[(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)]);
    check(5, &[(0, 1), (2, 3)]);
}

#[test]
fn every_small_binary_graph_has_exactly_its_connected_pairs() {
    let possible = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];
    for graph in 0..64 {
        let selected = possible
            .iter()
            .enumerate()
            .filter_map(|(bit, pair)| (graph & (1 << bit) != 0).then_some(*pair))
            .collect::<Vec<_>>();
        check(4, &selected);
    }
}

#[test]
fn true_hyperedge_requires_both_whole_sides() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut budget = PlanningBudget::default();
    let mut edges = edges(&[(0, 1), (2, 3)]);
    edges.push(Hyperedge {
        left: PartSet(3),
        right: PartSet(12),
    });
    let graph = Graph::new(4, &edges, &mut budget, &control)
        .unwrap()
        .unwrap();
    let actual = enumerate(&graph, &control, &mut budget).unwrap().unwrap();
    assert_eq!(
        actual.iter().copied().collect::<BTreeSet<_>>(),
        oracle(4, &edges)
    );
    assert!(actual.contains(&ConnectedPair { left: 3, right: 12 }));
    assert!(!graph.connects_singleton(3, 2));
}

#[test]
fn large_and_dense_search_stays_inside_shared_ledger() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    for parts in [8, 24, 128] {
        let mut budget = PlanningBudget::default();
        let edges = edges(
            &(0..parts)
                .flat_map(|a| ((a + 1)..parts).map(move |b| (a, b)))
                .collect::<Vec<_>>(),
        );
        let edges = if parts > 8 {
            self::edges(&(0..parts - 1).map(|p| (p, p + 1)).collect::<Vec<_>>())
        } else {
            edges
        };
        let graph = Graph::new(parts, &edges, &mut budget, &control)
            .unwrap()
            .unwrap();
        assert!(enumerate(&graph, &control, &mut budget).unwrap().is_none());
        assert!(budget.states <= 4096 && budget.bytes <= 256 * 1024);
    }
}

#[test]
fn cancellation_is_not_optional_budget_fallback() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| true);
    let mut budget = PlanningBudget::default();
    let graph = Graph::new(
        3,
        &edges(&[(0, 1), (1, 2)]),
        &mut budget,
        &ExecutionControl::new(&resources, &|| false),
    )
    .unwrap()
    .unwrap();
    assert!(enumerate(&graph, &control, &mut budget).is_err());
}

#[test]
fn mixed_five_vertex_hypergraphs_use_buildable_children() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    for complex in [(3, 12), (3, 24), (5, 24), (1, 12), (3, 16)] {
        let mut edges = edges(&[(0, 1), (1, 2), (2, 3), (3, 4)]);
        edges.push(Hyperedge {
            left: PartSet(complex.0),
            right: PartSet(complex.1),
        });
        let mut budget = PlanningBudget::default();
        let graph = Graph::new(5, &edges, &mut budget, &control)
            .unwrap()
            .unwrap();
        let pairs = enumerate(&graph, &control, &mut budget).unwrap().unwrap();
        assert_eq!(
            pairs.iter().copied().collect::<BTreeSet<_>>(),
            oracle(5, &edges)
        );
    }
}
