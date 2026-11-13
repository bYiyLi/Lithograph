//! Captured public CREATE IDs, never an engine query, define each expected bag.

use std::collections::{BTreeMap, BTreeSet};

use lithograph_core::cypher::Value;
use rusqlite::Connection;

use super::support;

#[derive(Clone, Copy, Debug)]
pub enum Shape {
    Triangle,
    FourCycle,
    Clique,
    SharedCycles,
}

#[derive(Clone, Copy, Debug)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub kind: &'static str,
}

pub struct CycleFixture {
    pub db: Connection,
    pub nodes: Vec<Vec<Value>>,
    pub edges: Vec<Edge>,
    pub relationships: Vec<BTreeMap<(usize, usize), Vec<Value>>>,
    degree: Option<usize>,
}

impl CycleFixture {
    pub fn new(shape: Shape, width: usize, first_edge_copies: usize) -> Self {
        Self::with_db(support::empty_db(), shape, width, first_edge_copies)
    }

    pub fn with_db(db: Connection, shape: Shape, width: usize, first_edge_copies: usize) -> Self {
        assert!(width >= 2);
        Self::seed(db, shape, width, first_edge_copies, false)
    }

    pub fn dense_hub(shape: Shape, width: usize) -> Self {
        Self::seed(support::empty_db(), shape, width, 1, true)
    }

    fn seed(
        db: Connection,
        shape: Shape,
        width: usize,
        first_edge_copies: usize,
        dense_hub: bool,
    ) -> Self {
        assert!(width > 0 && first_edge_copies > 0);
        let edges = shape.edges();
        let count = edges
            .iter()
            .flat_map(|edge| [edge.from, edge.to])
            .max()
            .unwrap()
            + 1;
        let mut nodes = Vec::new();
        let mut identities = BTreeSet::new();
        for domain in 0..count {
            let size = if dense_hub && domain == 0 { 1 } else { width };
            let seed = support::write(
                &db,
                &format!(
                    "UNWIND range(0,{}) AS seq CREATE(n:S8D{domain} {{seq:seq}}) RETURN seq,elementId(n)",
                    size - 1
                ),
            );
            assert_eq!(seed.rows.len(), size);
            let mut captured = vec![Value::Null; size];
            for row in seed.rows {
                let [Value::Integer(seq), Value::String(id)] = row.as_slice() else {
                    panic!("public CREATE returns typed sequence and NodeId: {row:?}");
                };
                assert!(id.starts_with("n:") && identities.insert(id.clone()));
                captured[usize::try_from(*seq).unwrap()] = row[1].clone();
            }
            assert!(!captured.contains(&Value::Null));
            nodes.push(captured);
        }
        let mut fixture = Self {
            db,
            nodes,
            edges,
            relationships: Vec::new(),
            degree: (!dense_hub).then_some(width / 2),
        };
        for edge in 0..fixture.edges.len() {
            fixture.create_edge(edge, if edge == 0 { first_edge_copies } else { 1 });
        }
        support::checkpoint(&fixture.db);
        fixture
    }

    fn create_edge(&mut self, ordinal: usize, copies: usize) {
        let edge = self.edges[ordinal];
        let predicate = self.degree.map_or_else(String::new, |degree| {
            let width = self.nodes[edge.from].len();
            format!(" WHERE ((b.seq-a.seq+{width})%{width})<{degree}")
        });
        let seeded = support::write(
            &self.db,
            &format!(
                "MATCH(a:S8D{}),(b:S8D{}){predicate} UNWIND range(1,{copies}) AS copy \
                 CREATE(a)-[r:{}]->(b) RETURN a.seq,b.seq,elementId(a),elementId(b),\
                 elementId(r),elementId(startNode(r)),elementId(endNode(r)),copy",
                edge.from, edge.to, edge.kind
            ),
        );
        assert_eq!(
            seeded.rows.len(),
            self.nodes[edge.from].len() * self.degree.unwrap_or(self.nodes[edge.to].len()) * copies
        );
        let mut relationships = BTreeMap::<(usize, usize), Vec<Value>>::new();
        let mut ids = BTreeSet::new();
        for row in seeded.rows {
            let Value::Integer(from) = row[0] else {
                panic!("typed source sequence")
            };
            let Value::Integer(to) = row[1] else {
                panic!("typed target sequence")
            };
            let key = (usize::try_from(from).unwrap(), usize::try_from(to).unwrap());
            assert_eq!(row[2], self.nodes[edge.from][key.0]);
            assert_eq!(row[3], self.nodes[edge.to][key.1]);
            assert_eq!(row[5], row[2]);
            assert_eq!(row[6], row[3]);
            assert!(
                matches!(&row[4], Value::String(id) if id.starts_with("r:") && ids.insert(id.clone()))
            );
            relationships.entry(key).or_default().push(row[4].clone());
        }
        self.relationships.push(relationships);
    }

