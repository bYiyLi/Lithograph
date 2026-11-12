//! Correlated multi-hop arms come only from captured public CREATE identities.

use std::collections::BTreeSet;

use lithograph_core::cypher::Value;
use lithograph_core::query::ExecutionOptions;
use lithograph_core::storage;
use rusqlite::Connection;

use super::support::{self, actual_operator, assert_bag, params, read};

struct HopFixture {
    db: Connection,
    hub: Value,
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
    nodes: BTreeSet<String>,
    relationships: BTreeSet<String>,
}

impl HopFixture {
    fn new() -> Self {
        let db = support::empty_db();
        let created = support::write(&db, "CREATE(h:Hub {k:7}) RETURN elementId(h)");
        assert_eq!(created.rows.len(), 1);
        assert_eq!(created.rows[0].len(), 1);
        let hub = created.rows[0][0].clone();
        for ddl in [
            "CREATE CONSTRAINT s7_hop_hub_type FOR(h:Hub) REQUIRE h.k IS :: INTEGER",
            "CREATE CONSTRAINT s7_hop_hub_unique FOR(h:Hub) REQUIRE h.k IS UNIQUE",
        ] {
            support::write(&db, ddl);
        }
        let mut nodes = BTreeSet::new();
        claim_node(&mut nodes, &hub);
        let mut fixture = Self {
            db,
            hub,
            left: Vec::new(),
            right: Vec::new(),
            nodes,
            relationships: BTreeSet::new(),
        };
        fixture.seed_left();
        fixture
    }

    fn seed_left(&mut self) {
        let created = support::write(
            &self.db,
            "MATCH(h:Hub) WHERE h.k=7 UNWIND range(1,64) AS seq \
             CREATE(a:LeafA),(m:MidA) \
             CREATE(a)-[r:A]->(m),(a)-[q:A]->(m),(m)-[t:T]->(h) \
             RETURN elementId(a),elementId(m),elementId(h),\
                    elementId(r),elementId(startNode(r)),elementId(endNode(r)),\
                    elementId(q),elementId(startNode(q)),elementId(endNode(q)),\
                    elementId(t),elementId(startNode(t)),elementId(endNode(t))",
        );
        assert_eq!(created.rows.len(), 64);
        for row in created.rows {
            assert_eq!(row.len(), 12);
            assert_eq!(row[2], self.hub);
            claim_node(&mut self.nodes, &row[0]);
            claim_node(&mut self.nodes, &row[1]);
            let first = relationship(&mut self.relationships, &row, 3, &row[0], &row[1]);
            let second = relationship(&mut self.relationships, &row, 6, &row[0], &row[1]);
            let link = relationship(&mut self.relationships, &row, 9, &row[1], &row[2]);
            for rel in [first, second] {
                self.left
                    .push(vec![row[0].clone(), rel, row[1].clone(), link.clone()]);
            }
        }
        assert_eq!(self.left.len(), 128);
    }

    fn seed_one_hop_right(&mut self) {
        let created = support::write(
            &self.db,
            "MATCH(h:Hub) WHERE h.k=7 UNWIND range(1,128) AS seq \
             CREATE(b:LeafB) CREATE(h)-[s:B]->(b),(h)-[q:B]->(b) \
             RETURN elementId(h),elementId(b),\
                    elementId(s),elementId(startNode(s)),elementId(endNode(s)),\
                    elementId(q),elementId(startNode(q)),elementId(endNode(q))",
        );
        assert_eq!(created.rows.len(), 128);
        for row in created.rows {
            assert_eq!(row.len(), 8);
            assert_eq!(row[0], self.hub);
            claim_node(&mut self.nodes, &row[1]);
            for slot in [2, 5] {
                let rel = relationship(&mut self.relationships, &row, slot, &row[0], &row[1]);
                self.right.push(vec![rel, row[1].clone()]);
            }
        }
        assert_eq!(self.right.len(), 256);
    }

    fn seed_two_hop_right(&mut self) {
        let created = support::write(
            &self.db,
            "MATCH(h:Hub) WHERE h.k=7 UNWIND range(1,128) AS seq \
             CREATE(n:MidB),(b:LeafB) \
             CREATE(h)-[s:B]->(n),(h)-[q:B]->(n),(n)-[u:U]->(b) \
             RETURN elementId(h),elementId(n),elementId(b),\
                    elementId(s),elementId(startNode(s)),elementId(endNode(s)),\
                    elementId(q),elementId(startNode(q)),elementId(endNode(q)),\
                    elementId(u),elementId(startNode(u)),elementId(endNode(u))",
        );
        assert_eq!(created.rows.len(), 128);
        for row in created.rows {
            assert_eq!(row.len(), 12);
            assert_eq!(row[0], self.hub);
            claim_node(&mut self.nodes, &row[1]);
            claim_node(&mut self.nodes, &row[2]);
            let first = relationship(&mut self.relationships, &row, 3, &row[0], &row[1]);
            let second = relationship(&mut self.relationships, &row, 6, &row[0], &row[1]);
            let link = relationship(&mut self.relationships, &row, 9, &row[1], &row[2]);
            for rel in [first, second] {
                self.right
                    .push(vec![rel, row[1].clone(), link.clone(), row[2].clone()]);
            }
        }
        assert_eq!(self.right.len(), 256);
    }

