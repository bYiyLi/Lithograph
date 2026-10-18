//! Path-pattern lowering, selectors and quantifier validation.

use super::*;
use crate::cypher::QuantifierKind;

pub(super) fn parse_part(
    part: &AstNode,
    params: &BTreeMap<String, Value>,
) -> QueryResult<PatternPartSpec> {
    let path_variable = part.path_assignment_variable().map(str::to_owned);
    let selector = parse_selector(part, params)?;
    let path_mode = part
        .children
        .iter()
        .find_map(|node| match node.kind {
            AstKind::PathMode(mode) => Some(mode),
            _ => None,
        })
        .unwrap_or(PathModeKind::Walk);
    let element = part
        .children
        .iter()
        .find(|node| node.kind == AstKind::Pattern)
        .ok_or_else(|| QueryError::semantic("pattern part is missing its path pattern"))?;
    let fragments = parse_pattern_element(element)?;
    if fragments.is_empty() {
        return Err(QueryError::semantic(
            "path pattern has no executable fragment",
        ));
    }
    Ok(PatternPartSpec {
        path_variable,
        selector,
        path_mode,
        fragments,
    })
}

fn parse_pattern_element(element: &AstNode) -> QueryResult<Vec<Fragment>> {
    let segment_nodes = element
        .children
        .iter()
        .filter(|node| matches!(node.kind, AstKind::Pattern | AstKind::QuantifiedPattern))
        .collect::<Vec<_>>();
    if segment_nodes.is_empty() {
        return match element.kind {
            AstKind::Pattern => Ok(vec![Fragment::Simple(parse_simple(element)?)]),
            AstKind::QuantifiedPattern => {
                Ok(vec![Fragment::Quantified(parse_quantified(element)?)])
            }
            _ => Err(QueryError::semantic("invalid path segment")),
        };
    }
    if has_direct_node_pattern(element) {
        return Ok(vec![Fragment::Simple(parse_simple(element)?)]);
    }
    segment_nodes
        .into_iter()
        .map(|segment| match segment.kind {
            AstKind::Pattern => parse_pattern_segment(segment),
            AstKind::QuantifiedPattern => parse_quantified(segment).map(Fragment::Quantified),
            _ => unreachable!(),
        })
        .collect()
}

fn parse_pattern_segment(segment: &AstNode) -> QueryResult<Fragment> {
    if has_direct_node_pattern(segment) {
        Ok(Fragment::Simple(parse_simple(segment)?))
    } else {
        let mut nested = parse_pattern_element(segment)?;
        if nested.len() != 1 {
            return Err(QueryError::semantic(
                "nested path segment must resolve to one linear fragment",
            ));
        }
        Ok(nested.remove(0))
    }
}

fn has_direct_node_pattern(node: &AstNode) -> bool {
    node.children
        .iter()
        .any(|child| child.kind == AstKind::NodePattern)
}

fn parse_simple(node: &AstNode) -> QueryResult<SimpleFragment> {
    let start = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::NodePattern)
        .ok_or_else(|| QueryError::semantic("simple path is missing its start Node"))?;
    let chains = node
        .children
        .iter()
        .filter(|child| child.kind == AstKind::RelationshipChain)
        .map(parse_relationship_chain)
        .collect::<QueryResult<Vec<_>>>()?;
    Ok(SimpleFragment {
        start: parse_node(start)?,
        chains,
    })
}

fn parse_quantified(node: &AstNode) -> QueryResult<QuantifiedFragment> {
    let inner = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::Pattern)
        .ok_or_else(|| QueryError::semantic("quantified pattern is missing its inner path"))?;
    let fragments = parse_pattern_element(inner)?;
    let quantifier = node
        .children
        .iter()
        .find(|child| matches!(child.kind, AstKind::Quantifier(_)))
        .ok_or_else(|| QueryError::semantic("quantified pattern is missing its quantifier"))?;
    let predicate = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::Where)
        .map(compile_surface_expression)
        .transpose()?;
    let mut variables = Vec::new();
    collect_fragment_variables(&fragments, &mut variables);
    Ok(QuantifiedFragment {
        fragments,
        bounds: parse_quantifier(quantifier)?,
        predicate,
        variables,
    })
}