    pub fn query(&self) -> String {
        let pattern = self
            .edges
            .iter()
            .enumerate()
            .map(|(ordinal, edge)| {
                format!(
                    "(n{}:S8D{})-[r{ordinal}:{}]->(n{}:S8D{})",
                    edge.from, edge.from, edge.kind, edge.to, edge.to
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let projection = (0..self.nodes.len())
            .map(|node| format!("elementId(n{node})"))
            .chain((0..self.edges.len()).map(|edge| format!("elementId(r{edge})")))
            .collect::<Vec<_>>()
            .join(",");
        format!("MATCH {pattern} RETURN {projection}")
    }

    pub fn expected(&self) -> Vec<Vec<Value>> {
        let mut result = Vec::new();
        self.node_assignments(&mut Vec::new(), &mut result);
        result
    }

    fn node_assignments(&self, selected: &mut Vec<usize>, result: &mut Vec<Vec<Value>>) {
        if selected.len() < self.nodes.len() {
            for node in 0..self.nodes[selected.len()].len() {
                selected.push(node);
                self.node_assignments(selected, result);
                selected.pop();
            }
            return;
        }
        let mut row = selected
            .iter()
            .enumerate()
            .map(|(domain, seq)| self.nodes[domain][*seq].clone())
            .collect::<Vec<_>>();
        self.relationship_assignments(selected, 0, &mut row, &mut BTreeSet::new(), result);
    }

    fn relationship_assignments(
        &self,
        selected: &[usize],
        ordinal: usize,
        row: &mut Vec<Value>,
        used: &mut BTreeSet<String>,
        result: &mut Vec<Vec<Value>>,
    ) {
        if ordinal == self.edges.len() {
            result.push(row.clone());
            return;
        }
        let edge = self.edges[ordinal];
        if let Some(relationships) =
            self.relationships[ordinal].get(&(selected[edge.from], selected[edge.to]))
        {
            for id in relationships {
                let key = format!("{id:?}");
                if used.insert(key.clone()) {
                    row.push(id.clone());
                    self.relationship_assignments(selected, ordinal + 1, row, used, result);
                    row.pop();
                    used.remove(&key);
                }
            }
        }
    }
}

impl Shape {
    fn edges(self) -> Vec<Edge> {
        let endpoints: &[(_, _, _)] = match self {
            Self::Triangle => &[(0, 1, "S8A"), (1, 2, "S8B"), (2, 0, "S8C")],
            Self::FourCycle => &[(0, 1, "S8A"), (1, 2, "S8B"), (2, 3, "S8C"), (3, 0, "S8D")],
            Self::Clique => &[
                (0, 1, "S8A"),
                (0, 2, "S8B"),
                (0, 3, "S8C"),
                (1, 2, "S8D"),
                (1, 3, "S8E"),
                (2, 3, "S8F"),
            ],
            Self::SharedCycles => &[
                (0, 1, "S8A"),
                (1, 2, "S8B"),
                (2, 0, "S8C"),
                (0, 2, "S8D"),
                (2, 3, "S8E"),
                (3, 0, "S8F"),
            ],
        };
        endpoints
            .iter()
            .map(|&(from, to, kind)| Edge { from, to, kind })
            .collect()
    }
}
