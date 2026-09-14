use std::collections::{BTreeMap, BTreeSet};

use crate::cypher::{
    AstKind, AstNode, MatchModeKind, NameExpressionKind, PathModeKind, PathSelectorKind,
    QuantifierKind, Value,
};
use crate::storage::{self, OwnerKind, RelationshipRecord, Snapshot};

use super::super::expression::{
    self, BindingRow, BindingValue, Expr, binding_value, compile_expression,
};
use super::super::graph::ResolvedGraphView;
use super::super::{QueryError, QueryErrorKind, QueryMetrics, QueryResult};

#[derive(Debug, Clone)]
struct PatternPartSpec {
    path_variable: Option<String>,
    selector: PathSelector,
    path_mode: PathModeKind,
    fragments: Vec<Fragment>,
}

#[derive(Debug, Clone)]
enum Fragment {
    Simple(SimpleFragment),
    Quantified(QuantifiedFragment),
}

#[derive(Debug, Clone)]
struct SimpleFragment {
    start: NodePattern,
    chains: Vec<RelationshipChain>,
}

#[derive(Debug, Clone)]
struct QuantifiedFragment {
    fragments: Vec<Fragment>,
    bounds: Bounds,
    predicate: Option<Expr>,
    variables: Vec<String>,
}

#[derive(Debug, Clone)]
struct RelationshipChain {
    relationship: RelationshipPattern,
    end: NodePattern,
    bounds: Bounds,
    grouped_relationship: bool,
}

#[derive(Debug, Clone)]
struct NodePattern {
    variable: Option<String>,
    labels: Option<AstNode>,
    properties: Option<Expr>,
    predicate: Option<Expr>,
}

#[derive(Debug, Clone)]
struct RelationshipPattern {
    variable: Option<String>,
    types: Option<AstNode>,
    properties: Option<Expr>,
    predicate: Option<Expr>,
    direction: Direction,
}

#[derive(Debug, Clone, Copy)]
enum Direction {
    Outgoing,
    Incoming,
    Undirected,
}