fn parse_relationship_chain(node: &AstNode) -> QueryResult<RelationshipChain> {
    let relationship = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::RelationshipPattern)
        .ok_or_else(|| QueryError::semantic("relationship chain is missing its Relationship"))?;
    let end = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::NodePattern)
        .ok_or_else(|| QueryError::semantic("relationship chain is missing its end Node"))?;
    let quantifier = node
        .children
        .iter()
        .find(|child| matches!(child.kind, AstKind::Quantifier(_)));
    let variable_length = relationship
        .descendants()
        .find(|child| child.kind == AstKind::VariableLength);
    let bounds = match (quantifier, variable_length) {
        (Some(_), Some(_)) => {
            return Err(QueryError::semantic(
                "Relationship cannot use both a variable-length marker and a quantifier",
            ));
        }
        (Some(quantifier), None) => parse_quantifier(quantifier)?,
        (None, Some(variable_length)) => parse_variable_length(variable_length)?,
        (None, None) => Bounds {
            minimum: 1,
            maximum: Some(1),
        },
    };
    Ok(RelationshipChain {
        relationship: parse_relationship(relationship)?,
        end: parse_node(end)?,
        bounds,
        grouped_relationship: quantifier.is_some() || variable_length.is_some(),
    })
}

pub(super) fn parse_node(node: &AstNode) -> QueryResult<NodePattern> {
    Ok(NodePattern {
        variable: node
            .children
            .iter()
            .find(|child| child.kind == AstKind::PatternVariable)
            .and_then(|child| child.text.clone()),
        labels: node
            .children
            .iter()
            .find(|child| child.kind == AstKind::LabelExpression)
            .cloned(),
        properties: direct_expression_kind(node, crate::cypher::ExpressionKind::Map)
            .map(compile_expression)
            .transpose()?,
        predicate: node
            .children
            .iter()
            .find(|child| child.kind == AstKind::Where)
            .map(compile_surface_expression)
            .transpose()?,
    })
}

fn parse_relationship(node: &AstNode) -> QueryResult<RelationshipPattern> {
    let detail = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::RelationshipDetail);
    let left = node
        .children
        .iter()
        .any(|child| child.kind == AstKind::RelationshipLeftArrow);
    let right = node
        .children
        .iter()
        .any(|child| child.kind == AstKind::RelationshipRightArrow);
    Ok(RelationshipPattern {
        variable: detail.and_then(|detail| {
            detail
                .children
                .iter()
                .find(|child| child.kind == AstKind::RelationshipVariable)
                .and_then(|child| child.text.clone())
        }),
        types: detail.and_then(|detail| {
            detail
                .children
                .iter()
                .find(|child| child.kind == AstKind::RelationshipTypeExpression)
                .cloned()
        }),
        properties: detail
            .and_then(|detail| direct_expression_kind(detail, crate::cypher::ExpressionKind::Map))
            .map(compile_expression)
            .transpose()?,
        predicate: detail
            .and_then(|detail| {
                detail
                    .children
                    .iter()
                    .find(|child| child.kind == AstKind::Where)
            })
            .map(compile_surface_expression)
            .transpose()?,
        direction: match (left, right) {
            (false, true) => Direction::Outgoing,
            (true, false) => Direction::Incoming,
            _ => Direction::Undirected,
        },
    })
}

fn direct_expression_kind(node: &AstNode, kind: crate::cypher::ExpressionKind) -> Option<&AstNode> {
    node.children
        .iter()
        .find(|child| child.kind == AstKind::Expression(kind))
}

fn compile_surface_expression(node: &AstNode) -> QueryResult<Expr> {
    node.children
        .iter()
        .find(|child| matches!(child.kind, AstKind::Expression(_)))
        .ok_or_else(|| QueryError::semantic("predicate is missing its expression"))
        .and_then(compile_expression)
}

fn parse_selector(part: &AstNode, params: &BTreeMap<String, Value>) -> QueryResult<PathSelector> {
    let Some(selector) = part
        .children
        .iter()
        .find(|node| matches!(node.kind, AstKind::PathSelector(_)))
    else {
        return Ok(PathSelector {
            kind: PathSelectorKind::All,
            count: usize::MAX,
        });
    };
    let AstKind::PathSelector(kind) = selector.kind else {
        unreachable!();
    };
    let count = selector
        .children
        .iter()
        .find(|node| node.kind == AstKind::PathCount)
        .map(|count| parse_path_count(count, params))
        .transpose()?
        .unwrap_or(1);
    if count == 0 {
        return Err(QueryError::semantic("path selector count must be positive"));
    }
    Ok(PathSelector { kind, count })
}

