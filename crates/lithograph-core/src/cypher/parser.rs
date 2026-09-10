use pest::Parser;
use pest::error::{Error as PestError, InputLocation, LineColLocation};
use pest::iterators::Pair;
use pest_derive::Parser;

use super::ast::{
    AstKind, AstNode, ClauseKind, ExecutionMode, ExpressionKind, IndexKind, LiteralKind,
    PathModeKind, PathSelectorKind, QuantifierKind, QueryAst, QueryConnector, SubqueryKind,
    TransactionDisjointKind, TransactionErrorKind,
};
use super::error::{FrontendError, FrontendErrorKind, Span, line_column};

#[derive(Parser)]
#[grammar = "cypher/cypher.pest"]
struct CypherParser;

pub fn parse(source: &str) -> Result<QueryAst, FrontendError> {
    if source.trim().is_empty() {
        return Err(FrontendError::new(
            FrontendErrorKind::Parse,
            "expected a Cypher query",
            Span { start: 0, end: 0 },
            source,
        ));
    }
    let mut parsed =
        CypherParser::parse(Rule::query, source).map_err(|error| map_error(source, error))?;
    let query = parsed.next().ok_or_else(|| {
        FrontendError::new(
            FrontendErrorKind::Parse,
            "expected a Cypher query",
            Span { start: 0, end: 0 },
            source,
        )
    })?;
    lower_query(query, source)
}

pub(crate) fn parse_expression_fragment_at(
    fragment_source: &str,
    full_source: &str,
    source_offset: usize,
) -> Result<AstNode, FrontendError> {
    let mut parsed =
        CypherParser::parse(Rule::expression_fragment, fragment_source).map_err(|error| {
            let mut mapped = map_error(fragment_source, error);
            mapped.span.start = mapped.span.start.saturating_add(source_offset);
            mapped.span.end = mapped.span.end.saturating_add(source_offset);
            (mapped.line, mapped.column) = line_column(full_source, mapped.span.start);
            mapped
        })?;
    let fragment = parsed.next().ok_or_else(|| {
        FrontendError::new(
            FrontendErrorKind::Parse,
            "expected an expression",
            Span {
                start: source_offset,
                end: source_offset,
            },
            full_source,
        )
    })?;
    let expression = fragment
        .into_inner()
        .find(|pair| pair.as_rule() == Rule::expression)
        .ok_or_else(|| {
            FrontendError::new(
                FrontendErrorKind::Parse,
                "expected an expression",
                Span {
                    start: source_offset,
                    end: source_offset,
                },
                full_source,
            )
        })?;
    Ok(lower_node_at(expression, source_offset))
}

fn lower_query(pair: Pair<'_, Rule>, source: &str) -> Result<QueryAst, FrontendError> {
    let span = pair_span(&pair);
    let mut execution_mode = ExecutionMode::Execute;
    let mut cypher_version = 25_u8;
    let mut root = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::execution_mode => {
                execution_mode = if child.as_str().eq_ignore_ascii_case("EXPLAIN") {
                    ExecutionMode::Explain
                } else {
                    ExecutionMode::Profile
                };
            }
            Rule::cypher_prefix => {
                let text = child.as_str();
                if !text.to_ascii_uppercase().starts_with("CYPHER 25") {
                    return Err(FrontendError::new(
                        FrontendErrorKind::Parse,
                        "Lithograph only accepts the CYPHER 25 compatibility mode",
                        pair_span(&child),
                        source,
                    ));
                }
                cypher_version = 25;
            }
            Rule::query_body => root = Some(lower_node(child)),
            Rule::EOI => {}
            _ => {}
        }
    }
    let root = root.ok_or_else(|| {
        FrontendError::new(
            FrontendErrorKind::Parse,
            "expected a Cypher query body",
            span,
            source,
        )
    })?;
    Ok(QueryAst {
        cypher_version,
        execution_mode,
        span,
        root,
    })
}

