//! Ordered Node intersections retain the complete Cypher relationship bag.

use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::ExecutionOptions;
use lithograph_core::storage;

#[path = "phase22_factorized/support.rs"]
pub mod support;

#[path = "phase22_multiway/fixture.rs"]
mod fixture;

#[path = "phase22_multiway/semantics.rs"]
mod semantics;

#[path = "phase22_multiway/state.rs"]
mod state;

#[path = "phase22_multiway/lifecycle.rs"]
mod lifecycle;

#[path = "phase22_multiway/mixed.rs"]
mod mixed;

#[path = "phase22_multiway/component.rs"]
mod component;

#[path = "phase22_multiway/component_hash.rs"]
mod component_hash;

use fixture::{CycleFixture, Shape};
use support::{actual_operator, assert_bag, read};

fn assert_multiway(result: &support::Observed, rows: usize) {
    let operator = actual_operator(result, "MultiwayIntersect");
    assert_eq!(operator.rows, rows as u64, "{:?}", result.profile);
    let work = operator
        .multiway
        .as_ref()
        .expect("actual intersection work");
    assert!(work.input_lists > 0 && work.seek_operations > 0, "{work:?}");
    assert!(work.intersection_steps > 0, "{work:?}");
    assert!(
        work.candidate_nodes > 0 && work.node_assignments > 0,
        "{work:?}"
    );
    assert!(work.relationship_candidates >= rows as u64, "{work:?}");
    assert!(work.memory_peak_bytes > 0, "{work:?}");
}

#[test]
fn triangle_four_cycle_clique_and_shared_cycles_deliver_every_identity_tuple() {
    for (shape, tuples) in [
        (Shape::Triangle, 352),
        (Shape::FourCycle, 5056),
        (Shape::Clique, 1920),
        (Shape::SharedCycles, 16),
    ] {
        let fixture = CycleFixture::new(shape, 16, 1);
        let expected = fixture.expected();
        assert_eq!(expected.len(), tuples);
        let before = storage::branch_head(&fixture.db, "main").unwrap();
        let result = read(
            &fixture.db,
            &format!("PROFILE {}", fixture.query()),
            BTreeMap::new(),
            ExecutionOptions::default(),
            17,
        );
        assert_bag(&result.rows, &expected);
        assert_multiway(&result, expected.len());
        assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
        assert!(fixture.db.is_autocommit());
    }
}

#[test]
fn parallel_relationships_are_enumerated_after_the_node_intersection() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 2);
    let expected = fixture.expected();
    assert_eq!(expected.len(), 704);
    assert_eq!(
        expected
            .iter()
            .map(|row| format!("{:?}", &row[..3]))
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        352,
        "each sparse Node assignment is expanded into both first-edge IDs"
    );
    let result = read(
        &fixture.db,
        &format!("PROFILE {}", fixture.query()),
        BTreeMap::new(),
        ExecutionOptions::default(),
        3,
    );
    assert_bag(&result.rows, &expected);
    assert_multiway(&result, expected.len());
    assert_eq!(
        result
            .rows
            .iter()
            .map(|row| format!("{row:?}"))
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        expected.len(),
        "every parallel RelationshipId contributes its own tuple"
    );
}

#[test]
fn dense_hub_triangle_retains_the_measured_cheaper_binary_plan() {
    // Native cost diagnosis measured binary work 8451 against WCOJ 27006.
    // This single-root triangle closes through a degree-one edge; it is an
    // explicit fallback example, alongside the four sparse cyclic positives.
    let fixture = CycleFixture::dense_hub(Shape::Triangle, 32);
    let expected = fixture.expected();
    assert_eq!(expected.len(), 1024);
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    let result = read(
        &fixture.db,
        &format!("PROFILE {}", fixture.query()),
        BTreeMap::new(),
        ExecutionOptions::default(),
        17,
    );
    assert_bag(&result.rows, &expected);
    assert!(
        !result.plan.contains("MultiwayIntersect"),
        "a cycle alone is not a work-saving proof: {}",
        result.plan
    );
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
    assert!(fixture.db.is_autocommit());
}
