//! Bounded, resumable scans over a checkpoint plus Layer overlay.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::params;

use super::layer::{DeltaOp, RelationshipRecord};
use super::snapshot::Snapshot;
use super::{LabelId, NodeId, RelationshipId, RelationshipTypeId, ScanPage, StorageResult};

#[derive(Clone, Copy)]
enum AdjacencyKind {
    Outgoing,
    Incoming,
    Incident,
}

impl Snapshot<'_> {
    /// Reads at most `limit` visible Node ids after `after` in database-id order.
    pub fn scan_nodes_after(&self, after: NodeId, limit: usize) -> StorageResult<ScanPage<NodeId>> {
        self.scan_node_domain_after(after, limit, None)
    }

    /// Reads at most `limit` visible Node ids with `label_id` after `after`.
    pub fn scan_label_after(
        &self,
        label_id: LabelId,
        after: NodeId,
        limit: usize,
    ) -> StorageResult<ScanPage<NodeId>> {
        self.scan_node_domain_after(after, limit, Some(label_id))
    }

    /// Reads at most `limit` visible Relationships after `after` in identity order.
    pub fn scan_relationships_after(
        &self,
        after: RelationshipId,
        limit: usize,
    ) -> StorageResult<ScanPage<RelationshipRecord>> {
        let limit = normalized_limit(limit);
        let base = self.base_relationship_page(after, limit)?;
        let overlay = self.overlay_relationship_ids(after, limit);
        Ok(self.merge_relationship_page(after, limit, base, overlay, |_| true))
    }

    pub fn scan_outgoing_after(
        &self,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        after: RelationshipId,
        limit: usize,
    ) -> StorageResult<ScanPage<RelationshipRecord>> {
        self.scan_adjacency_after(AdjacencyKind::Outgoing, node_id, type_id, after, limit)
    }

    pub fn scan_incoming_after(
        &self,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        after: RelationshipId,
        limit: usize,
    ) -> StorageResult<ScanPage<RelationshipRecord>> {
        self.scan_adjacency_after(AdjacencyKind::Incoming, node_id, type_id, after, limit)
    }

    pub fn scan_incident_after(
        &self,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        after: RelationshipId,
        limit: usize,
    ) -> StorageResult<ScanPage<RelationshipRecord>> {
        self.scan_adjacency_after(AdjacencyKind::Incident, node_id, type_id, after, limit)
    }

    fn overlay_relationship_ids(&self, after: RelationshipId, limit: usize) -> Vec<RelationshipId> {
        self.overlay
            .relationships
            .range(after.saturating_add(1)..)
            .take(limit)
            .map(|(id, _)| *id)
            .collect()
    }

    fn scan_adjacency_after(
        &self,
        kind: AdjacencyKind,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        after: RelationshipId,
        limit: usize,
    ) -> StorageResult<ScanPage<RelationshipRecord>> {
        let limit = normalized_limit(limit);
        let base = self.base_adjacency_page(kind, node_id, type_id, after, limit)?;
        let overlay = self.overlay_adjacency_ids(kind, node_id, type_id, after, limit);
        Ok(
            self.merge_relationship_page(after, limit, base, overlay, |record| {
                adjacency_matches(kind, node_id, type_id, record)
            }),
        )
    }

    fn merge_relationship_page(
        &self,
        after: RelationshipId,
        limit: usize,
        base: Vec<RelationshipRecord>,
        overlay: Vec<RelationshipId>,
        accept_overlay: impl Fn(RelationshipRecord) -> bool,
    ) -> ScanPage<RelationshipRecord> {
        let horizon = scan_horizon(
            base.last().map(|record| record.id),
            base.len() == limit,
            overlay.last().copied(),
            overlay.len() == limit,
        );
        let base_by_id = base
            .into_iter()
            .map(|record| (record.id, record))
            .collect::<BTreeMap<_, _>>();
        let mut candidates = BTreeSet::new();
        candidates.extend(base_by_id.keys().copied());
        candidates.extend(overlay);
        let mut items = Vec::with_capacity(limit);
        let mut consumed = after;
        for id in candidates {
            if horizon.is_some_and(|value| id > value) || items.len() == limit {
                break;
            }
            consumed = id;
            if let Some(delta) = self.overlay.relationships.get(&id) {
                if delta.op == DeltaOp::Add && accept_overlay(delta.record) {
                    items.push(delta.record);
                }
            } else if let Some(record) = base_by_id.get(&id) {
                items.push(*record);
            }
        }
        ScanPage {
            next_after: ((horizon.is_some() || items.len() == limit) && consumed > after)
                .then_some(consumed),
            items,
        }
    }

    fn overlay_adjacency_ids(
        &self,
        kind: AdjacencyKind,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        after: RelationshipId,
        limit: usize,
    ) -> Vec<RelationshipId> {
        let map = match kind {
            AdjacencyKind::Outgoing => &self.overlay.outgoing_by_id,
            AdjacencyKind::Incoming => &self.overlay.incoming_by_id,
            AdjacencyKind::Incident => &self.overlay.incident_by_id,
        };
        map.range((node_id, after.saturating_add(1))..=(node_id, i64::MAX))
            .filter(|(_, delta)| type_id.is_none_or(|value| delta.record.type_id == value))
            .take(limit)
            .map(|((_, relationship_id), _)| *relationship_id)
            .collect()
    }

    fn scan_node_domain_after(
        &self,
        after: NodeId,
        limit: usize,
        label_id: Option<LabelId>,
    ) -> StorageResult<ScanPage<NodeId>> {
        let limit = normalized_limit(limit);
        let base = self.base_node_page(after, limit, label_id)?;
        let overlay = self.overlay_node_page(after, limit, label_id);
        let horizon = scan_horizon(
            base.last().copied(),
            base.len() == limit,
            overlay.last().copied(),
            overlay.len() == limit,
        );
        let mut candidates = BTreeSet::new();
        candidates.extend(base.iter().copied());
        candidates.extend(overlay);
        let mut items = Vec::with_capacity(limit);
        let mut consumed = after;
        for node_id in candidates {
            if horizon.is_some_and(|value| node_id > value) || items.len() == limit {
                break;
            }
            consumed = node_id;
            if self.node_domain_contains(node_id, label_id, &base) {
                items.push(node_id);
            }
        }
        Ok(ScanPage {
            next_after: ((horizon.is_some() || items.len() == limit) && consumed > after)
                .then_some(consumed),
            items,
        })
    }

    fn node_domain_contains(
        &self,
        node_id: NodeId,
        label_id: Option<LabelId>,
        base: &[NodeId],
    ) -> bool {
        if let Some(label_id) = label_id {
            self.overlay
                .labels_by_label
                .get(&(label_id, node_id))
                .map_or_else(
                    || base.binary_search(&node_id).is_ok(),
                    |op| *op == DeltaOp::Add,
                )
        } else {
            self.overlay.nodes.get(&node_id).map_or_else(
                || base.binary_search(&node_id).is_ok(),
                |op| *op == DeltaOp::Add,
            )
        }
    }

    fn overlay_node_page(
        &self,
        after: NodeId,
        limit: usize,
        label_id: Option<LabelId>,
    ) -> Vec<NodeId> {
        match label_id {
            Some(label_id) => self
                .overlay
                .labels_by_label
                .range((label_id, after.saturating_add(1))..=(label_id, i64::MAX))
                .take(limit)
                .map(|((_, node_id), _)| *node_id)
                .collect(),
            None => self
                .overlay
                .nodes
                .range(after.saturating_add(1)..)
                .take(limit)
                .map(|(node_id, _)| *node_id)
                .collect(),
        }
    }

    fn base_node_page(
        &self,
        after: NodeId,
        limit: usize,
        label_id: Option<LabelId>,
    ) -> StorageResult<Vec<NodeId>> {
        let Some(checkpoint) = self.checkpoint else {
            return Ok(Vec::new());
        };
        let mut values = Vec::with_capacity(limit);
        if let Some(label_id) = label_id {
            let mut statement = self.connection.prepare(
                "SELECT node_id FROM main._lithograph_cp_labels INDEXED BY _lithograph_cp_labels_by_label WHERE commit_id = ?1 AND label_id = ?2 AND node_id > ?3 ORDER BY node_id LIMIT ?4",
            )?;
            let rows = statement.query_map(
                params![
                    checkpoint.as_bytes().as_slice(),
                    label_id,
                    after,
                    limit as i64
                ],
                |row| row.get::<_, i64>(0),
            )?;
            for row in rows {
                values.push(row?);
            }
        } else {
            let mut statement = self.connection.prepare(
                "SELECT node_id FROM main._lithograph_cp_nodes WHERE commit_id = ?1 AND node_id > ?2 ORDER BY node_id LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![checkpoint.as_bytes().as_slice(), after, limit as i64],
                |row| row.get::<_, i64>(0),
            )?;
            for row in rows {
                values.push(row?);
            }
        }
        Ok(values)
    }

    fn base_adjacency_page(
        &self,
        kind: AdjacencyKind,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        after: RelationshipId,
        limit: usize,
    ) -> StorageResult<Vec<RelationshipRecord>> {
        let Some(checkpoint) = self.checkpoint else {
            return Ok(Vec::new());
        };
        let endpoint = match kind {
            AdjacencyKind::Outgoing => "source_id = ?2",
            AdjacencyKind::Incoming => "target_id = ?2",
            AdjacencyKind::Incident => "(source_id = ?2 OR target_id = ?2)",
        };
        let mut values = Vec::with_capacity(limit);
        if let Some(type_id) = type_id {
            let sql = format!(
                "SELECT relationship_id, source_id, type_id, target_id FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND {endpoint} AND type_id = ?3 AND relationship_id > ?4 ORDER BY relationship_id LIMIT ?5"
            );
            let mut statement = self.connection.prepare(&sql)?;
            let rows = statement.query_map(
                params![
                    checkpoint.as_bytes().as_slice(),
                    node_id,
                    type_id,
                    after,
                    limit as i64
                ],
                relationship_from_row,
            )?;
            for row in rows {
                values.push(row?);
            }
        } else {
            let sql = format!(
                "SELECT relationship_id, source_id, type_id, target_id FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND {endpoint} AND relationship_id > ?3 ORDER BY relationship_id LIMIT ?4"
            );
            let mut statement = self.connection.prepare(&sql)?;
            let rows = statement.query_map(
                params![
                    checkpoint.as_bytes().as_slice(),
                    node_id,
                    after,
                    limit as i64
                ],
                relationship_from_row,
            )?;
            for row in rows {
                values.push(row?);
            }
        }
        Ok(values)
    }

    fn base_relationship_page(
        &self,
        after: RelationshipId,
        limit: usize,
    ) -> StorageResult<Vec<RelationshipRecord>> {
        let Some(checkpoint) = self.checkpoint else {
            return Ok(Vec::new());
        };
        let mut statement = self.connection.prepare(
            "SELECT relationship_id, source_id, type_id, target_id FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND relationship_id > ?2 ORDER BY relationship_id LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![checkpoint.as_bytes().as_slice(), after, limit as i64],
            |row| {
                Ok(RelationshipRecord {
                    id: row.get(0)?,
                    source: row.get(1)?,
                    type_id: row.get(2)?,
                    target: row.get(3)?,
                })
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn normalized_limit(limit: usize) -> usize {
    limit.clamp(1, 4_096)
}

fn scan_horizon(
    base_last: Option<i64>,
    base_full: bool,
    overlay_last: Option<i64>,
    overlay_full: bool,
) -> Option<i64> {
    match (
        base_full.then_some(base_last).flatten(),
        overlay_full.then_some(overlay_last).flatten(),
    ) {
        (Some(base), Some(overlay)) => Some(base.min(overlay)),
        (Some(base), None) => Some(base),
        (None, Some(overlay)) => Some(overlay),
        (None, None) => None,
    }
}

fn adjacency_matches(
    kind: AdjacencyKind,
    node_id: NodeId,
    type_id: Option<RelationshipTypeId>,
    record: RelationshipRecord,
) -> bool {
    let endpoint_matches = match kind {
        AdjacencyKind::Outgoing => record.source == node_id,
        AdjacencyKind::Incoming => record.target == node_id,
        AdjacencyKind::Incident => record.source == node_id || record.target == node_id,
    };
    endpoint_matches && type_id.is_none_or(|value| record.type_id == value)
}

fn relationship_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RelationshipRecord> {
    Ok(RelationshipRecord {
        id: row.get(0)?,
        source: row.get(1)?,
        type_id: row.get(2)?,
        target: row.get(3)?,
    })
}
