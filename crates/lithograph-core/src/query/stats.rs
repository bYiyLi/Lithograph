use std::collections::{BTreeMap, BTreeSet};

use crate::storage::{LabelId, RelationshipTypeId, Snapshot};

use super::QueryResult;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlannerStatistics {
    pub node_count: u64,
    pub relationship_count: u64,
    pub average_out_degree: f64,
    pub(crate) label_counts: BTreeMap<LabelId, u64>,
    pub(crate) type_counts: BTreeMap<RelationshipTypeId, u64>,
}

impl PlannerStatistics {
    pub(crate) fn collect(
        snapshot: &Snapshot<'_>,
        labels: &BTreeSet<LabelId>,
        relationship_types: &BTreeSet<RelationshipTypeId>,
        need_nodes: bool,
        need_relationships: bool,
    ) -> QueryResult<Self> {
        let Some(snapshot_statistics) = snapshot.statistics()? else {
            return Ok(Self::default());
        };
        let node_count = if need_nodes {
            snapshot_statistics.node_count
        } else {
            0
        };
        let label_counts = labels
            .iter()
            .filter_map(|label_id| {
                snapshot_statistics
                    .label_counts
                    .get(label_id)
                    .copied()
                    .map(|count| (*label_id, count))
            })
            .collect::<BTreeMap<_, _>>();
        let relationship_count = if need_relationships {
            snapshot_statistics.relationship_count
        } else {
            0
        };
        let type_counts = relationship_types
            .iter()
            .filter_map(|type_id| {
                snapshot_statistics
                    .type_counts
                    .get(type_id)
                    .copied()
                    .map(|count| (*type_id, count))
            })
            .collect::<BTreeMap<_, _>>();
        let average_out_degree = if node_count == 0 {
            0.0
        } else {
            relationship_count as f64 / node_count as f64
        };
        Ok(Self {
            node_count,
            relationship_count,
            average_out_degree,
            label_counts,
            type_counts,
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
