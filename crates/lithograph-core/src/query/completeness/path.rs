use std::collections::{BTreeMap, BTreeSet};

use crate::cypher::{
    AstKind, AstNode, MatchModeKind, NameExpressionKind, PathModeKind, PathSelectorKind, Value,
};
use crate::storage::{self, OwnerKind, RelationshipRecord, Snapshot};

use super::super::expression::{self, BindingRow, BindingValue, Expr, compile_expression};
use super::super::graph::ResolvedGraphView;
use super::super::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, checked_mul,
};
use super::super::{QueryError, QueryErrorKind, QueryMetrics, QueryResult};

mod parsing;
mod selector;
#[cfg(test)]
use parsing::parse_node;
use parsing::parse_part;
use selector::apply_selector;
#[cfg(test)]
use selector::sort_paths;

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

#[derive(Debug)]
struct PathState {
    row: BindingRow,
    current: Option<i64>,
    nodes: Vec<i64>,
    relationships: Vec<RelationshipRecord>,
    memory: MemoryReservation,
}

impl PathState {
    fn try_clone(&self, control: &ExecutionControl<'_>) -> QueryResult<Self> {
        let memory = control.reserve(self.memory.bytes())?;
        Ok(Self {
            row: expression::clone_binding_row_unreserved_controlled(&self.row, Some(control))?,
            current: self.current,
            nodes: clone_slice(&self.nodes, control)?,
            relationships: clone_slice(&self.relationships, control)?,
            memory,
        })
    }

    fn reserve_binding(&mut self, variable: Option<&str>) -> QueryResult<()> {
        if let Some(variable) = variable
            && !self.row.values.contains_key(variable)
        {
            let mut bytes = checked_add(
                crate::query::resources::map_entry_bytes::<BindingValue>()?,
                checked_mul(variable.len() as u64, 2)?,
            )?;
            if self.row.order.len() == self.row.order.capacity() {
                bytes = checked_add(
                    bytes,
                    allocation_bytes::<String>(self.row.order.capacity().max(4))?,
                )?;
            }
            self.memory.grow(bytes)?;
        }
        Ok(())
    }
}

fn binding_row_bytes(row: &BindingRow, control: &ExecutionControl<'_>) -> QueryResult<u64> {
    checked_add(
        expression::binding_row_bytes_controlled(row, Some(control))?,
        std::mem::size_of::<PathState>() as u64,
    )
}

// The caller owns a checked lease covering the clone before these helpers run.
fn clone_slice<T: Copy>(values: &[T], control: &ExecutionControl<'_>) -> QueryResult<Vec<T>> {
    let mut output = Vec::with_capacity(values.len());
    for chunk in values.chunks(256) {
        control.checkpoint(chunk.len() as u64)?;
        output.extend_from_slice(chunk);
    }
    Ok(output)
}

type CollectedBindings = BTreeMap<String, Vec<BindingValue>>;
type QuantifiedState = (PathState, CollectedBindings);

struct PathContext<'a, 'connection> {
    snapshot: &'a Snapshot<'connection>,
    graph_view: &'a ResolvedGraphView,
    params: &'a BTreeMap<String, Value>,
    metrics: &'a mut QueryMetrics,
    control: &'a ExecutionControl<'a>,
    match_mode: MatchModeKind,
}

pub(super) fn execute_match(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    params: &BTreeMap<String, Value>,
    clause: &AstNode,
    input: Vec<BindingRow>,
    metrics: &mut QueryMetrics,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<BindingRow>, MemoryReservation)> {
    let match_mode = clause.match_mode();
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
        control,
        match_mode,
    };
    let mut output = Vec::new();
    let mut output_memory = control.reserve(0)?;
    for mut row in input {
        control.checkpoint(1)?;
        row.used_relationships.clear();
        let (rows, rows_memory) = execute_match_parts(&mut context, &parts, row)?;
        output_memory.merge(rows_memory)?;
        output.extend(rows);
    }
    Ok((output, output_memory))
}

