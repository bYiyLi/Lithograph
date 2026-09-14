//! Cypher 25 frontend and value-semantics foundation.

mod ast;
mod builtins;
mod error;
mod json;
mod parser;
mod semantic;
mod semantic_clause;
mod semantic_expression;
mod semantic_interpolation;
mod semantic_projection;
mod semantic_rule_helpers;
mod semantic_rules;
mod semantic_schema;
mod semantic_transaction;
mod temporal;
mod temporal_datetime;
mod temporal_format;
mod types;
mod types_function;
mod types_literal;
mod uuid;
mod value;
mod value_order;

pub use ast::{
    AstKind, AstNode, ClauseKind, ConditionalBranchKind, ConstraintKind, ExecutionMode,
    ExistenceModifierKind, ExpressionKind, GraphTypeOperationKind, IndexKind, LiteralKind,
    MatchModeKind, MergeActionKind, NameExpressionKind, OrderDirectionKind, PathModeKind,
    PathSelectorKind, QuantifierKind, QueryAst, QueryConnector, QueryOption, QueryOptionValue,
    SetOperatorKind, SetQuantifierKind, ShowConstraintFilterKind, ShowTargetKind, SubqueryKind,
    TransactionDisjointKind, TransactionErrorKind,
};
pub use error::{FrontendError, FrontendErrorKind, Span};
pub use parser::parse;
pub(crate) use parser::parse_expression_fragment_at;
pub(crate) use semantic::unescape_identifier;
pub use semantic::{BindingKind, analyze, validate};
pub(crate) use semantic_clause::{show_projection_clause, show_yield_node};
pub(crate) use semantic_interpolation::interpolation_fragments;
pub(crate) use semantic_transaction::{
    query_body_ends_with_call, query_body_returns_columns, query_body_terminal_matches,
};
pub(crate) use types_literal::is_i64_min_unary_expression;

pub(crate) use builtins::{
    AGGREGATING_FUNCTIONS, SCALAR_FUNCTIONS, is_aggregating_function, is_builtin_function,
};
pub use json::{
    decode_json, decode_json_text, decode_parameters, decode_parameters_text, encode_json,
    encode_json_text,
};
pub use temporal::{
    DateValue, DurationValue, LocalDateTimeValue, LocalTimeValue, TimeValue, ZonedDateTimeValue,
};
pub(crate) use temporal::{
    civil_from_days, days_from_civil, days_in_month, format_date_from_days, format_local_time,
    format_offset,
};
pub use types::CypherType;
pub use value::{
    CypherComparison, NodeValue, PathValue, PointValue, RelationshipValue, UuidValue, Value,
    ValueError, VectorCoordinateType, VectorValue, VectorValues, cypher_compare, cypher_equals,
};
pub use value_order::cypher_order_compare;
