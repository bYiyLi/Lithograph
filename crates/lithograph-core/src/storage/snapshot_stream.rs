//! Streaming merge of checkpoint rows with the bounded Layer overlay.

use std::collections::btree_map;
use std::iter::Peekable;

use super::layer::{DeltaOp, RelationshipDelta};
use super::snapshot::Snapshot;
use super::{NodeId, RelationshipRecord, StorageResult};

impl Snapshot<'_> {
    /// Streams visible Node ids in database-id order.
    pub fn visit_nodes(
        &self,
        mut visitor: impl FnMut(NodeId) -> StorageResult<()>,
    ) -> StorageResult<()> {
        let Some(checkpoint) = self.state.checkpoint else {
            for (node_id, op) in &self.state.overlay.nodes {
                if *op == DeltaOp::Add {
                    visitor(*node_id)?;
                }
            }
            return Ok(());
        };
        let mut statement = self.connection.prepare(
            "SELECT node_id FROM main._lithograph_cp_nodes WHERE commit_id = ?1 ORDER BY node_id",
        )?;
        let mut rows = statement.query([checkpoint.as_bytes().as_slice()])?;
        let mut overlay = self.state.overlay.nodes.iter().peekable();
        while let Some(row) = rows.next()? {
            merge_node_before(row.get(0)?, &mut overlay, &mut visitor)?;
        }
        drain_nodes(&mut overlay, &mut visitor)
    }

    /// Streams visible Relationships in RelationshipId order.
    pub fn visit_relationships(
        &self,
        visitor: impl FnMut(RelationshipRecord) -> StorageResult<()>,
    ) -> StorageResult<()> {
        self.visit_relationships_checked(&|| Ok(()), visitor)
    }

    pub(crate) fn visit_relationships_checked(
        &self,
        check: &dyn Fn() -> StorageResult<()>,
        mut visitor: impl FnMut(RelationshipRecord) -> StorageResult<()>,
    ) -> StorageResult<()> {
        check()?;
        let Some(checkpoint) = self.state.checkpoint else {
            return drain_relationships(
                self.state.overlay.relationships.iter(),
                check,
                &mut visitor,
            );
        };
        let mut statement = self.connection.prepare(
            "SELECT relationship_id, source_id, type_id, target_id FROM main._lithograph_cp_relationships WHERE commit_id = ?1 ORDER BY relationship_id",
        )?;
        let mut rows = statement.query([checkpoint.as_bytes().as_slice()])?;
        let mut overlay = self.state.overlay.relationships.iter().peekable();
        while let Some(row) = rows.next()? {
            check()?;
            let record = RelationshipRecord {
                id: row.get(0)?,
                source: row.get(1)?,
                type_id: row.get(2)?,
                target: row.get(3)?,
            };
            merge_relationship_before(record, &mut overlay, check, &mut visitor)?;
        }
        drain_relationships(overlay, check, &mut visitor)
    }
}

type NodeOverlayIter<'a> = Peekable<btree_map::Iter<'a, NodeId, DeltaOp>>;
type RelationshipOverlayIter<'a> = Peekable<btree_map::Iter<'a, i64, RelationshipDelta>>;

fn merge_node_before(
    base: NodeId,
    overlay: &mut NodeOverlayIter<'_>,
    visitor: &mut impl FnMut(NodeId) -> StorageResult<()>,
) -> StorageResult<()> {
    while let Some((node_id, op)) = overlay.peek() {
        let node_id = **node_id;
        if node_id > base {
            break;
        }
        let op = **op;
        overlay.next();
        if op == DeltaOp::Add {
            visitor(node_id)?;
        }
        if node_id == base {
            return Ok(());
        }
    }
    visitor(base)
}

fn drain_nodes(
    overlay: &mut NodeOverlayIter<'_>,
    visitor: &mut impl FnMut(NodeId) -> StorageResult<()>,
) -> StorageResult<()> {
    for (node_id, op) in overlay {
        if *op == DeltaOp::Add {
            visitor(*node_id)?;
        }
    }
    Ok(())
}

fn merge_relationship_before(
    base: RelationshipRecord,
    overlay: &mut RelationshipOverlayIter<'_>,
    check: &dyn Fn() -> StorageResult<()>,
    visitor: &mut impl FnMut(RelationshipRecord) -> StorageResult<()>,
) -> StorageResult<()> {
    while let Some((relationship_id, delta)) = overlay.peek() {
        check()?;
        let relationship_id = **relationship_id;
        if relationship_id > base.id {
            break;
        }
        let delta = **delta;
        overlay.next();
        if delta.op == DeltaOp::Add {
            visitor(delta.record)?;
        }
        if relationship_id == base.id {
            return Ok(());
        }
    }
    visitor(base)
}

fn drain_relationships<'a>(
    overlay: impl IntoIterator<Item = (&'a i64, &'a RelationshipDelta)>,
    check: &dyn Fn() -> StorageResult<()>,
    visitor: &mut impl FnMut(RelationshipRecord) -> StorageResult<()>,
) -> StorageResult<()> {
    for (_, delta) in overlay {
        check()?;
        if delta.op == DeltaOp::Add {
            visitor(delta.record)?;
        }
    }
    Ok(())
}
