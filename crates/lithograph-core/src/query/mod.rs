//! Phase 04 read-query planning and execution.

mod error;
mod expression;
mod graph;
mod options;
mod plan;
mod spill;
mod stats;
mod stream;

pub use error::{QueryError, QueryErrorKind, QueryResult};
pub use options::{ExecutionOptions, GraphViewSelector, SnapshotSelector};
pub use plan::{
    LogicalOperator, LogicalPlan, PhysicalOperator, PhysicalPlan, PreparedQuery, prepare,
};
pub use stats::PlannerStatistics;
pub use stream::{QueryBatch, QueryCursor, QueryMetrics, QuerySummary};
