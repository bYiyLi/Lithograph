use super::*;
use crate::storage::{
    CommitMetadata, LayerBuilder, allocate_node_id, allocate_relationship_id, branch_head,
    commit_layer, create_storage_schema, initialize_root, intern_relationship_type,
};
use rusqlite::Connection;

pub(super) struct Fixture {
    pub db: Connection,
    pub head: HashId,
    pub nodes: [NodeId; 5],
    pub types: [RelationshipTypeId; 2],
    pub edges: Vec<RelationshipRecord>,
}

impl Fixture {
    pub fn new(checkpoint: bool) -> Self {
        let db = Connection::open_in_memory().expect("SQLite");
        db.execute_batch(
            "CREATE TABLE main._lithograph_meta(
                id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL,
                database_id TEXT NOT NULL, storage_format INTEGER NOT NULL
            )",
        )
        .expect("metadata");
        create_storage_schema(&db).expect("schema");
        initialize_root(&db).expect("root");
        let nodes = std::array::from_fn(|_| allocate_node_id(&db).expect("NodeId"));
        let types = [
            intern_relationship_type(&db, "EDGE").expect("type"),
            intern_relationship_type(&db, "OTHER").expect("type"),
        ];
        let mut layer = LayerBuilder::default();
        for node in nodes {
            layer.add_node(node).expect("Node");
        }
        // Relationship identities intentionally differ from neighbor order.
        let mut edges = Vec::new();
        for (source, target, type_id) in [
            (nodes[0], nodes[3], types[0]),
            (nodes[0], nodes[1], types[0]),
            (nodes[0], nodes[3], types[0]),
            (nodes[0], nodes[0], types[0]),
            (nodes[0], nodes[2], types[1]),
            (nodes[4], nodes[0], types[0]),
            (nodes[1], nodes[0], types[0]),
            (nodes[4], nodes[0], types[0]),
        ] {
            let record = RelationshipRecord {
                id: allocate_relationship_id(&db).expect("RelationshipId"),
                source,
                type_id,
                target,
            };
            layer.add_relationship(record).expect("Relationship");
            edges.push(record);
        }
        let parent = branch_head(&db, "main").expect("head");
        let head = commit_layer(&db, "main", parent, None, &layer, &metadata(1)).expect("commit");
        if checkpoint {
            crate::storage::create_checkpoint(&db, head).expect("checkpoint");
        }
        Self {
            db,
            head,
            nodes,
            types,
            edges,
        }
    }

    pub fn edge(&self, source: NodeId, target: NodeId) -> RelationshipRecord {
        RelationshipRecord {
            id: allocate_relationship_id(&self.db).expect("RelationshipId"),
            source,
            type_id: self.types[0],
            target,
        }
    }

    pub fn commit(&self, layer: &LayerBuilder) -> HashId {
        commit_layer(
            &self.db,
            "main",
            branch_head(&self.db, "main").expect("head"),
            None,
            layer,
            &metadata(2),
        )
        .expect("commit")
    }

    pub fn expected(
        &self,
        kind: AdjacencyKind,
        minimum: NodeId,
        records: &[RelationshipRecord],
    ) -> Vec<RelationshipRecord> {
        let mut expected: Vec<_> = records
            .iter()
            .copied()
            .filter(|record| adjacency_matches(kind, self.nodes[0], Some(self.types[0]), *record))
            .filter(|record| adjacency_key(kind, *record).neighbor_id >= minimum)
            .collect();
        expected.sort_by_key(|record| adjacency_key(kind, *record));
        expected
    }
}

fn metadata(at: i64) -> CommitMetadata {
    CommitMetadata {
        author: None,
        message: None,
        committed_at: at,
    }
}