fn lower_node(pair: Pair<'_, Rule>) -> AstNode {
    lower_node_at(pair, 0)
}

fn lower_node_at(pair: Pair<'_, Rule>, source_offset: usize) -> AstNode {
    let rule = pair.as_rule();
    let relative_span = pair_span(&pair);
    let span = Span {
        start: relative_span.start.saturating_add(source_offset),
        end: relative_span.end.saturating_add(source_offset),
    };
    let text = leaf_text(rule, pair.as_str());
    let kind = ast_kind(rule, &pair);
    let children = pair
        .into_inner()
        .map(|child| lower_node_at(child, source_offset))
        .collect();
    AstNode {
        kind,
        span,
        text,
        children,
    }
}

fn ast_kind(rule: Rule, pair: &Pair<'_, Rule>) -> AstKind {
    query_or_clause_ast_kind(rule, pair)
        .or_else(|| pattern_and_scope_ast_kind(rule, pair))
        .or_else(|| schema_ast_kind(rule, pair))
        .unwrap_or_else(|| expression_ast_kind(rule))
}

fn query_or_clause_ast_kind(rule: Rule, pair: &Pair<'_, Rule>) -> Option<AstKind> {
    let kind = match rule {
        Rule::query_body => AstKind::QueryBody,
        Rule::conditional_query => AstKind::ConditionalQuery,
        Rule::composed_query => AstKind::ComposedQuery,
        Rule::single_query => AstKind::SingleQuery,
        Rule::query_connector => AstKind::Connector(connector(pair.as_str())),
        Rule::optional_match_clause => AstKind::Clause(ClauseKind::OptionalMatch),
        Rule::match_clause => AstKind::Clause(ClauseKind::Match),
        Rule::filter_clause => AstKind::Clause(ClauseKind::Filter),
        Rule::return_clause => AstKind::Clause(ClauseKind::Return),
        Rule::with_clause => AstKind::Clause(ClauseKind::With),
        Rule::let_clause => AstKind::Clause(ClauseKind::Let),
        Rule::unwind_clause => AstKind::Clause(ClauseKind::Unwind),
        Rule::for_clause => AstKind::Clause(ClauseKind::For),
        Rule::finish_clause => AstKind::Clause(ClauseKind::Finish),
        Rule::create_clause => AstKind::Clause(ClauseKind::Create),
        Rule::insert_clause => AstKind::Clause(ClauseKind::Insert),
        Rule::merge_clause => AstKind::Clause(ClauseKind::Merge),
        Rule::set_clause => AstKind::Clause(ClauseKind::Set),
        Rule::remove_clause => AstKind::Clause(ClauseKind::Remove),
        Rule::delete_clause => AstKind::Clause(ClauseKind::Delete),
        Rule::detach_delete_clause => AstKind::Clause(ClauseKind::DetachDelete),
        Rule::foreach_clause => AstKind::Clause(ClauseKind::Foreach),
        Rule::call_clause => AstKind::Clause(ClauseKind::Call),
        Rule::load_csv_clause => AstKind::Clause(ClauseKind::LoadCsv),
        Rule::create_index_clause => AstKind::Clause(ClauseKind::CreateIndex),
        Rule::drop_index_clause => AstKind::Clause(ClauseKind::DropIndex),
        Rule::create_constraint_clause => AstKind::Clause(ClauseKind::CreateConstraint),
        Rule::drop_constraint_clause => AstKind::Clause(ClauseKind::DropConstraint),
        Rule::show_clause => AstKind::Clause(ClauseKind::Show),
        Rule::graph_type_clause => AstKind::Clause(ClauseKind::GraphType),
        Rule::search_subclause => AstKind::Search,
        _ => return None,
    };
    Some(kind)
}

