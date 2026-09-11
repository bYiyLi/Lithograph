use pest::iterators::Pair;

use super::super::ast::{
    AstKind, ClauseKind, ConditionalBranchKind, ConstraintKind, ExistenceModifierKind,
    ExpressionKind, GraphTypeOperationKind, IndexKind, LiteralKind, MatchModeKind, MergeActionKind,
    NameExpressionKind, OrderDirectionKind, PathModeKind, PathSelectorKind, QuantifierKind,
    QueryConnector, SetOperatorKind, SetQuantifierKind, ShowTargetKind, SubqueryKind,
    TransactionDisjointKind, TransactionErrorKind,
};
use super::Rule;

pub(super) fn ast_kind(rule: Rule, pair: &Pair<'_, Rule>) -> AstKind {
    query_or_clause_ast_kind(rule, pair)
        .or_else(|| pattern_ast_kind(rule, pair))
        .or_else(|| scope_ast_kind(rule, pair))
        .or_else(|| schema_ast_kind(rule, pair))
        .unwrap_or_else(|| expression_ast_kind(rule))
}

fn query_or_clause_ast_kind(rule: Rule, pair: &Pair<'_, Rule>) -> Option<AstKind> {
    let kind = match rule {
        Rule::query_body => AstKind::QueryBody,
        Rule::conditional_query => AstKind::ConditionalQuery,
        Rule::when_branch => AstKind::ConditionalBranch(ConditionalBranchKind::When),
        Rule::else_branch => AstKind::ConditionalBranch(ConditionalBranchKind::Else),
        Rule::composed_query => AstKind::ComposedQuery,
        Rule::single_query => AstKind::SingleQuery,
        Rule::query_connector => AstKind::Connector(connector(pair)),
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

fn pattern_ast_kind(rule: Rule, pair: &Pair<'_, Rule>) -> Option<AstKind> {
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
        Rule::match_mode => AstKind::MatchMode(match_mode_kind(pair)),
        Rule::variable_length => AstKind::VariableLength,
        Rule::label_expression => AstKind::LabelExpression,
        Rule::label_disjunction | Rule::relationship_type_disjunction => {
            AstKind::NameExpression(NameExpressionKind::Disjunction)
        }
        Rule::label_conjunction | Rule::relationship_type_conjunction => {
            AstKind::NameExpression(NameExpressionKind::Conjunction)
        }
        Rule::label_negation | Rule::relationship_type_negation => {
            AstKind::NameExpression(NameExpressionKind::Negation(negation_count(pair.as_str())))
        }
        Rule::label_atom | Rule::relationship_type_atom => {
            AstKind::NameExpression(NameExpressionKind::Atom)
        }
        Rule::dynamic_name => AstKind::NameExpression(NameExpressionKind::Dynamic),
        Rule::name_wildcard => AstKind::NameExpression(NameExpressionKind::Wildcard),
        Rule::label_name | Rule::graph_label => AstKind::LabelName,
        Rule::relationship_type_name | Rule::graph_relationship_name => {
            AstKind::RelationshipTypeName
        }
        _ => return None,
    };
    Some(kind)
}

fn scope_ast_kind(rule: Rule, pair: &Pair<'_, Rule>) -> Option<AstKind> {
    let kind = match rule {
        Rule::projection_body => AstKind::ProjectionBody,
        Rule::projection_quantifier | Rule::function_quantifier => {
            AstKind::SetQuantifier(set_quantifier_kind(pair.as_str()))
        }
        Rule::group_by => AstKind::GroupBy,
        Rule::order_by => AstKind::OrderBy,
        Rule::order_direction => AstKind::OrderDirection(order_direction_kind(pair.as_str())),
        Rule::where_subclause | Rule::inline_where => AstKind::Where,
        Rule::skip_clause => AstKind::Skip,
        Rule::limit_clause => AstKind::Limit,
        Rule::projection_item => AstKind::ProjectionItem,
        Rule::star_projection => AstKind::StarProjection,
        Rule::let_binding => AstKind::LetBinding,
        Rule::merge_action => AstKind::MergeAction(merge_action_kind(pair)),
        Rule::set_operator => AstKind::SetOperator(set_operator_kind(pair.as_str())),
        Rule::label_update => AstKind::LabelUpdate,
        Rule::expression_list | Rule::function_arguments => AstKind::ArgumentList,
        Rule::subquery_scope => AstKind::SubqueryScope,
        Rule::subquery_import => AstKind::SubqueryImport,
        Rule::subquery_scope_all => AstKind::SubqueryScopeAll,
        Rule::transaction_concurrent => AstKind::TransactionConcurrent,
        Rule::transaction_batch => AstKind::TransactionBatch,
        Rule::transaction_disjoint => AstKind::TransactionDisjoint(transaction_disjoint_kind(pair)),
        Rule::transaction_error => AstKind::TransactionError(transaction_error_kind(pair)),
        Rule::transaction_retry_fallback => {
            AstKind::TransactionRetryFallback(transaction_retry_fallback_kind(pair))
        }
        Rule::transaction_status => AstKind::TransactionStatus,
        Rule::transaction_status_binding => AstKind::TransactionStatusBinding,
        Rule::yield_all => AstKind::YieldAll,
        Rule::load_csv_headers => AstKind::LoadCsvHeaders,
        Rule::load_csv_field_terminator => AstKind::LoadCsvFieldTerminator,
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
        Rule::constraint_kind | Rule::graph_property_constraint | Rule::graph_require => {
            AstKind::ConstraintKind(constraint_kind(pair))
        }
        Rule::index_kind => AstKind::IndexKind(index_kind(pair.as_str())),
        Rule::index_name => AstKind::IndexName,
        Rule::index_target => AstKind::IndexTarget,
        Rule::index_target_each => AstKind::IndexTargetEach,
        Rule::index_additional_properties => AstKind::IndexAdditionalProperties,
        Rule::constraint_name => AstKind::ConstraintName,
        Rule::if_not_exists => AstKind::ExistenceModifier(ExistenceModifierKind::IfNotExists),
        Rule::if_exists => AstKind::ExistenceModifier(ExistenceModifierKind::IfExists),
        Rule::show_target => AstKind::ShowTarget(show_target_kind(pair)),
        Rule::show_as_graph => AstKind::ShowAsGraph,
        Rule::graph_type_operation => {
            AstKind::GraphTypeOperation(graph_type_operation_kind(pair.as_str()))
        }
        Rule::graph_node_type => AstKind::GraphNodeType,
        Rule::graph_relationship_type => AstKind::GraphRelationshipType,
        Rule::graph_constraint => AstKind::GraphConstraint,
        Rule::graph_alias => AstKind::GraphAlias,
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

fn connector(pair: &Pair<'_, Rule>) -> QueryConnector {
    if direct_has_rule(pair, Rule::KW_NEXT) {
        QueryConnector::Next
    } else {
        let Some(union) = pair
            .clone()
            .into_inner()
            .find(|child| child.as_rule() == Rule::union_connector)
        else {
            unreachable!("query_connector grammar always contains NEXT or UNION")
        };
        if direct_has_rule(&union, Rule::KW_ALL) {
            QueryConnector::UnionAll
        } else if direct_has_rule(&union, Rule::KW_DISTINCT) {
            QueryConnector::UnionDistinct
        } else {
            QueryConnector::Union
        }
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

fn match_mode_kind(pair: &Pair<'_, Rule>) -> MatchModeKind {
    if direct_has_rule(pair, Rule::KW_DIFFERENT) {
        MatchModeKind::DifferentRelationships
    } else if direct_has_rule(pair, Rule::KW_REPEATABLE) {
        MatchModeKind::RepeatableElements
    } else {
        unreachable!("match_mode grammar only accepts known modes")
    }
}

fn set_quantifier_kind(text: &str) -> SetQuantifierKind {
    match text.trim().to_ascii_uppercase().as_str() {
        "ALL" => SetQuantifierKind::All,
        "DISTINCT" => SetQuantifierKind::Distinct,
        _ => unreachable!("set quantifier grammar only accepts known quantifiers"),
    }
}

fn order_direction_kind(text: &str) -> OrderDirectionKind {
    match text.to_ascii_uppercase().as_str() {
        "ASC" | "ASCENDING" => OrderDirectionKind::Ascending,
        "DESC" | "DESCENDING" => OrderDirectionKind::Descending,
        _ => unreachable!("order direction grammar only accepts known directions"),
    }
}

fn merge_action_kind(pair: &Pair<'_, Rule>) -> MergeActionKind {
    if direct_has_rule(pair, Rule::KW_CREATE) {
        MergeActionKind::Create
    } else if direct_has_rule(pair, Rule::KW_MATCH) {
        MergeActionKind::Match
    } else {
        unreachable!("merge_action grammar only accepts ON CREATE or ON MATCH")
    }
}

fn set_operator_kind(text: &str) -> SetOperatorKind {
    match text.trim() {
        "=" => SetOperatorKind::Assign,
        "+=" => SetOperatorKind::AddAssign,
        _ => unreachable!("set operator grammar only accepts = and +="),
    }
}

fn negation_count(text: &str) -> usize {
    text.trim_start()
        .chars()
        .take_while(|value| *value == '!')
        .count()
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

fn transaction_disjoint_kind(pair: &Pair<'_, Rule>) -> TransactionDisjointKind {
    if direct_has_rule(pair, Rule::transaction_disjoint_none) {
        TransactionDisjointKind::None
    } else if direct_has_rule(pair, Rule::transaction_disjoint_auto) {
        TransactionDisjointKind::Auto
    } else if direct_has_rule(pair, Rule::transaction_disjoint_explicit) {
        TransactionDisjointKind::Explicit
    } else {
        unreachable!("transaction_disjoint grammar only accepts known modes")
    }
}

fn transaction_error_kind(pair: &Pair<'_, Rule>) -> TransactionErrorKind {
    if direct_has_rule(pair, Rule::transaction_error_retry) {
        TransactionErrorKind::Retry
    } else if direct_has_rule(pair, Rule::transaction_error_continue) {
        TransactionErrorKind::Continue
    } else if direct_has_rule(pair, Rule::transaction_error_break) {
        TransactionErrorKind::Break
    } else if direct_has_rule(pair, Rule::transaction_error_fail) {
        TransactionErrorKind::Fail
    } else {
        unreachable!("transaction_error grammar only accepts known modes")
    }
}

fn transaction_retry_fallback_kind(pair: &Pair<'_, Rule>) -> TransactionErrorKind {
    if direct_has_rule(pair, Rule::KW_CONTINUE) {
        TransactionErrorKind::Continue
    } else if direct_has_rule(pair, Rule::KW_BREAK) {
        TransactionErrorKind::Break
    } else if direct_has_rule(pair, Rule::KW_FAIL) {
        TransactionErrorKind::Fail
    } else {
        unreachable!("transaction_retry_fallback grammar only accepts known modes")
    }
}

fn constraint_kind(pair: &Pair<'_, Rule>) -> ConstraintKind {
    if direct_has_rule(pair, Rule::KW_NOT) && direct_has_rule(pair, Rule::KW_NULL) {
        ConstraintKind::NotNull
    } else if direct_has_rule(pair, Rule::KW_RELATIONSHIP) && direct_has_rule(pair, Rule::KW_KEY) {
        ConstraintKind::RelationshipKey
    } else if direct_has_rule(pair, Rule::KW_NODE) && direct_has_rule(pair, Rule::KW_KEY) {
        ConstraintKind::NodeKey
    } else if direct_has_rule(pair, Rule::KW_KEY) {
        ConstraintKind::Key
    } else if direct_has_rule(pair, Rule::KW_RELATIONSHIP) && direct_has_rule(pair, Rule::KW_UNIQUE)
    {
        ConstraintKind::RelationshipUnique
    } else if direct_has_rule(pair, Rule::KW_NODE) && direct_has_rule(pair, Rule::KW_UNIQUE) {
        ConstraintKind::NodeUnique
    } else if direct_has_rule(pair, Rule::KW_UNIQUE) {
        ConstraintKind::Unique
    } else {
        unreachable!("constraint grammar only accepts known constraint kinds")
    }
}

fn show_target_kind(pair: &Pair<'_, Rule>) -> ShowTargetKind {
    if direct_has_rule(pair, Rule::KW_CURRENT) {
        ShowTargetKind::CurrentGraphType
    } else if direct_has_rule(pair, Rule::KW_INDEXES) {
        ShowTargetKind::Indexes
    } else if direct_has_rule(pair, Rule::KW_CONSTRAINTS) {
        ShowTargetKind::Constraints
    } else if direct_has_rule(pair, Rule::KW_FUNCTIONS) {
        ShowTargetKind::Functions
    } else if direct_has_rule(pair, Rule::KW_PROCEDURES) {
        ShowTargetKind::Procedures
    } else {
        unreachable!("show_target grammar only accepts known targets")
    }
}

fn direct_has_rule(pair: &Pair<'_, Rule>, rule: Rule) -> bool {
    pair.clone()
        .into_inner()
        .any(|child| child.as_rule() == rule)
}

fn graph_type_operation_kind(text: &str) -> GraphTypeOperationKind {
    match text.trim().to_ascii_uppercase().as_str() {
        "SET" => GraphTypeOperationKind::Set,
        "ADD" => GraphTypeOperationKind::Add,
        "ALTER" => GraphTypeOperationKind::Alter,
        "DROP" => GraphTypeOperationKind::Drop,
        _ => unreachable!("graph type operation grammar only accepts known operations"),
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

pub(super) fn leaf_text(rule: Rule, text: &str) -> Option<String> {
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
        | Rule::graph_label
        | Rule::relationship_type_name
        | Rule::graph_relationship_name
        | Rule::index_name
        | Rule::constraint_name
        | Rule::graph_alias
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
