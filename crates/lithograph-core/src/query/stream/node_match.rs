use crate::storage::{LabelId, Snapshot};

use super::QueryMetrics;
use crate::query::graph::ResolvedGraphView;
use crate::query::plan::NodeSpec;
use crate::query::{ExecutionControl, QueryError, QueryResult};

pub(super) fn node_matches(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    spec: &NodeSpec,
    node_id: i64,
    metrics: &mut QueryMetrics,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    if !graph_view.visible_node_controlled(snapshot, node_id, control)? {
        return Ok(false);
    }
    metrics.record_db_hits(1);
    if spec.labels.is_empty() {
        return Ok(true);
    }
    labels_match(snapshot, spec, node_id, metrics, control)
}

pub(super) fn node_matches_scanned(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    spec: &NodeSpec,
    node_id: i64,
    known_label: Option<LabelId>,
    metrics: &mut QueryMetrics,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    if !graph_view.visible_existing_node_controlled(snapshot, node_id, known_label, control)? {
        return Ok(false);
    }
    metrics.record_db_hits(1);
    if spec.labels.is_empty() {
        return Ok(true);
    }
    if let Some(label) = known_label
        && matching_label_count(&spec.labels, label, control)? == spec.labels.len()
    {
        return Ok(true);
    }
    labels_match(snapshot, spec, node_id, metrics, control)
}

fn labels_match(
    snapshot: &Snapshot<'_>,
    spec: &NodeSpec,
    node_id: i64,
    metrics: &mut QueryMetrics,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let mut matched = 0usize;
    let check = || control.check().map_err(QueryError::into_storage_error);
    snapshot.visit_node_labels_checked(node_id, &check, |label| {
        control.work(1)?;
        matched += matching_label_count(&spec.labels, label, control)?;
        Ok::<_, QueryError>(())
    })?;
    metrics.record_db_hits(1);
    Ok(matched == spec.labels.len())
}

fn matching_label_count(
    labels: &[LabelId],
    label: LabelId,
    control: &ExecutionControl<'_>,
) -> QueryResult<usize> {
    let mut matched = 0;
    for required in labels {
        control.work(1)?;
        matched += usize::from(*required == label);
    }
    Ok(matched)
}