fn pattern_and_scope_ast_kind(rule: Rule, pair: &Pair<'_, Rule>) -> Option<AstKind> {
    let kind = match rule {
        Rule::pattern | Rule::pattern_element => AstKind::Pattern,
        Rule::pattern_part => AstKind::PatternPart,
        Rule::path_assignment => AstKind::PathAssignment,
        Rule::path_mode => AstKind::PathMode(path_mode_kind(pair.as_str())),
        Rule::path_selector_all => AstKind::PathSelector(PathSelectorKind::All),
        Rule::path_selector_any => AstKind::PathSelector(PathSelectorKind::Any),
        Rule::path_selector_all_shortest => AstKind::PathSelector(PathSelectorKind::AllShortest),
        Rule::path_selector_any_shortest => AstKind::PathSelector(PathSelectorKind::AnyShortest),
        Rule::path_selector_shortest_paths => {
            AstKind::PathSelector(PathSelectorKind::ShortestPaths)
        }
        Rule::path_selector_shortest_groups => {
            AstKind::PathSelector(PathSelectorKind::ShortestGroups)
        }
        Rule::path_count => AstKind::PathCount,
        Rule::quantifier => AstKind::Quantifier(quantifier_kind(pair.as_str())),
        Rule::quantifier_lower_bound => AstKind::QuantifierLowerBound,
        Rule::quantifier_upper_bound => AstKind::QuantifierUpperBound,
        Rule::node_pattern => AstKind::NodePattern,
        Rule::relationship_pattern => AstKind::RelationshipPattern,
        Rule::left_arrow => AstKind::RelationshipLeftArrow,
        Rule::right_arrow => AstKind::RelationshipRightArrow,
        Rule::relationship_detail => AstKind::RelationshipDetail,
        Rule::relationship_type_expression => AstKind::RelationshipTypeExpression,
        Rule::variable_length => AstKind::VariableLength,
        Rule::projection_body => AstKind::ProjectionBody,
        Rule::group_by => AstKind::GroupBy,
        Rule::order_by => AstKind::OrderBy,
        Rule::where_subclause | Rule::inline_where => AstKind::Where,
        Rule::skip_clause => AstKind::Skip,
        Rule::limit_clause => AstKind::Limit,
        Rule::projection_item => AstKind::ProjectionItem,
        Rule::star_projection => AstKind::StarProjection,
        Rule::let_binding => AstKind::LetBinding,
        Rule::expression_list | Rule::function_arguments => AstKind::ArgumentList,
        Rule::subquery_scope => AstKind::SubqueryScope,
        Rule::subquery_import => AstKind::SubqueryImport,
        Rule::subquery_scope_all => AstKind::SubqueryScopeAll,
        Rule::transaction_concurrent => AstKind::TransactionConcurrent,
        Rule::transaction_batch => AstKind::TransactionBatch,
        Rule::transaction_disjoint => {
            AstKind::TransactionDisjoint(transaction_disjoint_kind(pair.as_str()))
        }
        Rule::transaction_error => AstKind::TransactionError(transaction_error_kind(pair.as_str())),
        Rule::transaction_status => AstKind::TransactionStatus,
        Rule::transaction_status_binding => AstKind::TransactionStatusBinding,
        Rule::load_csv_binding => AstKind::LoadCsvBinding,
        Rule::subquery_call => AstKind::Subquery(SubqueryKind::Call),
        Rule::braced_query => AstKind::Subquery(SubqueryKind::Braced),
        Rule::exists_subquery_expression => AstKind::Subquery(SubqueryKind::Exists),
        Rule::count_subquery_expression => AstKind::Subquery(SubqueryKind::Count),
        Rule::collect_subquery_expression => AstKind::Subquery(SubqueryKind::Collect),
        Rule::pattern_variable => AstKind::PatternVariable,
        Rule::relationship_variable => AstKind::RelationshipVariable,
        Rule::projection_alias => AstKind::ProjectionAlias,
        Rule::yield_item => AstKind::YieldItem,
        Rule::yield_name => AstKind::YieldName,
        Rule::predicate_variable => AstKind::PredicateVariable,
        Rule::let_variable
        | Rule::unwind_variable
        | Rule::for_variable
        | Rule::foreach_variable
        | Rule::comprehension_variable => AstKind::BindingVariable,
        Rule::variable => AstKind::Variable,
        Rule::parameter => AstKind::Parameter,
        Rule::function_name | Rule::procedure_name => AstKind::FunctionName,
        Rule::property_key => AstKind::PropertyKey,
        Rule::label_name | Rule::graph_label => AstKind::LabelName,
        Rule::relationship_type_name | Rule::graph_relationship_name => {
            AstKind::RelationshipTypeName
        }
        _ => return None,
    };
    Some(kind)
}

