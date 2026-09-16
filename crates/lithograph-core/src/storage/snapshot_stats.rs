//! Planner statistics over one resolved Snapshot.

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, params};

use super::checkpoint::{SnapshotStatistics, load_checkpoint_statistics};
use super::layer::DeltaOp;
use super::snapshot::Snapshot;
use super::{LabelId, NodeId, RelationshipId, RelationshipTypeId, StorageError, StorageResult};

impl<'connection> Snapshot<'connection> {
    pub(crate) fn statistics(
        &self,
        labels: &BTreeSet<LabelId>,
        relationship_types: &BTreeSet<RelationshipTypeId>,
        need_nodes: bool,
        need_relationships: bool,
    ) -> StorageResult<Option<SnapshotStatistics>> {
        let mut statistics = match self.checkpoint {
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
        for (&node_id, &op) in &self.overlay.nodes {
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
            let count = statistics.label_counts.entry(label_id).or_default();
            for (&(_, node_id), &op) in self
                .overlay
                .labels_by_label
                .range((label_id, 0)..=(label_id, i64::MAX))
            {
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
        for (&relationship_id, delta) in &self.overlay.relationships {
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
        let Some(checkpoint) = self.checkpoint else {
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
        let Some(checkpoint) = self.checkpoint else {
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
        let Some(checkpoint) = self.checkpoint else {
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
        let Some(checkpoint) = self.checkpoint else {
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
        let Some(checkpoint) = self.checkpoint else {
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