fn parse_path_count(node: &AstNode, params: &BTreeMap<String, Value>) -> QueryResult<usize> {
    let text = node.text.as_deref().unwrap_or_default().trim();
    if let Some(name) = text.strip_prefix('$') {
        let value = params.get(name).ok_or_else(|| {
            QueryError::invalid_argument(format!("parameter ${name} was not provided"))
        })?;
        let Value::Integer(value) = value else {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "path selector parameter must be an Integer",
            ));
        };
        return usize::try_from(*value).map_err(|_| {
            QueryError::new(
                QueryErrorKind::Type,
                "path selector parameter must be positive",
            )
        });
    }
    text.parse::<usize>()
        .map_err(|_| QueryError::semantic("invalid path selector count"))
}

fn parse_quantifier(node: &AstNode) -> QueryResult<Bounds> {
    let AstKind::Quantifier(kind) = node.kind else {
        return Err(QueryError::internal(
            "quantifier parser received another AST kind",
        ));
    };
    let lower = node
        .descendants()
        .find(|child| child.kind == AstKind::QuantifierLowerBound)
        .and_then(|child| child.text.as_deref())
        .map(parse_bound)
        .transpose()?;
    let upper = node
        .descendants()
        .find(|child| child.kind == AstKind::QuantifierUpperBound)
        .and_then(|child| child.text.as_deref())
        .map(parse_bound)
        .transpose()?;
    let bounds = match kind {
        QuantifierKind::ZeroOrMore => Bounds {
            minimum: 0,
            maximum: None,
        },
        QuantifierKind::OneOrMore => Bounds {
            minimum: 1,
            maximum: None,
        },
        QuantifierKind::Fixed => Bounds {
            minimum: lower.unwrap_or(0),
            maximum: Some(lower.unwrap_or(0)),
        },
        QuantifierKind::Range => Bounds {
            minimum: lower.unwrap_or(0),
            maximum: upper,
        },
    };
    validate_bounds(bounds)
}

fn parse_variable_length(node: &AstNode) -> QueryResult<Bounds> {
    let text = node.text.as_deref().unwrap_or_default().trim();
    let range = text.strip_prefix('*').unwrap_or(text);
    if range.is_empty() {
        return Ok(Bounds {
            minimum: 1,
            maximum: None,
        });
    }
    if let Some((lower, upper)) = range.split_once("..") {
        return Ok(Bounds {
            minimum: if lower.is_empty() {
                1
            } else {
                parse_bound(lower)?
            },
            maximum: (!upper.is_empty())
                .then(|| parse_bound(upper))
                .transpose()?,
        });
    }
    let fixed = parse_bound(range)?;
    Ok(Bounds {
        minimum: fixed,
        maximum: Some(fixed),
    })
}

fn parse_bound(text: &str) -> QueryResult<usize> {
    text.parse::<usize>()
        .map_err(|_| QueryError::semantic("invalid path quantifier bound"))
}

fn validate_bounds(bounds: Bounds) -> QueryResult<Bounds> {
    if bounds
        .maximum
        .is_some_and(|maximum| maximum < bounds.minimum)
    {
        return Err(QueryError::semantic(
            "path quantifier upper bound is smaller than its lower bound",
        ));
    }
    Ok(bounds)
}

fn collect_fragment_variables(fragments: &[Fragment], output: &mut Vec<String>) {
    for fragment in fragments {
        match fragment {
            Fragment::Simple(simple) => {
                append_optional(output, simple.start.variable.as_ref());
                for chain in &simple.chains {
                    append_optional(output, chain.relationship.variable.as_ref());
                    append_optional(output, chain.end.variable.as_ref());
                }
            }
            Fragment::Quantified(group) => {
                for variable in &group.variables {
                    append_unique(output, variable.clone());
                }
            }
        }
    }
}

fn append_optional(output: &mut Vec<String>, value: Option<&String>) {
    if let Some(value) = value {
        append_unique(output, value.clone());
    }
}

fn append_unique(output: &mut Vec<String>, value: String) {
    if !output.contains(&value) {
        output.push(value);
    }
}