fn schema_ast_kind(rule: Rule, pair: &Pair<'_, Rule>) -> Option<AstKind> {
    let kind = match rule {
        Rule::type_simple_name
        | Rule::type_any_property_value
        | Rule::type_property_value
        | Rule::type_any_relationship
        | Rule::type_any_edge
        | Rule::type_any_vertex
        | Rule::type_any_node
        | Rule::type_signed_integer
        | Rule::type_timestamp_without_time_zone
        | Rule::type_timestamp_with_time_zone
        | Rule::type_time_without_time_zone
        | Rule::type_time_with_time_zone
        | Rule::type_local_datetime
        | Rule::type_zoned_datetime
        | Rule::type_local_time
        | Rule::type_zoned_time
        | Rule::type_any_value
        | Rule::vector_type => AstKind::TypeName,
        Rule::vector_signed_integer | Rule::vector_coordinate_name => {
            AstKind::VectorCoordinateTypeName
        }
        Rule::type_expression => AstKind::TypeExpression,
        Rule::type_term => AstKind::TypeTerm,
        Rule::type_parameters => AstKind::TypeParameters,
        Rule::type_suffix => AstKind::TypeListSuffix,
        Rule::nullability => AstKind::TypeNotNull,
        Rule::vector_dimension => AstKind::VectorDimension,
        Rule::type_predicate_shorthand => AstKind::TypePredicate,
        Rule::constraint_requirement => AstKind::ConstraintRequirement,
        Rule::index_kind => AstKind::IndexKind(index_kind(pair.as_str())),
        Rule::index_target => AstKind::IndexTarget,
        Rule::index_additional_properties => AstKind::IndexAdditionalProperties,
        Rule::graph_property => AstKind::GraphProperty,
        _ => return None,
    };
    Some(kind)
}

fn expression_ast_kind(rule: Rule) -> AstKind {
    match rule {
        Rule::null_literal => AstKind::Literal(LiteralKind::Null),
        Rule::boolean_literal => AstKind::Literal(LiteralKind::Boolean),
        Rule::integer_literal => AstKind::Literal(LiteralKind::Integer),
        Rule::float_literal => AstKind::Literal(LiteralKind::Float),
        Rule::string_literal => AstKind::Literal(LiteralKind::String),
        Rule::interpolated_string => AstKind::Expression(ExpressionKind::InterpolatedString),
        Rule::expression => AstKind::Expression(ExpressionKind::Expression),
        Rule::or_expression => AstKind::Expression(ExpressionKind::Or),
        Rule::xor_expression => AstKind::Expression(ExpressionKind::Xor),
        Rule::and_expression => AstKind::Expression(ExpressionKind::And),
        Rule::not_expression => AstKind::Expression(ExpressionKind::Not),
        Rule::comparison_expression => AstKind::Expression(ExpressionKind::Comparison),
        Rule::comparison_suffix => AstKind::ComparisonSuffix,
        Rule::subscript => AstKind::Subscript,
        Rule::additive_expression => AstKind::Expression(ExpressionKind::Additive),
        Rule::multiplicative_expression => AstKind::Expression(ExpressionKind::Multiplicative),
        Rule::power_expression => AstKind::Expression(ExpressionKind::Power),
        Rule::unary_expression => AstKind::Expression(ExpressionKind::Unary),
        Rule::postfix_expression => AstKind::Expression(ExpressionKind::Postfix),
        Rule::property_exists_function
        | Rule::vector_function_call
        | Rule::list_predicate_function
        | Rule::function_call => AstKind::Expression(ExpressionKind::FunctionCall),
        Rule::property_exists_name => AstKind::FunctionName,
        Rule::list_literal | Rule::list_comprehension | Rule::pattern_comprehension => {
            AstKind::Expression(ExpressionKind::List)
        }
        Rule::map_literal | Rule::map_projection => AstKind::Expression(ExpressionKind::Map),
        Rule::case_expression | Rule::searched_case | Rule::simple_case => {
            AstKind::Expression(ExpressionKind::Case)
        }
        Rule::parenthesized_expression => AstKind::Expression(ExpressionKind::Parenthesized),
        Rule::primary_expression => AstKind::Expression(ExpressionKind::Primary),
        Rule::pattern_expression => AstKind::Pattern,
        Rule::comparison_operator
        | Rule::KW_IN
        | Rule::KW_CONTAINS
        | Rule::KW_STARTS
        | Rule::KW_ENDS
        | Rule::KW_IS
        | Rule::additive_operator
        | Rule::multiplicative_operator
        | Rule::unary_operator
        | Rule::AND_OP
        | Rule::OR_OP
        | Rule::XOR_OP
        | Rule::NOT_OP => AstKind::Operator,
        _ => AstKind::Syntax,
    }
}