    fn expected(&self) -> Vec<Vec<Value>> {
        self.left
            .iter()
            .flat_map(|left| {
                self.right.iter().map(move |right| {
                    left.iter()
                        .cloned()
                        .chain(std::iter::once(self.hub.clone()))
                        .chain(right.iter().cloned())
                        .collect()
                })
            })
            .collect()
    }
}

fn claim_node(seen: &mut BTreeSet<String>, value: &Value) {
    let Value::String(id) = value else {
        panic!("CREATE must return typed Node elementId");
    };
    assert!(id.starts_with("n:"));
    assert!(seen.insert(id.clone()), "CREATE reused an actual NodeId");
}

fn relationship(
    seen: &mut BTreeSet<String>,
    row: &[Value],
    slot: usize,
    start: &Value,
    end: &Value,
) -> Value {
    let Value::String(id) = &row[slot] else {
        panic!("CREATE must return typed Relationship elementId");
    };
    assert!(id.starts_with("r:"));
    assert!(
        seen.insert(id.clone()),
        "CREATE reused an actual RelationshipId"
    );
    assert_eq!(&row[slot + 1], start);
    assert_eq!(&row[slot + 2], end);
    row[slot].clone()
}

fn verify(fixture: &HopFixture, text: &str, columns: &[&str]) {
    support::checkpoint(&fixture.db);
    let head = storage::branch_head(&fixture.db, "main").unwrap();
    let expected = fixture.expected();
    assert_eq!(expected.len(), 32768);
    let result = read(
        &fixture.db,
        &format!("PROFILE {text}"),
        params(),
        ExecutionOptions::default(),
        3,
    );
    let columns = columns
        .iter()
        .map(|column| (*column).to_owned())
        .collect::<Vec<_>>();
    assert_eq!(result.columns, columns);
    assert_bag(&result.rows, &expected);
    let accumulated = actual_operator(&result, "FactorizedAccumulate");
    assert!(accumulated.rows > 0);
    let group = accumulated
        .factorized
        .as_ref()
        .expect("actual multi-hop factor group");
    let mut arms = [group.accumulate_rows, group.build_rows];
    arms.sort_unstable();
    assert_eq!(arms, [128, 256], "{group:?}");
    assert_eq!(group.prefixes, 1);
    assert_eq!(group.group_members, 384);
    assert!(group.memory_peak_bytes > 0);
    let flatten = actual_operator(&result, "Flatten");
    assert_eq!(flatten.rows, 32768);
    assert_eq!(
        flatten
            .factorized
            .as_ref()
            .expect("actual multi-hop Flatten")
            .flattened_rows,
        32768
    );
    assert!(
        !result.plan.contains("ASPJoin"),
        "these are two correlated arms, not a third-Part ASP join"
    );
    assert_eq!(text.matches("WHERE h.k=$key").count(), 1);
    let flat = text.replace("WHERE h.k=$key", "WHERE (h.k=$key) OR false");
    let original = read(
        &fixture.db,
        &format!("PROFILE {flat}"),
        params(),
        ExecutionOptions::default(),
        7,
    );
    assert_eq!(original.columns, columns);
    assert_bag(&original.rows, &expected);
    assert!(
        !original.plan.contains("FactorizedAccumulate"),
        "{}",
        original.plan
    );
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), head);
}

#[test]
fn three_hops_preserve_correlated_left_rows_and_all_parallel_relationships() {
    let mut fixture = HopFixture::new();
    fixture.seed_one_hop_right();
    assert_eq!(fixture.nodes.len(), 257);
    assert_eq!(fixture.relationships.len(), 448);
    verify(
        &fixture,
        "MATCH(a:LeafA)-[r:A]->(m:MidA),(m)-[t:T]->(h:Hub),(h)-[s:B]->(b:LeafB) \
         WHERE h.k=$key \
         RETURN elementId(a),elementId(r),elementId(m),elementId(t),elementId(h),elementId(s),elementId(b)",
        &[
            "elementId(a)",
            "elementId(r)",
            "elementId(m)",
            "elementId(t)",
            "elementId(h)",
            "elementId(s)",
            "elementId(b)",
        ],
    );
}

#[test]
fn four_hops_preserve_both_correlated_arms_and_the_complete_nine_id_bag() {
    let mut fixture = HopFixture::new();
    fixture.seed_two_hop_right();
    assert_eq!(fixture.nodes.len(), 385);
    assert_eq!(fixture.relationships.len(), 576);
    verify(
        &fixture,
        "MATCH(a:LeafA)-[r:A]->(m:MidA),(m)-[t:T]->(h:Hub),\
              (h)-[s:B]->(n:MidB),(n)-[u:U]->(b:LeafB) \
         WHERE h.k=$key \
         RETURN elementId(a),elementId(r),elementId(m),elementId(t),elementId(h),\
                elementId(s),elementId(n),elementId(u),elementId(b)",
        &[
            "elementId(a)",
            "elementId(r)",
            "elementId(m)",
            "elementId(t)",
            "elementId(h)",
            "elementId(s)",
            "elementId(n)",
            "elementId(u)",
            "elementId(b)",
        ],
    );
}
