//! Planner statistics over one resolved Snapshot.

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, params};

use super::checkpoint::{SnapshotStatistics, load_checkpoint_statistics};
use super::layer::DeltaOp;
use super::snapshot::Snapshot;
use super::{LabelId, NodeId, RelationshipId, RelationshipTypeId, StorageError, StorageResult};

impl<'connection> Snapshot<'connection> {
    /// Constant-time work admission for the caller's shared planning budget.
    /// No SQL, allocation, or overlay traversal is performed here.
    pub(crate) fn bounded_statistics_work(
        &self,
        labels_len: usize,
        types_len: usize,
        need_relationships: bool,
    ) -> usize {
        self.state
            .overlay
            .nodes
            .len()
            .saturating_add(4)
            .saturating_add(labels_len)
            .saturating_add(types_len)
            .saturating_add(self.state.overlay.labels.len().saturating_mul(labels_len))
            .saturating_add(if need_relationships || types_len > 0 {
                self.state.overlay.relationships.len()
            } else {
                0
            })
    }

    /// Inspect one indexed BLOB length without fetching or decoding its content.
    /// Callers charge this probe, then charge one work unit per encoded byte.
    pub(crate) fn bounded_statistics_decode_work(&self) -> StorageResult<Option<usize>> {
        self.read_checkpoint()?;
        let Some(checkpoint) = self.state.checkpoint else {
            return Ok(Some(0));
        };
        let bytes: Option<i64> = self.connection.query_row(
            "SELECT CASE WHEN typeof(metadata)='blob' THEN length(metadata) ELSE NULL END FROM main._lithograph_checkpoints WHERE commit_id = ?1",
            [checkpoint.as_bytes().as_slice()],
            |row| row.get(0),
        ).optional()?.flatten();
        self.read_checkpoint()?;
        Ok(bytes.and_then(|bytes| usize::try_from(bytes).ok()))
    }

    /// Optional estimates must not replay a large overlay or decode unbounded
    /// derived metadata. Unknown changes plan quality, never graph membership.
    pub(crate) fn bounded_statistics(
        &self,
        labels: &BTreeSet<LabelId>,
        relationship_types: &BTreeSet<RelationshipTypeId>,
        need_relationships: bool,
        max_work: usize,
        max_bytes: usize,
    ) -> StorageResult<Option<SnapshotStatistics>> {
        self.read_checkpoint()?;
        let work = self.bounded_statistics_work(
            labels.len(),
            relationship_types.len(),
            need_relationships,
        );
        if work > max_work {
            return Ok(None);
        }
        let requested_bytes = labels
            .len()
            .saturating_add(relationship_types.len())
            .saturating_mul(128);
        let Some(metadata_bytes) = max_bytes.checked_sub(requested_bytes) else {
            return Ok(None);
        };
        let Some(decode_work) = self.bounded_statistics_decode_work()? else {
            return Ok(None);
        };
        // JSON Values and BTreeMap entries coexist during decoding. Bound
        // their conservative working capacity, not just encoded bytes, and
        // charge decoding to the same global work grant as overlay reads.
        if decode_work > metadata_bytes / 32
            || work
                .checked_add(decode_work)
                .is_none_or(|work| work > max_work)
        {
            return Ok(None);
        }
        let statistics = self.statistics(labels, relationship_types, true, need_relationships)?;
        self.read_checkpoint()?;
        Ok(statistics)
    }

    pub(crate) fn statistics(
        &self,
        labels: &BTreeSet<LabelId>,
        relationship_types: &BTreeSet<RelationshipTypeId>,
        need_nodes: bool,
        need_relationships: bool,
    ) -> StorageResult<Option<SnapshotStatistics>> {
        let mut statistics = match self.state.checkpoint {
            Some(checkpoint) => {
                let Some(statistics) = load_checkpoint_statistics(self.connection, checkpoint)?
                else {
                    return Ok(None);
                };
                statistics
            }
            None => SnapshotStatistics::default(),
        };
        let checkpoint_max_node = self.checkpoint_max_node_id()?;
        if need_nodes {
            self.adjust_node_count(&mut statistics, checkpoint_max_node)?;
        }
        self.adjust_label_counts(&mut statistics, labels, checkpoint_max_node)?;
        if need_relationships || !relationship_types.is_empty() {
            self.adjust_relationship_counts(
                &mut statistics,
                relationship_types,
                need_relationships,
            )?;
        }
        Ok(Some(statistics))
    }

    fn adjust_node_count(
        &self,
        statistics: &mut SnapshotStatistics,
        checkpoint_max_node: NodeId,
    ) -> StorageResult<()> {
        for (&node_id, &op) in &self.state.overlay.nodes {
            self.read_checkpoint()?;
            let before = if node_id > checkpoint_max_node {
                false
            } else {
                self.checkpoint_node_exists(node_id)?
            };
            adjust_count(&mut statistics.node_count, before, op == DeltaOp::Add);
        }
        Ok(())
    }