fn connector(text: &str) -> QueryConnector {
    let upper = text.to_ascii_uppercase();
    if upper.starts_with("NEXT") {
        QueryConnector::Next
    } else if upper.contains(" ALL") {
        QueryConnector::UnionAll
    } else if upper.contains(" DISTINCT") {
        QueryConnector::UnionDistinct
    } else {
        QueryConnector::Union
    }
}

fn path_mode_kind(text: &str) -> PathModeKind {
    match text.to_ascii_uppercase().as_str() {
        "WALK" => PathModeKind::Walk,
        "TRAIL" => PathModeKind::Trail,
        "ACYCLIC" => PathModeKind::Acyclic,
        "SIMPLE" => PathModeKind::Simple,
        _ => unreachable!("path_mode grammar only accepts known modes"),
    }
}

fn quantifier_kind(text: &str) -> QuantifierKind {
    match text.as_bytes().first() {
        Some(b'*') => QuantifierKind::ZeroOrMore,
        Some(b'+') => QuantifierKind::OneOrMore,
        Some(b'{') if text.contains(',') => QuantifierKind::Range,
        Some(b'{') => QuantifierKind::Fixed,
        _ => unreachable!("quantifier grammar only accepts known forms"),
    }
}

fn transaction_disjoint_kind(text: &str) -> TransactionDisjointKind {
    let upper = text.to_ascii_uppercase();
    if upper.ends_with(" NONE") {
        TransactionDisjointKind::None
    } else if upper.ends_with(" AUTO") {
        TransactionDisjointKind::Auto
    } else {
        TransactionDisjointKind::Explicit
    }
}

fn transaction_error_kind(text: &str) -> TransactionErrorKind {
    let upper = text.to_ascii_uppercase();
    if upper.contains(" RETRY") {
        TransactionErrorKind::Retry
    } else if upper.ends_with(" CONTINUE") {
        TransactionErrorKind::Continue
    } else if upper.ends_with(" BREAK") {
        TransactionErrorKind::Break
    } else {
        TransactionErrorKind::Fail
    }
}

fn index_kind(text: &str) -> IndexKind {
    match text.to_ascii_uppercase().as_str() {
        "LOOKUP" => IndexKind::Lookup,
        "RANGE" => IndexKind::Range,
        "TEXT" => IndexKind::Text,
        "POINT" => IndexKind::Point,
        "FULLTEXT" => IndexKind::FullText,
        "VECTOR" => IndexKind::Vector,
        _ => unreachable!("index_kind grammar only accepts known kinds"),
    }
}

