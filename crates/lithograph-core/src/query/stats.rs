use std::collections::{BTreeMap, BTreeSet};

use crate::storage::{LabelId, RelationshipTypeId, Snapshot};

use super::QueryResult;

/// Planning advice only. Even an exact zero must never suppress execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CardinalityEstimate {
    pub expected: u64,
    pub lower: u64,
    pub upper: u64,
}

impl CardinalityEstimate {
    pub(crate) fn exact(rows: u64) -> Self {
        Self {
            expected: rows,
            lower: rows,
            upper: rows,
        }
    }
}

/// Bounded, query-local observations of one compatible immutable generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexEstimate {
    pub rows: CardinalityEstimate,
    pub indexed_owners: u64,
    pub ndistinct: Option<u64>,
    pub mcv_rows: Option<u64>,
    pub sampled_owners: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlannerStatistics {
    pub node_count: u64,
    pub relationship_count: u64,
    pub average_out_degree: f64,
    pub(crate) label_counts: BTreeMap<LabelId, u64>,
    pub(crate) type_counts: BTreeMap<RelationshipTypeId, u64>,
}

impl PlannerStatistics {
    pub(crate) fn bounded(
        snapshot: &Snapshot<'_>,
        labels: &BTreeSet<LabelId>,
        types: &BTreeSet<RelationshipTypeId>,
        need_relationships: bool,
        max_work: usize,
    ) -> QueryResult<Self> {
        let Some(statistics) =
            snapshot.bounded_statistics(labels, types, need_relationships, max_work, 60 * 1024)?
        else {
            return Ok(Self::default());
        };
        Ok(Self {
            node_count: statistics.node_count,
            relationship_count: statistics.relationship_count,
            average_out_degree: if statistics.node_count > 0 {
                statistics.relationship_count as f64 / statistics.node_count as f64
            } else {
                0.0
            },
            label_counts: statistics.label_counts,
            type_counts: statistics.type_counts,
        })
    }

    pub fn label_selectivity(&self, label_id: LabelId) -> Option<f64> {
        self.label_counts.get(&label_id).map(|count| {
            if self.node_count == 0 {
                0.0
            } else {
                *count as f64 / self.node_count as f64
            }
        })
    }

    pub fn relationship_type_selectivity(&self, type_id: RelationshipTypeId) -> Option<f64> {
        self.type_counts.get(&type_id).map(|count| {
            if self.relationship_count == 0 {
                0.0
            } else {
                *count as f64 / self.relationship_count as f64
            }
        })
    }

    pub(crate) fn label_count(&self, label_id: LabelId) -> Option<u64> {
        self.label_counts.get(&label_id).copied()
    }
}
