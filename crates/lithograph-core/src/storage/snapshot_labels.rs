//! Streaming label membership resolution.

use std::collections::btree_map;
use std::iter::Peekable;

use super::layer::DeltaOp;
use super::snapshot::Snapshot;
use super::{LabelId, NodeId, StorageResult};

type LabelKey = (NodeId, LabelId);
type LabelOverlayIter<'a> = Peekable<btree_map::Iter<'a, LabelKey, DeltaOp>>;

impl Snapshot<'_> {
    /// Streams visible Node/Label membership pairs in key order.
    pub fn visit_labels(
        &self,
        mut visitor: impl FnMut(NodeId, LabelId) -> StorageResult<()>,
    ) -> StorageResult<()> {
        let Some(checkpoint) = self.checkpoint else {
            return drain_labels(self.overlay.labels.iter(), &mut visitor);
        };
        let mut statement = self.connection.prepare(
            "SELECT node_id, label_id FROM main._lithograph_cp_labels WHERE commit_id = ?1 ORDER BY node_id, label_id",
        )?;
        let mut rows = statement.query([checkpoint.as_bytes().as_slice()])?;
        let mut overlay = self.overlay.labels.iter().peekable();
        while let Some(row) = rows.next()? {
            merge_label_before((row.get(0)?, row.get(1)?), &mut overlay, &mut visitor)?;
        }
        drain_labels(overlay, &mut visitor)
    }
}

fn merge_label_before(
    base: LabelKey,
    overlay: &mut LabelOverlayIter<'_>,
    visitor: &mut impl FnMut(NodeId, LabelId) -> StorageResult<()>,
) -> StorageResult<()> {
    while let Some((key, op)) = overlay.peek() {
        let key = **key;
        if key > base {
            break;
        }
        let op = **op;
        overlay.next();
        if op == DeltaOp::Add {
            visitor(key.0, key.1)?;
        }
        if key == base {
            return Ok(());
        }
    }
    visitor(base.0, base.1)
}

fn drain_labels<'a>(
    overlay: impl IntoIterator<Item = (&'a LabelKey, &'a DeltaOp)>,
    visitor: &mut impl FnMut(NodeId, LabelId) -> StorageResult<()>,
) -> StorageResult<()> {
    for (key, op) in overlay {
        if *op == DeltaOp::Add {
            visitor(key.0, key.1)?;
        }
    }
    Ok(())
}
