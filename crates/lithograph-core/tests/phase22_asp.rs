//! ASP uses an independent third Part after the complete factorized star.

use std::collections::{BTreeMap, BTreeSet};

use lithograph_core::cypher::Value;
use lithograph_core::query::ExecutionOptions;
use lithograph_core::storage;

#[path = "phase22_factorized/support.rs"]
pub mod support;

use support::{ArmRow, StarFixture, actual_operator, assert_bag, params, read};

const ASP: &str = "MATCH (h:Hub)-[r:A]->(a:LeafA),(h)-[s:B]->(b:LeafB),(d:Build) \
    WHERE h.k=$key AND a=d \
    RETURN elementId(h),elementId(r),elementId(a),elementId(s),elementId(b),elementId(d)";

struct AspFixture {
    star: StarFixture,
    expected: Vec<Vec<Value>>,
}

impl AspFixture {
    fn new() -> Self {
        let mut star = StarFixture::new(2, 2);
        // Repeated A identities make completing the semijoin ID set useful:
        // 128 A relationships map to only two distinct target NodeIds.
        let extra = support::write(
            &star.db,
            "MATCH (h:Hub),(a:LeafA) WHERE h.k=7 \
             UNWIND range(1,62) AS seq CREATE(h)-[r:A]->(a) \
             RETURN elementId(r),elementId(a),elementId(startNode(r)),elementId(endNode(r))",
        );
        assert_eq!(extra.rows.len(), 124);
        for row in extra.rows {
            assert_eq!(row.len(), 4);
            assert_eq!(row[2], star.hub);
            assert_eq!(row[3], row[1]);
            star.left.push(ArmRow {
                relationship: row[0].clone(),
                endpoint: row[1].clone(),
            });
        }
        assert_eq!(star.left.len(), 128);
        assert_eq!(star.right.len(), 4);
        assert!(
            star.left.iter().all(|row| {
                matches!(&row.relationship, Value::String(id) if id.starts_with("r:"))
            })
        );
        assert_eq!(
            star.left
                .iter()
                .map(|row| format!("{:?}", row.relationship))
                .collect::<BTreeSet<_>>()
                .len(),
            128
        );
        let selected = star.left[0].endpoint.clone();
        let labelled = support::execute(
            &star.db,
            "MATCH(a:LeafA) WHERE elementId(a)=$selected SET a:Build RETURN elementId(a)",
            BTreeMap::from([("selected".to_owned(), selected.clone())]),
            ExecutionOptions::default(),
            1,
        )
        .unwrap();
        assert_eq!(labelled.rows, vec![vec![selected.clone()]]);
        let noise = support::write(
            &star.db,
            "UNWIND range(1,64) AS seq CREATE(d:Build {noise:seq}) RETURN elementId(d)",
        );
        assert_eq!(noise.rows.len(), 64);
        assert!(noise.rows.iter().all(|row| {
            matches!(row.as_slice(), [Value::String(id)] if id.starts_with("n:"))
                && row[0] != selected
        }));
        assert_eq!(
            noise
                .rows
                .iter()
                .map(|row| format!("{row:?}"))
                .collect::<BTreeSet<_>>()
                .len(),
            64
        );
        support::checkpoint(&star.db);
        let expected = star
            .expected_rows()
            .into_iter()
            .filter(|row| row[2] == selected)
            .map(|mut row| {
                row.push(row[2].clone());
                row
            })
            .collect::<Vec<_>>();
        assert_eq!(expected.len(), 256);
        Self { star, expected }
    }
}

#[test]
fn third_part_identity_join_keeps_the_complete_parallel_six_id_bag() {
    let fixture = AspFixture::new();
    let head = storage::branch_head(&fixture.star.db, "main").unwrap();
    let result = read(
        &fixture.star.db,
        &format!("PROFILE {ASP}"),
        params(),
        ExecutionOptions::default(),
        1,
    );
    assert_eq!(
        result.columns,
        [
            "elementId(h)",
            "elementId(r)",
            "elementId(a)",
            "elementId(s)",
            "elementId(b)",
            "elementId(d)"
        ]
    );
    assert_bag(&result.rows, &fixture.expected);
    let accumulated = actual_operator(&result, "FactorizedAccumulate");
    assert_eq!(accumulated.rows, 4);
    let table = accumulated
        .factorized
        .as_ref()
        .expect("real accumulation metrics");
    // The cheaper B arm supplies h first, then its bound A arm is retained.
    assert_eq!(table.accumulate_rows, 4);
    assert_eq!(table.build_rows, 128);
    assert_eq!(table.group_members, 132);
    let joined = actual_operator(&result, "ASPJoin");
    assert!(joined.rows > 0);
    let asp = joined
        .factorized
        .as_ref()
        .expect("real ASP pipeline metrics");
    // The complete A arm contains two distinct IDs, one with Build membership.
    // Reading only those IDs must skip the 64 unrelated Build nodes physically.
    assert_eq!(asp.semijoin_keys, 2, "{asp:?}");
    assert_eq!(asp.semijoin_seeks, 2, "{asp:?}");
    assert_eq!(asp.build_rows, 1, "{asp:?}");
    assert_eq!(asp.hash_entries, 1, "{asp:?}");
    assert_eq!(asp.accumulate_rows, 128, "{asp:?}");
    assert_eq!(asp.probe_rows, 128, "{asp:?}");
    assert_eq!(asp.group_members, 68, "{asp:?}");
    let flattened = actual_operator(&result, "Flatten");
    assert_eq!(flattened.rows, 256);
    assert_eq!(
        flattened
            .factorized
            .as_ref()
            .expect("real Flatten metrics")
            .flattened_rows,
        256
    );
    assert_eq!(
        storage::branch_head(&fixture.star.db, "main").unwrap(),
        head
    );
}

#[test]
fn null_prefix_key_falls_back_without_accumulating_or_building() {
    let fixture = AspFixture::new();
    let mut bindings = params();
    bindings.insert("key".to_owned(), Value::Null);
    let head = storage::branch_head(&fixture.star.db, "main").unwrap();
    let result = read(
        &fixture.star.db,
        &format!("PROFILE {ASP}"),
        bindings,
        ExecutionOptions::default(),
        1,
    );
    assert!(result.rows.is_empty());
    assert!(!result.plan.contains("ASPJoin"), "{}", result.plan);
    assert!(result.profile.iter().all(|operator| {
        !matches!(
            operator.operator.as_str(),
            "ASPJoin" | "FactorizedAccumulate"
        ) || operator.rows == 0
    }));
    assert_eq!(
        storage::branch_head(&fixture.star.db, "main").unwrap(),
        head
    );
}