fn leaf_text(rule: Rule, text: &str) -> Option<String> {
    let canonical_type = match rule {
        Rule::type_any_property_value | Rule::type_property_value => Some("PROPERTY VALUE"),
        Rule::type_any_relationship | Rule::type_any_edge => Some("RELATIONSHIP"),
        Rule::type_any_vertex | Rule::type_any_node => Some("NODE"),
        Rule::type_signed_integer => Some("INTEGER"),
        Rule::type_timestamp_without_time_zone | Rule::type_local_datetime => {
            Some("LOCAL DATETIME")
        }
        Rule::type_timestamp_with_time_zone | Rule::type_zoned_datetime => Some("ZONED DATETIME"),
        Rule::type_time_without_time_zone | Rule::type_local_time => Some("LOCAL TIME"),
        Rule::type_time_with_time_zone | Rule::type_zoned_time => Some("ZONED TIME"),
        Rule::type_any_value => Some("ANY"),
        Rule::vector_type => Some("VECTOR"),
        Rule::vector_signed_integer => Some("INTEGER"),
        _ => None,
    };
    if let Some(canonical_type) = canonical_type {
        return Some(canonical_type.to_owned());
    }
    match rule {
        Rule::relationship_pattern
        | Rule::relationship_detail
        | Rule::relationship_type_expression
        | Rule::variable_length
        | Rule::projection_body
        | Rule::group_by
        | Rule::order_by
        | Rule::skip_clause
        | Rule::limit_clause
        | Rule::yield_item
        | Rule::yield_name
        | Rule::vector_function_call
        | Rule::list_predicate_function
        | Rule::predicate_variable
        | Rule::subquery_scope
        | Rule::subquery_import
        | Rule::transaction_status_binding
        | Rule::load_csv_binding
        | Rule::path_count
        | Rule::quantifier_lower_bound
        | Rule::quantifier_upper_bound
        | Rule::pattern_variable
        | Rule::relationship_variable
        | Rule::projection_alias
        | Rule::let_variable
        | Rule::unwind_variable
        | Rule::for_variable
        | Rule::foreach_variable
        | Rule::comprehension_variable
        | Rule::variable
        | Rule::parameter
        | Rule::function_name
        | Rule::property_exists_name
        | Rule::procedure_name
        | Rule::property_key
        | Rule::label_name
        | Rule::relationship_type_name
        | Rule::type_simple_name
        | Rule::vector_coordinate_name
        | Rule::vector_dimension
        | Rule::null_literal
        | Rule::boolean_literal
        | Rule::integer_literal
        | Rule::float_literal
        | Rule::string_literal
        | Rule::interpolated_string
        | Rule::comparison_operator
        | Rule::KW_IN
        | Rule::KW_CONTAINS
        | Rule::KW_STARTS
        | Rule::KW_ENDS
        | Rule::KW_IS
        | Rule::additive_operator
        | Rule::multiplicative_operator
        | Rule::unary_operator
        | Rule::AND_OP
        | Rule::OR_OP
        | Rule::XOR_OP
        | Rule::NOT_OP => Some(text.to_owned()),
        _ => None,
    }
}

fn pair_span(pair: &Pair<'_, Rule>) -> Span {
    let span = pair.as_span();
    Span {
        start: span.start(),
        end: span.end(),
    }
}

fn map_error(_source: &str, error: PestError<Rule>) -> FrontendError {
    let (start, end) = match error.location {
        InputLocation::Pos(position) => (position, position),
        InputLocation::Span((start, end)) => (start, end),
    };
    let (line, column) = match error.line_col {
        LineColLocation::Pos((line, column)) => (line as u32, column as u32),
        LineColLocation::Span((line, column), _) => (line as u32, column as u32),
    };
    FrontendError {
        kind: FrontendErrorKind::Parse,
        message: format!(
            "expected valid Cypher 25 syntax: {}",
            error.variant.message()
        ),
        span: Span { start, end },
        line,
        column,
    }
}