#[derive(Debug, Clone, Copy)]
struct Bounds {
    minimum: usize,
    maximum: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
struct PathSelector {
    kind: PathSelectorKind,
    count: usize,
}

#[derive(Debug, Clone)]
struct PathState {
    row: BindingRow,
    current: Option<i64>,
    nodes: Vec<i64>,
    relationships: Vec<RelationshipRecord>,
}

type CollectedBindings = BTreeMap<String, Vec<BindingValue>>;
type QuantifiedState = (PathState, CollectedBindings);

struct PathContext<'a, 'connection> {
    snapshot: &'a Snapshot<'connection>,
    graph_view: &'a ResolvedGraphView,
    params: &'a BTreeMap<String, Value>,
    metrics: &'a mut QueryMetrics,
    is_interrupted: &'a dyn Fn() -> bool,
    match_mode: MatchModeKind,
}

pub(super) fn execute_match(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    params: &BTreeMap<String, Value>,
    clause: &AstNode,
    input: Vec<BindingRow>,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let match_mode = clause
        .children
        .iter()
        .find_map(|node| match node.kind {
            AstKind::MatchMode(mode) => Some(mode),
            _ => None,
        })
        .unwrap_or(MatchModeKind::DifferentRelationships);
    let pattern = clause
        .children
        .iter()
        .find(|node| node.kind == AstKind::Pattern)
        .ok_or_else(|| QueryError::semantic("MATCH clause is missing its pattern"))?;
    let parts = pattern
        .children
        .iter()
        .filter(|node| node.kind == AstKind::PatternPart)
        .map(|node| parse_part(node, params))
        .collect::<QueryResult<Vec<_>>>()?;
    let mut context = PathContext {
        snapshot,
        graph_view,
        params,
        metrics,
        is_interrupted,
        match_mode,
    };
    let mut output = Vec::new();
    for mut row in input {
        check_interrupted(is_interrupted)?;
        row.used_relationships.clear();
        let mut rows = vec![row];
        for part in &parts {
            let mut expanded = Vec::new();
            for row in rows {
                expanded.extend(execute_part(&mut context, part, row)?);
            }
            rows = expanded;
            if rows.is_empty() {
                break;
            }
        }
        output.extend(rows);
    }
    Ok(output)
}

fn parse_part(part: &AstNode, params: &BTreeMap<String, Value>) -> QueryResult<PatternPartSpec> {
    let path_variable = part
        .children
        .iter()
        .find(|node| node.kind == AstKind::PathAssignment)
        .and_then(|node| {
            node.descendants()
                .find(|child| child.kind == AstKind::PatternVariable)
        })
        .and_then(|node| node.text.clone());
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

fn parse_node(node: &AstNode) -> QueryResult<NodePattern> {
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

fn execute_part(
    context: &mut PathContext<'_, '_>,
    part: &PatternPartSpec,
    row: BindingRow,
) -> QueryResult<Vec<BindingRow>> {
    let initial = PathState {
        row,
        current: None,
        nodes: Vec::new(),
        relationships: Vec::new(),
    };
    let mut states = execute_fragments(context, part, &part.fragments, vec![initial])?;
    for state in &mut states {
        if let Some(path_variable) = &part.path_variable {
            bind_path(
                &mut state.row,
                path_variable,
                &state.nodes,
                &state.relationships,
            )?;
        }
    }
    states = apply_selector(states, part.selector);
    Ok(states.into_iter().map(|state| state.row).collect())
}

fn execute_fragments(
    context: &mut PathContext<'_, '_>,
    part: &PatternPartSpec,
    fragments: &[Fragment],
    mut states: Vec<PathState>,
) -> QueryResult<Vec<PathState>> {
    for fragment in fragments {
        check_interrupted(context.is_interrupted)?;
        states = match fragment {
            Fragment::Simple(simple) => execute_simple(context, part, simple, states)?,
            Fragment::Quantified(group) => execute_quantified(context, part, group, states)?,
        };
        if states.is_empty() {
            break;
        }
    }
    Ok(states)
}

fn execute_simple(
    context: &mut PathContext<'_, '_>,
    part: &PatternPartSpec,
    simple: &SimpleFragment,
    states: Vec<PathState>,
) -> QueryResult<Vec<PathState>> {
    let mut started = Vec::new();
    for state in states {
        started.extend(match_start_node(context, simple, state)?);
    }
    let mut states = started;
    for chain in &simple.chains {
        let mut expanded = Vec::new();
        for state in states {
            expanded.extend(execute_chain(context, part, chain, state)?);
        }
        states = expanded;
    }
    Ok(states)
}

fn match_start_node(
    context: &mut PathContext<'_, '_>,
    simple: &SimpleFragment,
    state: PathState,
) -> QueryResult<Vec<PathState>> {
    if let Some(current) = state.current {
        return match_node(context, &simple.start, state, current)
            .map(|state| state.into_iter().collect());
    }
    if let Some(variable) = &simple.start.variable
        && let Some(value) = state.row.values.get(variable)
    {
        return match value {
            BindingValue::Node(id) => match_node(context, &simple.start, state.clone(), *id)
                .map(|state| state.into_iter().collect()),
            BindingValue::Null => Ok(Vec::new()),
            _ => Err(QueryError::semantic(format!(
                "variable {variable} is not a Node binding"
            ))),
        };
    }
    let ids = candidate_start_nodes(context, &simple.start)?;
    let mut output = Vec::new();
    for id in ids {
        if let Some(state) = match_node(context, &simple.start, state.clone(), id)? {
            output.push(state);
        }
    }
    Ok(output)
}

fn candidate_start_nodes(
    context: &mut PathContext<'_, '_>,
    spec: &NodePattern,
) -> QueryResult<Vec<i64>> {
    let static_label = simple_static_label(spec);
    let pattern_label = match static_label {
        Some(name) => match storage::find_label(context.snapshot.connection_for_query(), name)? {
            Some(label_id) => Some(label_id),
            None => return Ok(Vec::new()),
        },
        None => None,
    };
    let scan_label = pattern_label.or_else(|| context.graph_view.scan_label());
    let mut ids = Vec::new();
    let mut after = 0_i64;
    loop {
        check_interrupted(context.is_interrupted)?;
        let page = match scan_label {
            Some(label_id) => context.snapshot.scan_label_after(label_id, after, 256)?,
            None => context.snapshot.scan_nodes_after(after, 256)?,
        };
        context.metrics.record_db_hits(page.items.len() as u64);
        ids.extend(page.items);
        let Some(next) = page.next_after else {
            break;
        };
        after = next;
    }
    Ok(ids)
}

fn simple_static_label(spec: &NodePattern) -> Option<&str> {
    let labels = spec.labels.as_ref()?;
    if labels.descendants().any(|node| match node.kind {
        AstKind::NameExpression(NameExpressionKind::Negation(count)) => count > 0,
        AstKind::NameExpression(NameExpressionKind::Dynamic | NameExpressionKind::Wildcard) => true,
        _ => false,
    }) {
        return None;
    }
    let mut names = labels
        .descendants()
        .filter(|node| node.kind == AstKind::LabelName)
        .filter_map(|node| node.text.as_deref());
    let name = names.next()?;
    names.next().is_none().then_some(name)
}

fn match_node(
    context: &mut PathContext<'_, '_>,
    spec: &NodePattern,
    mut state: PathState,
    node_id: i64,
) -> QueryResult<Option<PathState>> {
    if !context.graph_view.visible_node(context.snapshot, node_id)? {
        return Ok(None);
    }
    context.metrics.record_db_hits(1);
    if !bind_node(&mut state.row, spec.variable.as_deref(), node_id)? {
        return Ok(None);
    }
    if !node_predicates_match(context, spec, &state.row, node_id)? {
        return Ok(None);
    }
    if state.current.is_none() {
        state.current = Some(node_id);
        state.nodes.push(node_id);
    } else if state.current != Some(node_id) {
        return Ok(None);
    }
    Ok(Some(state))
}

fn execute_chain(
    context: &mut PathContext<'_, '_>,
    part: &PatternPartSpec,
    chain: &RelationshipChain,
    state: PathState,
) -> QueryResult<Vec<PathState>> {
    if chain.grouped_relationship {
        return execute_repeated_relationship(context, part, chain, state);
    }
    let mut output = Vec::new();
    for state in expand_one(context, part, &chain.relationship, state)? {
        let Some(end) = state.current else {
            continue;
        };
        if let Some(state) = match_node(context, &chain.end, state, end)? {
            output.push(state);
        }
    }
    Ok(output)
}

fn execute_repeated_relationship(
    context: &mut PathContext<'_, '_>,
    part: &PatternPartSpec,
    chain: &RelationshipChain,
    state: PathState,
) -> QueryResult<Vec<PathState>> {
    let maximum = effective_maximum(context, chain.bounds)?;
    let prebound_group = chain
        .relationship
        .variable
        .as_ref()
        .and_then(|variable| state.row.values.get(variable))
        .cloned();
    let mut output = Vec::new();
    let mut frontier = vec![(state, Vec::<BindingValue>::new())];
    for depth in 0..=maximum {
        check_interrupted(context.is_interrupted)?;
        let mut next = Vec::new();
        for (state, collected) in frontier {
            if depth >= chain.bounds.minimum {
                let Some(end) = state.current else {
                    continue;
                };
                if let Some(matched) = complete_repeated_state(
                    context,
                    chain,
                    &state,
                    &collected,
                    prebound_group.as_ref(),
                    end,
                )? {
                    output.push(matched);
                }
            }
            if depth < maximum {
                next.extend(advance_repeated_state(
                    context, part, chain, state, collected,
                )?);
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    Ok(output)
}

fn complete_repeated_state(
    context: &mut PathContext<'_, '_>,
    chain: &RelationshipChain,
    state: &PathState,
    collected: &[BindingValue],
    prebound_group: Option<&BindingValue>,
    end: i64,
) -> QueryResult<Option<PathState>> {
    let Some(mut matched) = match_node(context, &chain.end, state.clone(), end)? else {
        return Ok(None);
    };
    if let Some(variable) = &chain.relationship.variable
        && !bind_repeated_relationship_values(
            context.snapshot,
            &mut matched.row,
            variable,
            collected,
            prebound_group,
        )?
    {
        return Ok(None);
    }
    Ok(Some(matched))
}

fn bind_repeated_relationship_values(
    snapshot: &Snapshot<'_>,
    row: &mut BindingRow,
    variable: &str,
    values: &[BindingValue],
    prebound_group: Option<&BindingValue>,
) -> QueryResult<bool> {
    let values = values
        .iter()
        .map(|value| binding_value(snapshot, value))
        .collect::<QueryResult<Vec<_>>>()?;
    let value = Value::List(values);
    match prebound_group.or_else(|| row.values.get(variable)) {
        Some(BindingValue::Scalar(existing)) => {
            if crate::cypher::cypher_equals(existing, &value)? == Some(true) {
                row.insert(variable.to_owned(), BindingValue::Scalar(value));
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Some(BindingValue::Null) => Ok(false),
        Some(_) => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("variable {variable} is not a Relationship-list binding"),
        )),
        None => {
            row.insert(variable.to_owned(), BindingValue::Scalar(value));
            Ok(true)
        }
    }
}

fn advance_repeated_state(
    context: &mut PathContext<'_, '_>,
    part: &PatternPartSpec,
    chain: &RelationshipChain,
    mut state: PathState,
    collected: Vec<BindingValue>,
) -> QueryResult<Vec<(PathState, Vec<BindingValue>)>> {
    if let Some(variable) = &chain.relationship.variable {
        state.row.values.remove(variable);
        state.row.order.retain(|name| name != variable);
    }
    let mut next = Vec::new();
    for mut expanded in expand_one(context, part, &chain.relationship, state)? {
        let mut values = collected.clone();
        if let Some(variable) = &chain.relationship.variable
            && let Some(value) = expanded.row.values.remove(variable)
        {
            expanded.row.order.retain(|name| name != variable);
            values.push(value);
        }
        next.push((expanded, values));
    }
    Ok(next)
}

fn execute_quantified(
    context: &mut PathContext<'_, '_>,
    part: &PatternPartSpec,
    group: &QuantifiedFragment,
    states: Vec<PathState>,
) -> QueryResult<Vec<PathState>> {
    let maximum = effective_maximum(context, group.bounds)?;
    let mut output = Vec::new();
    let empty = group
        .variables
        .iter()
        .map(|name| (name.clone(), Vec::<BindingValue>::new()))
        .collect::<BTreeMap<_, _>>();
    let mut frontier = states
        .into_iter()
        .map(|state| (state, empty.clone()))
        .collect::<Vec<_>>();
    for depth in 0..=maximum {
        check_interrupted(context.is_interrupted)?;
        let mut next = Vec::new();
        for (state, collected) in frontier {
            if depth >= group.bounds.minimum {
                output.push(complete_quantified_state(context, &state, &collected)?);
            }
            if depth < maximum {
                next.extend(advance_quantified_state(
                    context, part, group, state, collected,
                )?);
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    Ok(output)
}

fn complete_quantified_state(
    context: &PathContext<'_, '_>,
    state: &PathState,
    collected: &CollectedBindings,
) -> QueryResult<PathState> {
    let mut completed = state.clone();
    for (name, values) in collected {
        bind_group_values(context.snapshot, &mut completed.row, name, values)?;
    }
    Ok(completed)
}

fn advance_quantified_state(
    context: &mut PathContext<'_, '_>,
    part: &PatternPartSpec,
    group: &QuantifiedFragment,
    mut state: PathState,
    collected: CollectedBindings,
) -> QueryResult<Vec<QuantifiedState>> {
    remove_quantified_bindings(&mut state, &group.variables);
    let expanded = execute_fragments(context, part, &group.fragments, vec![state])?;
    let mut next = Vec::new();
    for mut state in expanded {
        if !quantified_predicate_matches(context, group.predicate.as_ref(), &state.row)? {
            continue;
        }
        let mut values = collected.clone();
        collect_quantified_bindings(&mut state, &group.variables, &mut values);
        next.push((state, values));
    }
    Ok(next)
}

fn remove_quantified_bindings(state: &mut PathState, variables: &[String]) {
    for variable in variables {
        state.row.values.remove(variable);
        state.row.order.retain(|name| name != variable);
    }
}

fn quantified_predicate_matches(
    context: &PathContext<'_, '_>,
    predicate: Option<&Expr>,
    row: &BindingRow,
) -> QueryResult<bool> {
    match predicate {
        Some(predicate) => expression::predicate(expression::evaluate(
            predicate,
            context.snapshot,
            row,
            context.params,
        )?),
        None => Ok(true),
    }
}

fn collect_quantified_bindings(
    state: &mut PathState,
    variables: &[String],
    collected: &mut CollectedBindings,
) {
    for variable in variables {
        if let Some(value) = state.row.values.remove(variable) {
            collected.entry(variable.clone()).or_default().push(value);
        }
        state.row.order.retain(|name| name != variable);
    }
}

fn effective_maximum(context: &PathContext<'_, '_>, bounds: Bounds) -> QueryResult<usize> {
    match bounds.maximum {
        Some(maximum) => Ok(maximum),
        None if context.match_mode == MatchModeKind::DifferentRelationships => Ok(usize::MAX),
        None => Err(QueryError::semantic(
            "REPEATABLE ELEMENTS requires an upper bound on every quantified path",
        )),
    }
}

fn expand_one(
    context: &mut PathContext<'_, '_>,
    part: &PatternPartSpec,
    relationship: &RelationshipPattern,
    state: PathState,
) -> QueryResult<Vec<PathState>> {
    let current = state
        .current
        .ok_or_else(|| QueryError::internal("Relationship expansion has no current Node"))?;
    let mut candidates =
        relationship_candidates(context.snapshot, current, relationship.direction)?;
    candidates.sort_by_key(|record| record.id);
    context.metrics.record_db_hits(candidates.len() as u64);
    let mut output = Vec::new();
    for record in candidates {
        check_interrupted(context.is_interrupted)?;
        let Some(next_node) =
            candidate_next_node(context, part, relationship, &state, current, record)?
        else {
            continue;
        };
        if let Some(expanded) = expand_candidate(context, relationship, &state, record, next_node)?
        {
            output.push(expanded);
        }
    }
    Ok(output)
}

fn relationship_candidates(
    snapshot: &Snapshot<'_>,
    current: i64,
    direction: Direction,
) -> QueryResult<Vec<RelationshipRecord>> {
    match direction {
        Direction::Outgoing => Ok(snapshot.outgoing(current, None)?),
        Direction::Incoming => Ok(snapshot.incoming(current, None)?),
        Direction::Undirected => {
            let mut values = snapshot.outgoing(current, None)?;
            values.extend(snapshot.incoming(current, None)?);
            values.sort_by_key(|record| record.id);
            values.dedup_by_key(|record| record.id);
            Ok(values)
        }
    }
}

fn candidate_next_node(
    context: &PathContext<'_, '_>,
    part: &PatternPartSpec,
    relationship: &RelationshipPattern,
    state: &PathState,
    current: i64,
    record: RelationshipRecord,
) -> QueryResult<Option<i64>> {
    if !context
        .graph_view
        .visible_relationship(context.snapshot, record)?
        || context.match_mode == MatchModeKind::DifferentRelationships
            && state.row.used_relationships.contains(&record.id)
        || matches!(part.path_mode, PathModeKind::Trail)
            && state.relationships.iter().any(|used| used.id == record.id)
    {
        return Ok(None);
    }
    let next = match relationship.direction {
        Direction::Outgoing => record.target,
        Direction::Incoming => record.source,
        Direction::Undirected if record.source == current => record.target,
        Direction::Undirected => record.source,
    };
    if matches!(part.path_mode, PathModeKind::Acyclic) && state.nodes.contains(&next) {
        Ok(None)
    } else {
        Ok(Some(next))
    }
}

fn expand_candidate(
    context: &mut PathContext<'_, '_>,
    relationship: &RelationshipPattern,
    state: &PathState,
    record: RelationshipRecord,
    next_node: i64,
) -> QueryResult<Option<PathState>> {
    let mut expanded = state.clone();
    if !bind_relationship(&mut expanded.row, relationship.variable.as_deref(), record)?
        || !relationship_predicates_match(context, relationship, &expanded.row, record)?
    {
        return Ok(None);
    }
    expanded.current = Some(next_node);
    expanded.nodes.push(next_node);
    expanded.relationships.push(record);
    expanded.row.used_relationships.insert(record.id);
    Ok(Some(expanded))
}

fn node_predicates_match(
    context: &mut PathContext<'_, '_>,
    spec: &NodePattern,
    row: &BindingRow,
    node_id: i64,
) -> QueryResult<bool> {
    if let Some(labels) = &spec.labels {
        let ids = context.snapshot.labels(node_id)?;
        let mut names = BTreeSet::new();
        for id in ids {
            if let Some(name) = storage::label_name(context.snapshot.connection_for_query(), id)? {
                names.insert(name);
            }
        }
        if !name_expression_matches(labels, &names, context, row)? {
            return Ok(false);
        }
    }
    if let Some(properties) = &spec.properties
        && !properties_match(context, row, properties, OwnerKind::Node, node_id)?
    {
        return Ok(false);
    }
    predicate_matches(spec.predicate.as_ref(), context, row)
}

fn relationship_predicates_match(
    context: &mut PathContext<'_, '_>,
    spec: &RelationshipPattern,
    row: &BindingRow,
    record: RelationshipRecord,
) -> QueryResult<bool> {
    if let Some(types) = &spec.types {
        let name = storage::relationship_type_name(
            context.snapshot.connection_for_query(),
            record.type_id,
        )?
        .ok_or_else(|| QueryError::internal("Relationship Type dictionary entry is missing"))?;
        if !name_expression_matches(types, &BTreeSet::from([name]), context, row)? {
            return Ok(false);
        }
    }
    if let Some(properties) = &spec.properties
        && !properties_match(context, row, properties, OwnerKind::Relationship, record.id)?
    {
        return Ok(false);
    }
    predicate_matches(spec.predicate.as_ref(), context, row)
}

fn name_expression_matches(
    node: &AstNode,
    names: &BTreeSet<String>,
    context: &PathContext<'_, '_>,
    row: &BindingRow,
) -> QueryResult<bool> {
    super::super::name_expression::matches(node, names, |dynamic| {
        expression::evaluate(
            &compile_expression(dynamic)?,
            context.snapshot,
            row,
            context.params,
        )
    })
}

fn properties_match(
    context: &PathContext<'_, '_>,
    row: &BindingRow,
    expression: &Expr,
    owner: OwnerKind,
    owner_id: i64,
) -> QueryResult<bool> {
    let Value::Map(expected) =
        expression::evaluate(expression, context.snapshot, row, context.params)?
    else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "pattern properties must evaluate to a Map",
        ));
    };
    for (name, expected) in expected {
        let actual = storage::find_property_key(context.snapshot.connection_for_query(), &name)?
            .map(|key| context.snapshot.property(owner, owner_id, key))
            .transpose()?
            .flatten()
            .map(super::super::graph::property_value)
            .transpose()?
            .unwrap_or(Value::Null);
        if crate::cypher::cypher_equals(&actual, &expected)? != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn predicate_matches(
    predicate: Option<&Expr>,
    context: &PathContext<'_, '_>,
    row: &BindingRow,
) -> QueryResult<bool> {
    predicate.map_or(Ok(true), |predicate| {
        expression::predicate(expression::evaluate(
            predicate,
            context.snapshot,
            row,
            context.params,
        )?)
    })
}

fn bind_node(row: &mut BindingRow, variable: Option<&str>, id: i64) -> QueryResult<bool> {
    let Some(variable) = variable else {
        return Ok(true);
    };
    match row.values.get(variable) {
        Some(BindingValue::Node(existing)) => Ok(*existing == id),
        Some(BindingValue::Null) => Ok(false),
        Some(_) => Err(QueryError::semantic(format!(
            "variable {variable} is not a Node binding"
        ))),
        None => {
            row.insert(variable.to_owned(), BindingValue::Node(id));
            Ok(true)
        }
    }
}

fn bind_relationship(
    row: &mut BindingRow,
    variable: Option<&str>,
    relationship: RelationshipRecord,
) -> QueryResult<bool> {
    let Some(variable) = variable else {
        return Ok(true);
    };
    match row.values.get(variable) {
        Some(BindingValue::Relationship(existing)) => Ok(existing.id == relationship.id),
        Some(BindingValue::Null) => Ok(false),
        Some(_) => Err(QueryError::semantic(format!(
            "variable {variable} is not a Relationship binding"
        ))),
        None => {
            row.insert(
                variable.to_owned(),
                BindingValue::Relationship(relationship),
            );
            Ok(true)
        }
    }
}

fn bind_path(
    row: &mut BindingRow,
    variable: &str,
    nodes: &[i64],
    relationships: &[RelationshipRecord],
) -> QueryResult<()> {
    let value = BindingValue::Path {
        nodes: nodes.to_vec(),
        relationships: relationships.to_vec(),
    };
    match row.values.get(variable) {
        Some(existing) if existing == &value => Ok(()),
        Some(_) => Err(QueryError::semantic(format!(
            "path variable {variable} conflicts with an existing binding"
        ))),
        None => {
            row.insert(variable.to_owned(), value);
            Ok(())
        }
    }
}

fn bind_group_values(
    snapshot: &Snapshot<'_>,
    row: &mut BindingRow,
    variable: &str,
    values: &[BindingValue],
) -> QueryResult<()> {
    let values = values
        .iter()
        .map(|value| binding_value(snapshot, value))
        .collect::<QueryResult<Vec<_>>>()?;
    row.insert(
        variable.to_owned(),
        BindingValue::Scalar(Value::List(values)),
    );
    Ok(())
}

fn apply_selector(states: Vec<PathState>, selector: PathSelector) -> Vec<PathState> {
    if selector.kind == PathSelectorKind::All {
        return states;
    }
    let mut partitions = BTreeMap::<(Option<i64>, Option<i64>), Vec<PathState>>::new();
    for state in states {
        partitions
            .entry((state.nodes.first().copied(), state.nodes.last().copied()))
            .or_default()
            .push(state);
    }
    let mut output = Vec::new();
    for (_, mut paths) in partitions {
        paths.sort_by(|left, right| {
            left.relationships
                .len()
                .cmp(&right.relationships.len())
                .then_with(|| {
                    left.relationships
                        .iter()
                        .map(|relationship| relationship.id)
                        .cmp(
                            right
                                .relationships
                                .iter()
                                .map(|relationship| relationship.id),
                        )
                })
        });
        match selector.kind {
            PathSelectorKind::Any => output.extend(paths.into_iter().take(selector.count)),
            PathSelectorKind::AnyShortest => output.extend(paths.into_iter().take(1)),
            PathSelectorKind::AllShortest => {
                if let Some(length) = paths.first().map(|path| path.relationships.len()) {
                    output.extend(
                        paths
                            .into_iter()
                            .take_while(|path| path.relationships.len() == length),
                    );
                }
            }
            PathSelectorKind::ShortestPaths => {
                output.extend(paths.into_iter().take(selector.count));
            }
            PathSelectorKind::ShortestGroups => {
                let mut lengths = BTreeSet::new();
                for path in paths {
                    lengths.insert(path.relationships.len());
                    if lengths.len() > selector.count {
                        break;
                    }
                    output.push(path);
                }
            }
            PathSelectorKind::All => unreachable!(),
        }
    }
    output
}

fn check_interrupted(is_interrupted: &dyn Fn() -> bool) -> QueryResult<()> {
    if is_interrupted() {
        Err(QueryError::interrupted())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