fn execute_match_parts(
    context: &mut PathContext<'_, '_>,
    parts: &[PatternPartSpec],
    row: BindingRow,
) -> QueryResult<(Vec<BindingRow>, MemoryReservation)> {
    let control = context.control;
    let mut rows = vec![row];
    let mut rows_memory = control.reserve(0)?;
    for part in parts {
        let mut expanded = Vec::new();
        let mut expanded_memory = control.reserve(0)?;
        for row in rows {
            context.control.checkpoint(1)?;
            let (matches, memory) = execute_part(context, part, row)?;
            expanded_memory.merge(memory)?;
            expanded.extend(matches);
        }
        rows = expanded;
        rows_memory = expanded_memory;
        if rows.is_empty() {
            break;
        }
    }
    Ok((rows, rows_memory))
}

fn execute_part(
    context: &mut PathContext<'_, '_>,
    part: &PatternPartSpec,
    row: BindingRow,
) -> QueryResult<(Vec<BindingRow>, MemoryReservation)> {
    let memory = context
        .control
        .reserve(binding_row_bytes(&row, context.control)?)?;
    let initial = PathState {
        row,
        current: None,
        nodes: Vec::new(),
        relationships: Vec::new(),
        memory,
    };
    let mut states = execute_fragments(context, part, &part.fragments, vec![initial])?;
    bind_part_paths(context, part, &mut states)?;
    states = apply_selector(states, part.selector, context.control)?;
    let mut memory = context
        .control
        .reserve(allocation_bytes::<BindingRow>(states.len())?)?;
    let mut rows = Vec::with_capacity(states.len());
    for state in states {
        context.control.checkpoint(1)?;
        memory.merge(state.memory)?;
        rows.push(state.row);
    }
    Ok((rows, memory))
}

fn bind_part_paths(
    context: &PathContext<'_, '_>,
    part: &PatternPartSpec,
    states: &mut [PathState],
) -> QueryResult<()> {
    for state in states {
        context.control.checkpoint(1)?;
        if let Some(path_variable) = &part.path_variable {
            state.reserve_binding(Some(path_variable))?;
            state.memory.grow(checked_add(
                allocation_bytes::<i64>(state.nodes.len())?,
                allocation_bytes::<RelationshipRecord>(state.relationships.len())?,
            )?)?;
            bind_path(
                &mut state.row,
                path_variable,
                &state.nodes,
                &state.relationships,
                context.control,
            )?;
        }
    }
    Ok(())
}

fn execute_fragments(
    context: &mut PathContext<'_, '_>,
    part: &PatternPartSpec,
    fragments: &[Fragment],
    mut states: Vec<PathState>,
) -> QueryResult<Vec<PathState>> {
    for fragment in fragments {
        context.control.checkpoint(1)?;
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
        context.control.checkpoint(1)?;
        started.extend(match_start_node(context, simple, state)?);
    }
    let mut states = started;
    for chain in &simple.chains {
        let mut expanded = Vec::new();
        for state in states {
            context.control.checkpoint(1)?;
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
            BindingValue::Node(id) => match_node(
                context,
                &simple.start,
                state.try_clone(context.control)?,
                *id,
            )
            .map(|state| state.into_iter().collect()),
            BindingValue::Null => Ok(Vec::new()),
            _ => Err(QueryError::semantic(format!(
                "variable {variable} is not a Node binding"
            ))),
        };
    }
    let (ids, _ids_memory) = candidate_start_nodes(context, &simple.start)?;
    let mut output = Vec::new();
    for id in ids {
        context.control.checkpoint(1)?;
        if let Some(state) = match_node(
            context,
            &simple.start,
            state.try_clone(context.control)?,
            id,
        )? {
            output.push(state);
        }
    }
    Ok(output)
}

