use std::collections::{BTreeMap, BTreeSet};

use super::*;

fn add_unrelated_leaf_b_nodes(db: &rusqlite::Connection) {
    let noise = support::write(
        db,
        "UNWIND range(1,2048) AS seq CREATE(n:LeafB) RETURN elementId(n)",
    );
    assert_eq!(noise.rows.len(), 2048);
    assert_eq!(noise.counters.nodes_created, 2048);
    assert_eq!(noise.counters.relationships_created, 0);
    assert_eq!(noise.counters.properties_set, 0);
    let ids = noise
        .rows
        .iter()
        .map(|row| {
            assert_eq!(row.len(), 1);
            let Value::String(id) = &row[0] else {
                panic!("public CREATE must return the actual NodeId")
            };
            assert!(id.starts_with("n:"));
            id.clone()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), 2048);
}

#[test]
fn the_build_arm_shared_label_filters_nonhub_candidates_before_accumulation() {
    let fixture = StarFixture::new(64, 128);
    let chosen = fixture.left[0].endpoint.clone();
    let extra = support::execute(
        &fixture.db,
        "MATCH(a:LeafA) WHERE elementId(a)=$chosen \
         CREATE(h:NonHub {k:7})-[r:A]->(a) \
         RETURN elementId(h),elementId(r),elementId(startNode(r)),elementId(endNode(r)),h.k",
        BTreeMap::from([("chosen".to_owned(), chosen.clone())]),
        ExecutionOptions::default(),
        1,
    )
    .unwrap();
    assert_eq!(extra.rows.len(), 1);
    let bad = &extra.rows[0];
    assert_eq!(bad.len(), 5);
    assert_ne!(bad[0], fixture.hub);
    assert_eq!(bad[2], bad[0]);
    assert_eq!(bad[3], chosen);
    assert_eq!(bad[4], Value::Integer(7));
    assert!(matches!(&bad[1], Value::String(id) if id.starts_with("r:")));
    // Keep the real arms at 128/256 relationships, but make scanning LeafB
    // less selective than LeafA64. The normal CBO must exercise AnyA first.
    add_unrelated_leaf_b_nodes(&fixture.db);
    support::checkpoint(&fixture.db);
    let head = storage::branch_head(&fixture.db, "main").unwrap();
    let query = "MATCH(h)-[r:A]->(a:LeafA),(h:Hub)-[s:B]->(b:LeafB) \
        WHERE h.k=$key \
        RETURN elementId(h),elementId(r),elementId(a),elementId(s),elementId(b)";
    let actual = read(
        &fixture.db,
        &format!("PROFILE {query}"),
        params(),
        ExecutionOptions::default(),
        3,
    );
    let oracle = read(
        &fixture.db,
        &format!(
            "PROFILE {}",
            query.replace("h.k=$key", "(h.k=$key) OR false")
        ),
        params(),
        ExecutionOptions::default(),
        256,
    );
    let expected = fixture.expected_rows();
    assert_eq!(expected.len(), 32768);
    assert_bag(&actual.rows, &expected);
    assert_bag(&oracle.rows, &expected);
    assert!(
        !oracle.plan.contains("FactorizedAccumulate"),
        "{}",
        oracle.plan
    );
    assert_factorized(&actual, 32768);
    let first_access = actual
        .profile
        .iter()
        .find(|operator| operator.db_hits > 0)
        .expect("an actual first storage access");
    assert_eq!(first_access.operator, "LabelIndexScan");
    assert_eq!(first_access.rows, 64, "LeafA64 must supply the first arm");
    assert_eq!(actual_operator(&actual, "FactorizedAccumulate").rows, 128);
    assert!(
        actual
            .rows
            .iter()
            .all(|row| row[0] != bad[0] && row[1] != bad[1])
    );
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), head);
}
