//! Phase 04 read-query planning and execution.

use std::time::{SystemTime, UNIX_EPOCH};

mod completeness;
mod error;
mod expression;
mod functions;
mod graph;
mod ingestion;
mod managed_semantic;
mod mutation;
mod name_expression;
mod options;
mod plan;
pub(crate) mod registry;
mod schema;
mod semantic_index;
mod spill;
mod stats;
mod stream;
mod transaction;
mod version;

pub use error::{QueryError, QueryErrorKind, QueryResult};
pub use options::{ExecutionOptions, GraphViewSelector, MergeSessionSelector, SnapshotSelector};
pub use plan::{
    LogicalOperator, LogicalPlan, PhysicalOperator, PhysicalPlan, PreparedQuery, prepare,
};
pub use stats::PlannerStatistics;
pub use stream::{
    OperatorRuntimeMetrics, QueryBatch, QueryCounters, QueryCursor, QueryMetrics, QuerySummary,
    QueryType,
};
#[doc(hidden)]
pub use version::validate_candidate_state;

pub(crate) fn now_micros() -> QueryResult<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| QueryError::internal("system clock is before the Unix epoch"))?;
    i64::try_from(elapsed.as_micros())
        .map_err(|_| QueryError::internal("system clock exceeds supported Commit timestamp range"))
}

pub(crate) fn check_interrupted(is_interrupted: &dyn Fn() -> bool) -> QueryResult<()> {
    if is_interrupted() {
        Err(QueryError::interrupted())
    } else {
        Ok(())
    }
}