fn candidate_start_nodes(
    context: &mut PathContext<'_, '_>,
    spec: &NodePattern,
) -> QueryResult<(Vec<i64>, MemoryReservation)> {
    let mut memory = context.control.reserve(0)?;
    let static_label = simple_static_label(spec).map(crate::cypher::unescape_identifier);
    let pattern_label = match static_label.as_deref() {
        Some(name) => match storage::find_label(context.snapshot.connection_for_query(), name)? {
            Some(label_id) => Some(label_id),
            None => return Ok((Vec::new(), memory)),
        },
        None => None,
    };
    let scan_label = pattern_label.or_else(|| context.graph_view.scan_label());
    let mut ids = Vec::new();
    let mut after = 0_i64;
    loop {
        context.control.check()?;
        let _page_memory = context.control.reserve(allocation_bytes::<i64>(256)?)?;
        let page = match scan_label {
            Some(label_id) => context.snapshot.scan_label_after(label_id, after, 256)?,
            None => context.snapshot.scan_nodes_after(after, 256)?,
        };
        context.metrics.record_db_hits(page.items.len() as u64);
        memory.grow(allocation_bytes::<i64>(page.items.len().saturating_mul(2))?)?;
        ids.extend(page.items);
        let Some(next) = page.next_after else {
            break;
        };
        after = next;
    }
    Ok((ids, memory))
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
    if !context
        .graph_view
        .visible_node_controlled(context.snapshot, node_id, context.control)?
    {
        return Ok(None);
    }
    context.metrics.record_db_hits(1);
    context.control.checkpoint(1)?;
    state.reserve_binding(spec.variable.as_deref())?;
    if !bind_node(&mut state.row, spec.variable.as_deref(), node_id)? {
        return Ok(None);
    }
    if !node_predicates_match(context, spec, &state.row, node_id)? {
        return Ok(None);
    }
    if state.current.is_none() {
        state.current = Some(node_id);
        state.memory.grow(allocation_bytes::<i64>(4)?)?;
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
    let prebound_memory = chain
        .relationship
        .variable
        .as_ref()
        .and_then(|variable| state.row.values.get(variable))
        .map(|value| {
            context.control.reserve(expression::binding_value_bytes(
                value,
                Some(context.control),
            )?)
        })
        .transpose()?;
    let _prebound_memory = prebound_memory;
    let prebound_group = chain
        .relationship
        .variable
        .as_ref()
        .and_then(|variable| state.row.values.get(variable))
        .map(|value| {
            expression::clone_binding_value_unreserved_controlled(value, Some(context.control))
        })
        .transpose()?;
    walk_frontier(
        context,
        maximum,
        vec![(state, Vec::<BindingValue>::new())],
        |context, depth, state, collected, output, next| {
            if depth >= chain.bounds.minimum {
                let Some(end) = state.current else {
                    return Ok(());
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
            Ok(())
        },
    )
}

fn complete_repeated_state(
    context: &mut PathContext<'_, '_>,
    chain: &RelationshipChain,
    state: &PathState,
    collected: &[BindingValue],
    prebound_group: Option<&BindingValue>,
    end: i64,
) -> QueryResult<Option<PathState>> {
    let Some(mut matched) =
        match_node(context, &chain.end, state.try_clone(context.control)?, end)?
    else {
        return Ok(None);
    };
    if let Some(variable) = &chain.relationship.variable
        && !bind_repeated_relationship_values(
            context,
            &mut matched,
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
    context: &PathContext<'_, '_>,
    state: &mut PathState,
    variable: &str,
    values: &[BindingValue],
    prebound_group: Option<&BindingValue>,
) -> QueryResult<bool> {
    let (value, memory) = materialize_group_values(context, values)?;
    state.memory.merge(memory)?;
    match prebound_group.or_else(|| state.row.values.get(variable)) {
        Some(BindingValue::Scalar(existing)) => {
            if crate::cypher::cypher_equals(existing, &value)? == Some(true) {
                state
                    .row
                    .insert(variable.to_owned(), BindingValue::Scalar(value));
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
            state
                .row
                .insert(variable.to_owned(), BindingValue::Scalar(value));
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
        context.control.checkpoint(1)?;
        let collected_bytes = collected.iter().try_fold(0, |bytes, value| {
            checked_add(
                bytes,
                expression::binding_value_bytes(value, Some(context.control))?,
            )
        })?;
        expanded.memory.grow(checked_add(
            collected_bytes,
            allocation_bytes::<BindingValue>(collected.len().saturating_add(1))?,
        )?)?;
        let mut values = collected
            .iter()
            .map(|value| {
                expression::clone_binding_value_unreserved_controlled(value, Some(context.control))
            })
            .collect::<QueryResult<Vec<_>>>()?;
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
    let empty = group
        .variables
        .iter()
        .map(|name| (name.clone(), Vec::<BindingValue>::new()))
        .collect::<BTreeMap<_, _>>();
    let frontier = states
        .into_iter()
        .map(|state| (state, empty.clone()))
        .collect::<Vec<_>>();
    walk_frontier(
        context,
        maximum,
        frontier,
        |context, depth, state, collected, output, next| {
            if depth >= group.bounds.minimum {
                output.push(complete_quantified_state(context, &state, &collected)?);
            }
            if depth < maximum {
                next.extend(advance_quantified_state(
                    context, part, group, state, collected,
                )?);
            }
            Ok(())
        },
    )
}

fn walk_frontier<'query, 'connection, T>(
    context: &mut PathContext<'query, 'connection>,
    maximum: usize,
    mut frontier: Vec<(PathState, T)>,
    mut visit: impl FnMut(
        &mut PathContext<'query, 'connection>,
        usize,
        PathState,
        T,
        &mut Vec<PathState>,
        &mut Vec<(PathState, T)>,
    ) -> QueryResult<()>,
) -> QueryResult<Vec<PathState>> {
    let mut output = Vec::new();
    for depth in 0..=maximum {
        context.control.checkpoint(1)?;
        let mut next = Vec::new();
        for (state, collected) in frontier {
            context.control.checkpoint(1)?;
            visit(context, depth, state, collected, &mut output, &mut next)?;
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
    let mut completed = state.try_clone(context.control)?;
    for (name, values) in collected {
        context.control.checkpoint(1)?;
        bind_group_values(context, &mut completed, name, values)?;
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
        context.control.checkpoint(1)?;
        if !quantified_predicate_matches(context, group.predicate.as_ref(), &state.row)? {
            continue;
        }
        state
            .memory
            .grow(collected_bindings_bytes(&collected, context.control)?)?;
        let mut values = clone_collected_bindings(&collected, context.control)?;
        collect_quantified_bindings(&mut state, &group.variables, &mut values);
        next.push((state, values));
    }
    Ok(next)
}

fn collected_bindings_bytes(
    collected: &CollectedBindings,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    collected.iter().try_fold(0, |bytes, (name, values)| {
        let values_bytes = values.iter().try_fold(
            allocation_bytes::<BindingValue>(values.len().saturating_add(1))?,
            |bytes, value| {
                checked_add(
                    bytes,
                    expression::binding_value_bytes(value, Some(control))?,
                )
            },
        )?;
        checked_add(bytes, checked_add(256 + name.len() as u64, values_bytes)?)
    })
}

fn clone_collected_bindings(
    collected: &CollectedBindings,
    control: &ExecutionControl<'_>,
) -> QueryResult<CollectedBindings> {
    collected
        .iter()
        .map(|(name, values)| {
            control.checkpoint(1)?;
            Ok((
                expression::clone_string_controlled(name, Some(control))?,
                values
                    .iter()
                    .map(|value| {
                        expression::clone_binding_value_unreserved_controlled(value, Some(control))
                    })
                    .collect::<QueryResult<Vec<_>>>()?,
            ))
        })
        .collect::<QueryResult<CollectedBindings>>()
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
        Some(predicate) => expression::predicate(
            expression::evaluate_with_access_control(
                predicate,
                context.snapshot,
                row,
                context.params,
                &context.graph_view.query_access,
                context.control,
            )?
            .value,
        ),
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
    let (mut candidates, _candidates_memory) =
        relationship_candidates(context, current, relationship.direction)?;
    context.control.check()?;
    candidates.sort_by_key(|record| record.id);
    context.control.check()?;
    context.metrics.record_db_hits(candidates.len() as u64);
    let mut output = Vec::new();
    for record in candidates {
        context.control.checkpoint(1)?;
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
    context: &PathContext<'_, '_>,
    current: i64,
    direction: Direction,
) -> QueryResult<(Vec<RelationshipRecord>, MemoryReservation)> {
    let mut memory = context.control.reserve(0)?;
    let mut values = Vec::new();
    let mut cursor = None;
    loop {
        context.control.check()?;
        // Storage merges bounded base and overlay pages before returning them.
        let _page_memory = context
            .control
            .reserve(allocation_bytes::<RelationshipRecord>(1024)?)?;
        let page = match direction {
            Direction::Outgoing => context
                .snapshot
                .scan_outgoing_page(current, None, cursor, 256)?,
            Direction::Incoming => context
                .snapshot
                .scan_incoming_page(current, None, cursor, 256)?,
            Direction::Undirected => context
                .snapshot
                .scan_incident_page(current, None, cursor, 256)?,
        };
        memory.grow(allocation_bytes::<RelationshipRecord>(
            page.items.len().saturating_mul(2),
        )?)?;
        values.extend(page.items);
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    Ok((values, memory))
}

fn candidate_next_node(
    context: &PathContext<'_, '_>,
    part: &PatternPartSpec,
    relationship: &RelationshipPattern,
    state: &PathState,
    current: i64,
    record: RelationshipRecord,
) -> QueryResult<Option<i64>> {
    if !context.graph_view.visible_relationship_controlled(
        context.snapshot,
        record,
        context.control,
    )? || context.match_mode == MatchModeKind::DifferentRelationships
        && state.row.used_relationships.contains(&record.id)
        || matches!(part.path_mode, PathModeKind::Trail)
            && path_contains_relationship(state, record.id, context.control)?
    {
        return Ok(None);
    }
    let next = match relationship.direction {
        Direction::Outgoing => record.target,
        Direction::Incoming => record.source,
        Direction::Undirected if record.source == current => record.target,
        Direction::Undirected => record.source,
    };
    if matches!(part.path_mode, PathModeKind::Acyclic)
        && path_contains_node(state, next, context.control)?
    {
        Ok(None)
    } else {
        Ok(Some(next))
    }
}

fn path_contains_relationship(
    state: &PathState,
    id: i64,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    for record in &state.relationships {
        control.checkpoint(1)?;
        if record.id == id {
            return Ok(true);
        }
    }
    Ok(false)
}

fn path_contains_node(
    state: &PathState,
    id: i64,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    for node in &state.nodes {
        control.checkpoint(1)?;
        if *node == id {
            return Ok(true);
        }
    }
    Ok(false)
}

fn expand_candidate(
    context: &mut PathContext<'_, '_>,
    relationship: &RelationshipPattern,
    state: &PathState,
    record: RelationshipRecord,
    next_node: i64,
) -> QueryResult<Option<PathState>> {
    let mut expanded = state.try_clone(context.control)?;
    expanded.reserve_binding(relationship.variable.as_deref())?;
    expanded.memory.grow(checked_add(
        allocation_bytes::<i64>(2)?,
        allocation_bytes::<RelationshipRecord>(2)?,
    )?)?;
    if !expanded
        .row
        .bind_relationship(relationship.variable.as_deref(), record)?
        || !relationship_predicates_match(context, relationship, &expanded.row, record)?
    {
        return Ok(None);
    }
    expanded.current = Some(next_node);
    expanded.nodes.push(next_node);
    expanded.relationships.push(record);
    if !expanded.row.used_relationships.contains(&record.id) {
        expanded
            .memory
            .grow(crate::query::resources::map_entry_bytes::<i64>()?)?;
    }
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
            context.control.checkpoint(1)?;
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
    let mut memory = context.control.reserve(0)?;
    super::super::name_expression::matches_controlled(
        node,
        names,
        |dynamic| {
            memory.resize(0)?;
            let evaluated = expression::evaluate_with_access_control(
                &compile_expression(dynamic)?,
                context.snapshot,
                row,
                context.params,
                &context.graph_view.query_access,
                context.control,
            )?;
            // Keep the consumed dynamic value charged until this name collection
            // has been checked; the next callback replaces the released collection.
            memory.merge(evaluated.reservation)?;
            Ok(evaluated.value)
        },
        Some(context.control),
    )
}

fn properties_match(
    context: &PathContext<'_, '_>,
    row: &BindingRow,
    expression: &Expr,
    owner: OwnerKind,
    owner_id: i64,
) -> QueryResult<bool> {
    let expected = expression::evaluate_with_access_control(
        expression,
        context.snapshot,
        row,
        context.params,
        &context.graph_view.query_access,
        context.control,
    )?;
    let Value::Map(expected_values) = &expected.value else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "pattern properties must evaluate to a Map",
        ));
    };
    for (name, expected) in expected_values {
        context.control.checkpoint(1)?;
        if context.graph_view.query_access.denies(name) {
            return Ok(false);
        }
        let Some(key) = storage::find_property_key(context.snapshot.connection_for_query(), name)?
        else {
            return Ok(false);
        };
        let equal = context.snapshot.with_property_checked(
            owner,
            owner_id,
            key,
            &|| {
                context
                    .control
                    .check()
                    .map_err(QueryError::into_storage_error)
            },
            |actual| -> QueryResult<bool> {
                let Some(actual) = actual else {
                    return Ok(false);
                };
                let (actual, _memory) =
                    super::super::graph::borrowed_property_with_control(actual, context.control)?;
                Ok(
                    expression::cypher_equals_controlled(&actual, expected, Some(context.control))?
                        == Some(true),
                )
            },
        )?;
        if !equal {
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
        expression::predicate(
            expression::evaluate_with_access_control(
                predicate,
                context.snapshot,
                row,
                context.params,
                &context.graph_view.query_access,
                context.control,
            )?
            .value,
        )
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

fn bind_path(
    row: &mut BindingRow,
    variable: &str,
    nodes: &[i64],
    relationships: &[RelationshipRecord],
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let value = BindingValue::Path {
        nodes: clone_slice(nodes, control)?,
        relationships: clone_slice(relationships, control)?,
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

fn materialize_group_values(
    context: &PathContext<'_, '_>,
    values: &[BindingValue],
) -> QueryResult<(Value, MemoryReservation)> {
    let mut memory = context
        .control
        .reserve(allocation_bytes::<Value>(values.len())?)?;
    let mut converted = Vec::with_capacity(values.len());
    for value in values {
        context.control.checkpoint(1)?;
        let evaluated = expression::binding_value_with_memory(
            context.snapshot,
            value,
            &context.graph_view.query_access,
            context.control,
        )?;
        memory.merge(evaluated.reservation)?;
        converted.push(evaluated.value);
    }
    Ok((Value::List(converted), memory))
}

fn bind_group_values(
    context: &PathContext<'_, '_>,
    state: &mut PathState,
    variable: &str,
    values: &[BindingValue],
) -> QueryResult<()> {
    let (value, memory) = materialize_group_values(context, values)?;
    state.memory.merge(memory)?;
    state
        .row
        .insert(variable.to_owned(), BindingValue::Scalar(value));
    Ok(())
}

#[cfg(test)]
mod tests;
