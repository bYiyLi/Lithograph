//! Cypher 25 frontend and value-semantics foundation.

mod ast;
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
    AstKind, AstNode, ClauseKind, ExecutionMode, ExpressionKind, LiteralKind, QueryAst,
    QueryConnector,
};
pub use error::{FrontendError, FrontendErrorKind, Span};
pub use parser::parse;
pub use semantic::{BindingKind, analyze, validate};

pub use json::{
    decode_json, decode_json_text, decode_parameters, decode_parameters_text, encode_json,
    encode_json_text,
};
pub use temporal::{
    DateValue, DurationValue, LocalDateTimeValue, LocalTimeValue, TimeValue, ZonedDateTimeValue,
};
pub use types::CypherType;
pub use value::{
    CypherComparison, NodeValue, PathValue, PointValue, RelationshipValue, UuidValue, Value,
    ValueError, VectorCoordinateType, VectorValue, VectorValues, cypher_compare, cypher_equals,
};
pub use value_order::cypher_order_compare;
