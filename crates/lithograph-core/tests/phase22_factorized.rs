//! Factorized packets preserve complete relationship identity bags at Flatten.

use lithograph_core::cypher::Value;
use lithograph_core::query::ExecutionOptions;
use lithograph_core::storage;

#[path = "phase22_factorized/support.rs"]
pub mod support;

#[path = "phase22_factorized/lifecycle.rs"]
mod lifecycle;

#[path = "phase22_factorized/lifecycle_state.rs"]
mod lifecycle_state;

#[path = "phase22_factorized/semantics.rs"]
mod semantics;

#[path = "phase22_factorized/shared_labels.rs"]
mod shared_labels;

#[path = "phase22_factorized/hops.rs"]
mod hops;

use support::{STAR, StarFixture, actual_operator, assert_bag, params, read};

fn assert_factorized(result: &support::Observed, output_rows: u64) {
    let accumulate = actual_operator(result, "FactorizedAccumulate");
    assert!(
        accumulate.rows > 0,
        "a static plan name is not an executed factor table: {:?}",
        result.profile
    );
    let stats = accumulate
        .factorized
        .as_ref()
        .expect("actual factor table runtime counters");
    assert!(stats.prefixes > 0 && stats.accumulate_rows > 0);
    assert!(stats.group_members > 0 && stats.group_members < output_rows);
    assert!(stats.memory_peak_bytes > 0);
    let flatten = actual_operator(result, "Flatten");
    assert_eq!(flatten.rows, output_rows);
    assert_eq!(
        flatten
            .factorized
            .as_ref()
            .expect("actual Flatten runtime counters")
            .flattened_rows,
        output_rows
    );
}

#[test]
fn public_create_parallel_relationships_keep_the_complete_32768_five_id_bag() {
    let fixture = StarFixture::new(64, 128);
    let expected = fixture.expected_rows();
    assert_eq!(fixture.left.len(), 128);
    assert_eq!(fixture.right.len(), 256);
    assert_eq!(expected.len(), 32768);
    let head = storage::branch_head(&fixture.db, "main").unwrap();
    let result = read(
        &fixture.db,
        &format!("PROFILE {STAR}"),
        params(),
        ExecutionOptions::default(),
        3,
    );
    assert_eq!(
        result.columns,
        [
            "elementId(h)",
            "elementId(r)",
            "elementId(a)",
            "elementId(s)",
            "elementId(b)",
        ]
    );
    assert_bag(&result.rows, &expected);
    assert_factorized(&result, 32768);
    let accumulate = actual_operator(&result, "FactorizedAccumulate");
    assert_eq!(accumulate.rows, 128);
    let stats = accumulate.factorized.as_ref().unwrap();
    assert_eq!(stats.prefixes, 1);
    assert_eq!(stats.accumulate_rows, 128);
    assert_eq!(stats.group_members, 384);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), head);
}

#[test]
fn overlapping_relationship_type_domains_fall_back_without_reusing_a_relationship() {
    let fixture = StarFixture::new(2, 2);
    let text = "MATCH(h:Hub)-[r:A]->(a:LeafA),(h)-[s:A]->(b:LeafA) \
        WHERE h.k=$key \
        RETURN elementId(h),elementId(r),elementId(a),elementId(s),elementId(b)";
    let hub = &fixture.hub;
    let expected = fixture
        .left
        .iter()
        .flat_map(|left| {
            fixture
                .left
                .iter()
                .filter(move |right| left.relationship != right.relationship)
                .map(move |right| {
                    vec![
                        hub.clone(),
                        left.relationship.clone(),
                        left.endpoint.clone(),
                        right.relationship.clone(),
                        right.endpoint.clone(),
                    ]
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 12);
    let result = read(
        &fixture.db,
        &format!("PROFILE {text}"),
        params(),
        ExecutionOptions::default(),
        1,
    );
    assert_bag(&result.rows, &expected);
    assert!(
        !result.plan.contains("FactorizedAccumulate") && !result.plan.contains("ASPJoin"),
        "unproved cross-arm uniqueness must use the original path: {}",
        result.plan
    );
}

#[test]
fn a_missing_prefix_does_not_manufacture_an_empty_product_row() {
    let fixture = StarFixture::new(2, 2);
    let mut bindings = params();
    bindings.insert("key".to_owned(), Value::Integer(8));
    let result = read(
        &fixture.db,
        &format!("PROFILE {STAR}"),
        bindings,
        ExecutionOptions::default(),
        1,
    );
    assert!(result.rows.is_empty());
    assert!(result.profile.iter().all(|operator| {
        !matches!(
            operator.operator.as_str(),
            "Flatten" | "FactorizedAccumulate"
        ) || operator.rows == 0
    }));
}

#[test]
fn distinct_relationship_self_loops_preserve_one_complete_identity_tuple() {
    let db = support::empty_db();
    let seed = support::write(
        &db,
        "CREATE(h:Hub:LeafA:LeafB {k:7}) \
         CREATE(h)-[r:A]->(h),(h)-[s:B]->(h) \
         RETURN elementId(h),elementId(r),elementId(s)",
    );
    assert_eq!(seed.rows.len(), 1);
    let ids = &seed.rows[0];
    assert_ne!(ids[1], ids[2]);
    let expected = vec![vec![
        ids[0].clone(),
        ids[1].clone(),
        ids[0].clone(),
        ids[2].clone(),
        ids[0].clone(),
    ]];
    let result = read(
        &db,
        &format!("PROFILE {STAR}"),
        params(),
        ExecutionOptions::default(),
        1,
    );
    // This tiny graph may legitimately retain a cheaper flat plan. The large
    // parallel fixture separately requires actual factorized execution.
    assert_bag(&result.rows, &expected);
}
