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
        visitor: impl FnMut(NodeId, LabelId) -> StorageResult<()>,
    ) -> StorageResult<()> {
        self.visit_labels_checked(&|| Ok(()), visitor)
    }

    pub(crate) fn visit_labels_checked(
        &self,
        check: &dyn Fn() -> StorageResult<()>,
        mut visitor: impl FnMut(NodeId, LabelId) -> StorageResult<()>,
    ) -> StorageResult<()> {
        check()?;
        let Some(checkpoint) = self.state.checkpoint else {
            return drain_labels(self.state.overlay.labels.iter(), check, &mut visitor);
        };
        let mut statement = self.connection.prepare(
            "SELECT node_id, label_id FROM main._lithograph_cp_labels WHERE commit_id = ?1 ORDER BY node_id, label_id",
        )?;
        let mut rows = statement.query([checkpoint.as_bytes().as_slice()])?;
        let mut overlay = self.state.overlay.labels.iter().peekable();
        while let Some(row) = rows.next()? {
            check()?;
            merge_label_before(
                (row.get(0)?, row.get(1)?),
                &mut overlay,
                check,
                &mut visitor,
            )?;
        }
        drain_labels(overlay, check, &mut visitor)
    }
}

fn merge_label_before(
    base: LabelKey,
    overlay: &mut LabelOverlayIter<'_>,
    check: &dyn Fn() -> StorageResult<()>,
    visitor: &mut impl FnMut(NodeId, LabelId) -> StorageResult<()>,
) -> StorageResult<()> {
    while let Some((key, op)) = overlay.peek() {
        check()?;
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
    check: &dyn Fn() -> StorageResult<()>,
    visitor: &mut impl FnMut(NodeId, LabelId) -> StorageResult<()>,
) -> StorageResult<()> {
    for (key, op) in overlay {
        check()?;
        if *op == DeltaOp::Add {
            visitor(key.0, key.1)?;
        }
    }
    Ok(())
}
