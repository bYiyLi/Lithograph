use pest::Parser;
use pest::error::{Error as PestError, InputLocation, LineColLocation};
use pest::iterators::Pair;
use pest_derive::Parser;

use super::ast::{AstNode, ExecutionMode, QueryAst, QueryOption, QueryOptionValue};
use super::error::{FrontendError, FrontendErrorKind, Span, line_column};

#[derive(Parser)]
#[grammar = "cypher/cypher.pest"]
struct CypherParser;

mod kind;
use kind::{ast_kind, leaf_text};

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
    let mut query_options = Vec::new();
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
                for nested in child.into_inner() {
                    match nested.as_rule() {
                        Rule::cypher_version => {
                            if nested.as_str() != "25" {
                                return Err(FrontendError::new(
                                    FrontendErrorKind::Parse,
                                    "Lithograph only accepts the CYPHER 25 compatibility mode",
                                    pair_span(&nested),
                                    source,
                                ));
                            }
                            cypher_version = 25;
                        }
                        Rule::query_option => {
                            query_options.push(lower_query_option(nested, source)?);
                        }
                        _ => {}
                    }
                }
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
        query_options,
        span,
        root,
    })
}

fn lower_query_option(pair: Pair<'_, Rule>, source: &str) -> Result<QueryOption, FrontendError> {
    let span = pair_span(&pair);
    let mut inner = pair.into_inner();
    let name = inner.next().ok_or_else(|| {
        FrontendError::new(
            FrontendErrorKind::Parse,
            "query option is missing a name",
            span,
            source,
        )
    })?;
    let value = inner.next().ok_or_else(|| {
        FrontendError::new(
            FrontendErrorKind::Parse,
            "query option is missing a value",
            span,
            source,
        )
    })?;
    let value = value.into_inner().next().ok_or_else(|| {
        FrontendError::new(
            FrontendErrorKind::Parse,
            "query option is missing a value",
            span,
            source,
        )
    })?;
    let value = match value.as_rule() {
        Rule::symbolic_name => QueryOptionValue::Identifier(value.as_str().to_owned()),
        Rule::string_literal => QueryOptionValue::StringLiteral(value.as_str().to_owned()),
        Rule::integer_literal => QueryOptionValue::IntegerLiteral(value.as_str().to_owned()),
        Rule::float_literal => QueryOptionValue::FloatLiteral(value.as_str().to_owned()),
        _ => unreachable!("query option grammar only accepts structured option values"),
    };
    Ok(QueryOption {
        name: name.as_str().to_owned(),
        value,
        span,
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
