//! Bounded, resumable scans over a checkpoint plus Layer overlay.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use rusqlite::{params, params_from_iter, types::Value as SqlValue};

use super::layer::{DeltaOp, RelationshipRecord};
use super::snapshot::Snapshot;
use super::{HashId, LabelId, NodeId, RelationshipId, RelationshipTypeId, ScanPage, StorageResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdjacencyKind {
    Outgoing,
    Incoming,
    Incident,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AdjacencyKey {
    type_id: RelationshipTypeId,
    neighbor_id: NodeId,
    relationship_id: RelationshipId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncidentPhase {
    Outgoing,
    Incoming,
}

/// Opaque, snapshot-bound continuation for one adjacency scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdjacencyCursor {
    cache_identity: super::HashId,
    kind: AdjacencyKind,
    node_id: NodeId,
    type_id: Option<RelationshipTypeId>,
    phase: IncidentPhase,
    after: Option<AdjacencyKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdjacencyScanPage {
    pub(crate) items: Vec<RelationshipRecord>,
    pub(crate) next_cursor: Option<AdjacencyCursor>,
}

struct AdjacencyMergeSources {
    base: Vec<RelationshipRecord>,
    overlay: Vec<(AdjacencyKey, RelationshipRecord, DeltaOp)>,
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

    /// Reads at most `limit` visible Relationships of `type_id` after `after`.
    pub fn scan_relationship_type_after(
        &self,
        type_id: RelationshipTypeId,
        after: RelationshipId,
        limit: usize,
    ) -> StorageResult<ScanPage<RelationshipRecord>> {
        let limit = normalized_limit(limit);
        let base = self.base_relationship_type_page(type_id, after, limit)?;
        let overlay = self
            .overlay
            .relationships
            .range(after.saturating_add(1)..)
            .filter(|(_, delta)| delta.record.type_id == type_id)
            .take(limit)
            .map(|(id, _)| *id)
            .collect();
        Ok(
            self.merge_relationship_page(after, limit, base, overlay, |record| {
                record.type_id == type_id
            }),
        )
    }

    pub(crate) fn scan_outgoing_page(
        &self,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        cursor: Option<AdjacencyCursor>,
        limit: usize,
    ) -> StorageResult<AdjacencyScanPage> {
        self.scan_directed_adjacency_page(AdjacencyKind::Outgoing, node_id, type_id, cursor, limit)
    }

    pub(crate) fn scan_incoming_page(
        &self,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        cursor: Option<AdjacencyCursor>,
        limit: usize,
    ) -> StorageResult<AdjacencyScanPage> {
        self.scan_directed_adjacency_page(AdjacencyKind::Incoming, node_id, type_id, cursor, limit)
    }

    pub(crate) fn scan_incident_page(
        &self,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        cursor: Option<AdjacencyCursor>,
        limit: usize,
    ) -> StorageResult<AdjacencyScanPage> {
        self.validate_adjacency_cursor(AdjacencyKind::Incident, node_id, type_id, cursor)?;
        let limit = normalized_limit(limit);
        let phase = cursor.map_or(IncidentPhase::Outgoing, |value| value.phase);
        let after = cursor.and_then(|value| value.after);
        let directed_kind = match phase {
            IncidentPhase::Outgoing => AdjacencyKind::Outgoing,
            IncidentPhase::Incoming => AdjacencyKind::Incoming,
        };
        let directed_cursor = after.map(|after| AdjacencyCursor {
            cache_identity: self.cache_identity,
            kind: directed_kind,
            node_id,
            type_id,
            phase,
            after: Some(after),
        });
        let first = self.scan_directed_adjacency_page(
            directed_kind,
            node_id,
            type_id,
            directed_cursor,
            limit,
        )?;
        if first.next_cursor.is_some() {
            return Ok(AdjacencyScanPage {
                items: filter_incident_self_loops(node_id, phase, first.items),
                next_cursor: first.next_cursor.map(|value| AdjacencyCursor {
                    cache_identity: value.cache_identity,
                    kind: AdjacencyKind::Incident,
                    node_id,
                    type_id,
                    phase,
                    after: value.after,
                }),
            });
        }
        let mut items = filter_incident_self_loops(node_id, phase, first.items);
        if phase == IncidentPhase::Incoming || items.len() >= limit {
            return Ok(AdjacencyScanPage {
                items,
                next_cursor: None,
            });
        }
        let remaining = limit - items.len();
        let incoming = self.scan_directed_adjacency_page(
            AdjacencyKind::Incoming,
            node_id,
            type_id,
            None,
            remaining,
        )?;
        items.extend(filter_incident_self_loops(
            node_id,
            IncidentPhase::Incoming,
            incoming.items,
        ));
        let next_cursor = incoming.next_cursor.map(|value| AdjacencyCursor {
            cache_identity: value.cache_identity,
            kind: AdjacencyKind::Incident,
            node_id,
            type_id,
            phase: IncidentPhase::Incoming,
            after: value.after,
        });
        Ok(AdjacencyScanPage { items, next_cursor })
    }

    fn overlay_relationship_ids(&self, after: RelationshipId, limit: usize) -> Vec<RelationshipId> {
        self.overlay
            .relationships
            .range(after.saturating_add(1)..)
            .take(limit)
            .map(|(id, _)| *id)
            .collect()
    }

    fn scan_directed_adjacency_page(
        &self,
        kind: AdjacencyKind,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        cursor: Option<AdjacencyCursor>,
        limit: usize,
    ) -> StorageResult<AdjacencyScanPage> {
        debug_assert!(kind != AdjacencyKind::Incident);
        self.validate_adjacency_cursor(kind, node_id, type_id, cursor)?;
        let limit = normalized_limit(limit);
        let after = cursor.and_then(|value| value.after);
        let base = self.base_adjacency_keyset_page(kind, node_id, type_id, after, limit)?;
        let overlay = self.overlay_adjacency_keyset_page(kind, node_id, type_id, after, limit);
        Ok(self.merge_adjacency_page(
            kind,
            node_id,
            type_id,
            after,
            limit,
            AdjacencyMergeSources { base, overlay },
        ))
    }

    fn validate_adjacency_cursor(
        &self,
        kind: AdjacencyKind,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        cursor: Option<AdjacencyCursor>,
    ) -> StorageResult<()> {
        if let Some(cursor) = cursor
            && (cursor.cache_identity != self.cache_identity
                || cursor.kind != kind
                || cursor.node_id != node_id
                || cursor.type_id != type_id)
        {
            return Err(super::StorageError::corrupt(
                "adjacency continuation does not belong to this snapshot/domain",
            ));
        }
        Ok(())
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

    fn merge_adjacency_page(
        &self,
        kind: AdjacencyKind,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        after: Option<AdjacencyKey>,
        limit: usize,
        sources: AdjacencyMergeSources,
    ) -> AdjacencyScanPage {
        let AdjacencyMergeSources { base, overlay } = sources;
        let base_last = base.last().map(|record| adjacency_key(kind, *record));
        let overlay_last = overlay.last().map(|(key, _, _)| *key);
        let horizon = scan_key_horizon(
            base_last,
            base.len() == limit,
            overlay_last,
            overlay.len() == limit,
        );
        let mut candidates = base
            .into_iter()
            .map(|record| (adjacency_key(kind, record), Some(record)))
            .collect::<BTreeMap<_, _>>();
        for (key, record, op) in overlay {
            candidates.insert(key, (op == DeltaOp::Add).then_some(record));
        }
        let mut items = Vec::with_capacity(limit);
        let mut consumed = after;
        for (key, record) in candidates {
            if horizon.is_some_and(|value| key > value) || items.len() == limit {
                break;
            }
            consumed = Some(key);
            if let Some(record) = record
                && adjacency_matches(kind, node_id, type_id, record)
            {
                items.push(record);
            }
        }
        let next_cursor = ((horizon.is_some() || items.len() == limit) && consumed != after)
            .then_some(AdjacencyCursor {
                cache_identity: self.cache_identity,
                kind,
                node_id,
                type_id,
                phase: match kind {
                    AdjacencyKind::Incoming => IncidentPhase::Incoming,
                    AdjacencyKind::Outgoing | AdjacencyKind::Incident => IncidentPhase::Outgoing,
                },
                after: consumed,
            });
        AdjacencyScanPage { next_cursor, items }
    }

    fn overlay_adjacency_keyset_page(
        &self,
        kind: AdjacencyKind,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        after: Option<AdjacencyKey>,
        limit: usize,
    ) -> Vec<(AdjacencyKey, RelationshipRecord, DeltaOp)> {
        let map = match kind {
            AdjacencyKind::Outgoing => &self.overlay.outgoing,
            AdjacencyKind::Incoming => &self.overlay.incoming,
            AdjacencyKind::Incident => unreachable!("incident scans use two directed streams"),
        };
        let lower = match after {
            Some(key) => {
                Bound::Excluded((node_id, key.type_id, key.neighbor_id, key.relationship_id))
            }
            None => Bound::Included((node_id, type_id.unwrap_or(i64::MIN), i64::MIN, i64::MIN)),
        };
        let upper = type_id.map_or(
            Bound::Included((node_id, i64::MAX, i64::MAX, i64::MAX)),
            |type_id| Bound::Included((node_id, type_id, i64::MAX, i64::MAX)),
        );
        map.range((lower, upper))
            .filter(|((_, candidate_type, _, _), _)| {
                type_id.is_none_or(|value| *candidate_type == value)
            })
            .take(limit)
            .map(|((_, type_id, neighbor_id, relationship_id), delta)| {
                (
                    AdjacencyKey {
                        type_id: *type_id,
                        neighbor_id: *neighbor_id,
                        relationship_id: *relationship_id,
                    },
                    delta.record,
                    delta.op,
                )
            })
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

    fn base_adjacency_keyset_page(
        &self,
        kind: AdjacencyKind,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        after: Option<AdjacencyKey>,
        limit: usize,
    ) -> StorageResult<Vec<RelationshipRecord>> {
        let Some(checkpoint) = self.checkpoint else {
            return Ok(Vec::new());
        };
        #[cfg(feature = "test-support")]
        crate::performance::record_adjacency_page();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.base_directed_adjacency_page(checkpoint, kind, node_id, type_id, after, limit)
    }

    fn base_directed_adjacency_page(
        &self,
        checkpoint: HashId,
        kind: AdjacencyKind,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        after: Option<AdjacencyKey>,
        limit: i64,
    ) -> StorageResult<Vec<RelationshipRecord>> {
        let (index, endpoint, neighbor) = match kind {
            AdjacencyKind::Outgoing => ("_lithograph_cp_rel_out", "source_id", "target_id"),
            AdjacencyKind::Incoming => ("_lithograph_cp_rel_in", "target_id", "source_id"),
            AdjacencyKind::Incident => unreachable!("incident scans use two directed streams"),
        };
        let mut sql = format!(
            "SELECT relationship_id, source_id, type_id, target_id \
             FROM main._lithograph_cp_relationships INDEXED BY {index} \
             WHERE commit_id = ? AND {endpoint} = ?"
        );
        let mut parameters = vec![
            SqlValue::Blob(checkpoint.as_bytes().to_vec()),
            SqlValue::Integer(node_id),
        ];
        if let Some(type_id) = type_id {
            sql.push_str(" AND type_id = ?");
            parameters.push(SqlValue::Integer(type_id));
        }
        if let Some(after) = after {
            if type_id.is_some() {
                sql.push_str(&format!(" AND ({neighbor}, relationship_id) > (?, ?)"));
            } else {
                sql.push_str(&format!(
                    " AND (type_id, {neighbor}, relationship_id) > (?, ?, ?)"
                ));
                parameters.push(SqlValue::Integer(after.type_id));
            }
            parameters.push(SqlValue::Integer(after.neighbor_id));
            parameters.push(SqlValue::Integer(after.relationship_id));
        }
        if type_id.is_some() {
            sql.push_str(&format!(" ORDER BY {neighbor}, relationship_id LIMIT ?"));
        } else {
            sql.push_str(&format!(
                " ORDER BY type_id, {neighbor}, relationship_id LIMIT ?"
            ));
        }
        parameters.push(SqlValue::Integer(limit));
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(parameters), relationship_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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

    fn base_relationship_type_page(
        &self,
        type_id: RelationshipTypeId,
        after: RelationshipId,
        limit: usize,
    ) -> StorageResult<Vec<RelationshipRecord>> {
        let Some(checkpoint) = self.checkpoint else {
            return Ok(Vec::new());
        };
        let mut statement = self.connection.prepare(
            "SELECT relationship_id, source_id, type_id, target_id FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND type_id = ?2 AND relationship_id > ?3 ORDER BY relationship_id LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                checkpoint.as_bytes().as_slice(),
                type_id,
                after,
                limit as i64
            ],
            relationship_from_row,
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

fn scan_key_horizon(
    base_last: Option<AdjacencyKey>,
    base_full: bool,
    overlay_last: Option<AdjacencyKey>,
    overlay_full: bool,
) -> Option<AdjacencyKey> {
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

fn adjacency_key(kind: AdjacencyKind, record: RelationshipRecord) -> AdjacencyKey {
    let neighbor_id = match kind {
        AdjacencyKind::Outgoing => record.target,
        AdjacencyKind::Incoming => record.source,
        AdjacencyKind::Incident => unreachable!("incident scans use directed adjacency keys"),
    };
    AdjacencyKey {
        type_id: record.type_id,
        neighbor_id,
        relationship_id: record.id,
    }
}

fn filter_incident_self_loops(
    node_id: NodeId,
    phase: IncidentPhase,
    records: Vec<RelationshipRecord>,
) -> Vec<RelationshipRecord> {
    if phase == IncidentPhase::Outgoing {
        return records;
    }
    records
        .into_iter()
        .filter(|record| !(record.source == node_id && record.target == node_id))
        .collect()
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