    fn adjust_label_counts(
        &self,
        statistics: &mut SnapshotStatistics,
        labels: &BTreeSet<LabelId>,
        checkpoint_max_node: NodeId,
    ) -> StorageResult<()> {
        for &label_id in labels {
            self.read_checkpoint()?;
            let count = statistics.label_counts.entry(label_id).or_default();
            for (&(_, node_id), &op) in self
                .state
                .overlay
                .labels_by_label
                .range((label_id, 0)..=(label_id, i64::MAX))
            {
                self.read_checkpoint()?;
                let before = if node_id > checkpoint_max_node {
                    false
                } else {
                    self.checkpoint_label_exists(node_id, label_id)?
                };
                adjust_count(count, before, op == DeltaOp::Add);
            }
        }
        Ok(())
    }

    fn adjust_relationship_counts(
        &self,
        statistics: &mut SnapshotStatistics,
        relationship_types: &BTreeSet<RelationshipTypeId>,
        need_relationships: bool,
    ) -> StorageResult<()> {
        let checkpoint_max_relationship = self.checkpoint_max_relationship_id()?;
        for (&relationship_id, delta) in &self.state.overlay.relationships {
            self.read_checkpoint()?;
            let before_type = if relationship_id > checkpoint_max_relationship {
                None
            } else {
                self.checkpoint_relationship_type(relationship_id)?
            };
            let after_type = (delta.op == DeltaOp::Add).then_some(delta.record.type_id);
            if need_relationships {
                adjust_count(
                    &mut statistics.relationship_count,
                    before_type.is_some(),
                    after_type.is_some(),
                );
            }
            if before_type != after_type {
                adjust_type_count(
                    &mut statistics.type_counts,
                    relationship_types,
                    before_type,
                    false,
                );
                adjust_type_count(
                    &mut statistics.type_counts,
                    relationship_types,
                    after_type,
                    true,
                );
            }
        }
        Ok(())
    }

    fn checkpoint_max_node_id(&self) -> StorageResult<NodeId> {
        let Some(checkpoint) = self.state.checkpoint else {
            return Ok(0);
        };
        self.connection
            .query_row(
                "SELECT coalesce(max(node_id), 0) FROM main._lithograph_cp_nodes WHERE commit_id = ?1",
                [checkpoint.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(StorageError::from)
    }

    fn checkpoint_max_relationship_id(&self) -> StorageResult<RelationshipId> {
        let Some(checkpoint) = self.state.checkpoint else {
            return Ok(0);
        };
        self.connection
            .query_row(
                "SELECT coalesce(max(relationship_id), 0) FROM main._lithograph_cp_relationships WHERE commit_id = ?1",
                [checkpoint.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(StorageError::from)
    }

    fn checkpoint_node_exists(&self, node_id: NodeId) -> StorageResult<bool> {
        let Some(checkpoint) = self.state.checkpoint else {
            return Ok(false);
        };
        let exists: i64 = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_cp_nodes WHERE commit_id = ?1 AND node_id = ?2)",
            params![checkpoint.as_bytes().as_slice(), node_id],
            |row| row.get(0),
        )?;
        Ok(exists == 1)
    }

    fn checkpoint_label_exists(&self, node_id: NodeId, label_id: LabelId) -> StorageResult<bool> {
        let Some(checkpoint) = self.state.checkpoint else {
            return Ok(false);
        };
        let exists: i64 = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_cp_labels WHERE commit_id = ?1 AND node_id = ?2 AND label_id = ?3)",
            params![checkpoint.as_bytes().as_slice(), node_id, label_id],
            |row| row.get(0),
        )?;
        Ok(exists == 1)
    }

    fn checkpoint_relationship_type(
        &self,
        relationship_id: RelationshipId,
    ) -> StorageResult<Option<RelationshipTypeId>> {
        let Some(checkpoint) = self.state.checkpoint else {
            return Ok(None);
        };
        self.connection
            .query_row(
                "SELECT type_id FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND relationship_id = ?2",
                params![checkpoint.as_bytes().as_slice(), relationship_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }
}

#[path = "snapshot_stats/sampling.rs"]
mod sampling;

fn adjust_count(count: &mut u64, before: bool, after: bool) {
    match (before, after) {
        (false, true) => *count = count.saturating_add(1),
        (true, false) => *count = count.saturating_sub(1),
        _ => {}
    }
}

fn adjust_type_count(
    counts: &mut std::collections::BTreeMap<RelationshipTypeId, u64>,
    requested: &BTreeSet<RelationshipTypeId>,
    type_id: Option<RelationshipTypeId>,
    add: bool,
) {
    let Some(type_id) = type_id.filter(|type_id| requested.contains(type_id)) else {
        return;
    };
    let count = counts.entry(type_id).or_default();
    if add {
        *count = count.saturating_add(1);
    } else {
        *count = count.saturating_sub(1);
    }
}

#[cfg(test)]
#[path = "snapshot_stats/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "snapshot_stats/sampling_tests.rs"]
mod sampling_tests;
