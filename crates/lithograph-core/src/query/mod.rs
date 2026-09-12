//! Phase 04 read-query planning and execution.

mod completeness;
mod error;
mod expression;
mod functions;
mod graph;
mod mutation;
mod name_expression;
mod options;
mod plan;
pub(crate) mod registry;
mod spill;
mod stats;
mod stream;

pub use error::{QueryError, QueryErrorKind, QueryResult};
pub use options::{ExecutionOptions, GraphViewSelector, SnapshotSelector};
pub use plan::{
    LogicalOperator, LogicalPlan, PhysicalOperator, PhysicalPlan, PreparedQuery, prepare,
};
pub use stats::PlannerStatistics;
pub use stream::{QueryBatch, QueryCounters, QueryCursor, QueryMetrics, QuerySummary, QueryType};
