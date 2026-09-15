use crate::storage::{LabelId, Snapshot};

use super::QueryMetrics;
use crate::query::QueryResult;
use crate::query::graph::ResolvedGraphView;
use crate::query::plan::NodeSpec;

pub(super) fn node_matches(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    spec: &NodeSpec,
    node_id: i64,
    metrics: &mut QueryMetrics,
) -> QueryResult<bool> {
    if !graph_view.visible_node(snapshot, node_id)? {
        return Ok(false);
    }
    metrics.record_db_hits(1);
    if spec.labels.is_empty() {
        return Ok(true);
    }
    let labels = snapshot.labels(node_id)?;
    metrics.record_db_hits(1);
    Ok(spec
        .labels
        .iter()
        .all(|label| labels.binary_search(label).is_ok()))
}

pub(super) fn node_matches_scanned(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    spec: &NodeSpec,
    node_id: i64,
    known_label: Option<LabelId>,
    metrics: &mut QueryMetrics,
) -> QueryResult<bool> {
    if !graph_view.visible_existing_node(snapshot, node_id, known_label)? {
        return Ok(false);
    }
    metrics.record_db_hits(1);
    if spec.labels.is_empty()
        || known_label.is_some_and(|label| spec.labels.iter().all(|required| *required == label))
    {
        return Ok(true);
    }
    let labels = snapshot.labels(node_id)?;
    metrics.record_db_hits(1);
    Ok(spec
        .labels
        .iter()
        .all(|label| labels.binary_search(label).is_ok()))
}
