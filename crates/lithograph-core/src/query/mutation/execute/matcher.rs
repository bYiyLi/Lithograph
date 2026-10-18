use super::*;
use crate::query::access::PropertyRules;

pub(super) fn match_pattern(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    seed: &BindingRow,
    pattern: &WritePatternPart,
    params: &BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<BindingRow>, MemoryReservation)> {
    let (mut seed, _seed_memory) = clone_binding(seed, control)?;
    seed.used_relationships.clear();
    let matcher = PathMatchContext {
        connection,
        snapshot,
        graph_view,
        pattern,
        params,
        control,
        output_memory: std::cell::RefCell::new(control.reserve(0)?),
    };
    let (starts, _starts_memory) = node_candidates(
        connection,
        snapshot,
        graph_view,
        &seed,
        &pattern.nodes[0],
        params,
        control,
    )?;
    let mut output = Vec::new();
    for start in starts {
        control.check()?;
        let (mut row, _row_memory) = clone_binding(&seed, control)?;
        bind_node(&mut row, pattern.nodes[0].variable.as_deref(), start)?;
        let mut nodes = vec![start];
        let mut relationships = Vec::new();
        match_path_from(
            &matcher,
            0,
            start,
            row,
            &mut nodes,
            &mut relationships,
            &mut output,
        )?;
    }
    Ok((output, matcher.output_memory.into_inner()))
}

struct PathMatchContext<'a, 'snapshot> {
    connection: &'a Connection,
    snapshot: &'a Snapshot<'snapshot>,
    graph_view: &'a ResolvedGraphView,
    pattern: &'a WritePatternPart,
    params: &'a BTreeMap<String, Value>,
    control: &'a ExecutionControl<'a>,
    output_memory: std::cell::RefCell<MemoryReservation>,
}

fn match_path_from(
    context: &PathMatchContext<'_, '_>,
    index: usize,
    current: i64,
    row: BindingRow,
    nodes: &mut Vec<i64>,
    relationships: &mut Vec<RelationshipRecord>,
    output: &mut Vec<BindingRow>,
) -> QueryResult<()> {
    if index == context.pattern.relationships.len() {
        return emit_complete_path(context, row, nodes, relationships, output);
    }
    let rel_spec = &context.pattern.relationships[index];
    let (candidates, _candidate_memory) =
        path_relationship_candidates(context, current, rel_spec, &row)?;
    for relationship in candidates {
        context.control.check()?;
        if !relationship_is_usable(context, &row, rel_spec, relationship)?
            || !relationship_binding_matches(&row, rel_spec, relationship)
        {
            continue;
        }
        match_next_relationship(
            context,
            index,
            &row,
            relationship,
            nodes,
            relationships,
            output,
        )?;
    }
    Ok(())
}

fn emit_complete_path(
    context: &PathMatchContext<'_, '_>,
    row: BindingRow,
    nodes: &[i64],
    relationships: &[RelationshipRecord],
    output: &mut Vec<BindingRow>,
) -> QueryResult<()> {
    let mut memory = context.output_memory.borrow_mut();
    memory.grow(expression::binding_row_bytes(&row)?)?;
    if context.pattern.path_variable.is_some() {
        memory.grow(crate::query::resources::allocation_bytes::<i64>(
            nodes.len(),
        )?)?;
        memory.grow(crate::query::resources::allocation_bytes::<
            RelationshipRecord,
        >(relationships.len())?)?;
        memory.grow(256)?;
    }
    reserve_entries(output, 1, &mut memory)?;
    output.push(complete_path(context.pattern, row, nodes, relationships));
    Ok(())
}

fn match_next_relationship(
    context: &PathMatchContext<'_, '_>,
    index: usize,
    row: &BindingRow,
    relationship: RelationshipRecord,
    nodes: &mut Vec<i64>,
    relationships: &mut Vec<RelationshipRecord>,
    output: &mut Vec<BindingRow>,
) -> QueryResult<()> {
    let rel_spec = &context.pattern.relationships[index];
    let current = *nodes
        .last()
        .ok_or_else(|| QueryError::internal("mutation path is missing its current Node"))?;
    let next = match rel_spec.direction {
        Direction::Outgoing => relationship.target,
        Direction::Incoming => relationship.source,
        Direction::Undirected if relationship.source == current => relationship.target,
        Direction::Undirected => relationship.source,
    };
    let (mut next_row, _next_memory) = clone_binding(row, context.control)?;
    if !node_matches(
        context.snapshot,
        context.graph_view,
        &next_row,
        &context.pattern.nodes[index + 1],
        next,
        context.params,
        context.control,
    )? {
        return Ok(());
    }
    bind_relationship(&mut next_row, rel_spec.variable.as_deref(), relationship)?;
    bind_node(
        &mut next_row,
        context.pattern.nodes[index + 1].variable.as_deref(),
        next,
    )?;
    nodes.push(next);
    relationships.push(relationship);
    match_path_from(
        context,
        index + 1,
        next,
        next_row,
        nodes,
        relationships,
        output,
    )?;
    nodes.pop();
    relationships.pop();
    Ok(())
}

fn complete_path(
    pattern: &WritePatternPart,
    mut row: BindingRow,
    nodes: &[i64],
    relationships: &[RelationshipRecord],
) -> BindingRow {
    if let Some(variable) = &pattern.path_variable {
        row.insert(
            variable.clone(),
            BindingValue::Path {
                nodes: nodes.to_vec(),
                relationships: relationships.to_vec(),
            },
        );
    }
    row
}

fn path_relationship_candidates(
    context: &PathMatchContext<'_, '_>,
    current: i64,
    spec: &RelationshipWriteSpec,
    row: &BindingRow,
) -> QueryResult<(Vec<RelationshipRecord>, MemoryReservation)> {
    let mut memory = context.control.reserve(0)?;
    let (names, _names_memory) = resolve_write_names(
        std::slice::from_ref(&spec.relationship_type),
        context.snapshot,
        row,
        context.params,
        &context.graph_view.query_access,
        context.control,
    )?;
    if names.len() != 1 {
        return Err(QueryError::semantic(
            "dynamic Relationship Type expression must produce exactly one name",
        ));
    }
    let Some(type_id) = storage::find_relationship_type(context.connection, &names[0])? else {
        return Ok((Vec::new(), memory));
    };
    let mut cursor = None;
    let mut output = Vec::new();
    loop {
        context.control.check()?;
        let page = match spec.direction {
            Direction::Outgoing => {
                context
                    .snapshot
                    .scan_outgoing_page(current, Some(type_id), cursor, SCAN_BATCH)?
            }
            Direction::Incoming => {
                context
                    .snapshot
                    .scan_incoming_page(current, Some(type_id), cursor, SCAN_BATCH)?
            }
            Direction::Undirected => {
                context
                    .snapshot
                    .scan_incident_page(current, Some(type_id), cursor, SCAN_BATCH)?
            }
        };
        reserve_entries(&mut output, page.items.len(), &mut memory)?;
        output.extend(page.items);
        let Some(next) = page.next_cursor else {
            return Ok((output, memory));
        };
        cursor = Some(next);
    }
}

fn relationship_is_usable(
    context: &PathMatchContext<'_, '_>,
    row: &BindingRow,
    spec: &RelationshipWriteSpec,
    relationship: RelationshipRecord,
) -> QueryResult<bool> {
    if row.used_relationships.contains(&relationship.id) {
        return Ok(false);
    }
    if !context.graph_view.visible_relationship_controlled(
        context.snapshot,
        relationship,
        context.control,
    )? {
        return Ok(false);
    }
    relationship_properties_match(
        context.snapshot,
        row,
        spec,
        relationship.id,
        context.params,
        &context.graph_view.query_access,
        context.control,
    )
}

fn relationship_binding_matches(
    row: &BindingRow,
    spec: &RelationshipWriteSpec,
    relationship: RelationshipRecord,
) -> bool {
    let Some(variable) = &spec.variable else {
        return true;
    };
    match row.values.get(variable) {
        None => true,
        Some(BindingValue::Relationship(value)) => value.id == relationship.id,
        Some(_) => false,
    }
}

fn node_candidates(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    row: &BindingRow,
    spec: &NodeWriteSpec,
    params: &BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<i64>, MemoryReservation)> {
    let mut memory = control.reserve(0)?;
    if let Some(candidates) =
        bound_node_candidates(snapshot, graph_view, row, spec, params, control)
    {
        return lease_candidate_nodes(candidates, control);
    }
    let first_label = first_static_label(connection, spec)?;
    if spec
        .labels
        .iter()
        .any(|label| matches!(label, WriteName::Static(_)))
        && first_label.is_none()
    {
        return Ok((Vec::new(), memory));
    }
    let mut after = 0;
    let mut output = Vec::new();
    loop {
        control.check()?;
        let page = match first_label {
            Some(label_id) => snapshot.scan_label_after(label_id, after, SCAN_BATCH)?,
            None => snapshot.scan_nodes_after(after, SCAN_BATCH)?,
        };
        for id in page.items {
            control.check()?;
            if node_matches(snapshot, graph_view, row, spec, id, params, control)? {
                reserve_entries(&mut output, 1, &mut memory)?;
                output.push(id);
            }
        }
        let Some(next) = page.next_after else {
            break;
        };
        after = next;
    }
    Ok((output, memory))
}

fn lease_candidate_nodes(
    candidates: QueryResult<Vec<i64>>,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<i64>, MemoryReservation)> {
    let candidates = candidates?;
    let memory = control.reserve(crate::query::resources::allocation_bytes::<i64>(
        candidates.capacity(),
    )?)?;
    Ok((candidates, memory))
}

fn first_static_label(connection: &Connection, spec: &NodeWriteSpec) -> QueryResult<Option<i64>> {
    let first_label = spec
        .labels
        .iter()
        .find_map(|label| match label {
            WriteName::Static(label) => Some(label),
            WriteName::Dynamic(_) => None,
        })
        .map(|label| storage::find_label(connection, label))
        .transpose()?
        .flatten();
    Ok(first_label)
}

fn bound_node_candidates(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    row: &BindingRow,
    spec: &NodeWriteSpec,
    params: &BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
) -> Option<QueryResult<Vec<i64>>> {
    let variable = spec.variable.as_ref()?;
    let binding = row.values.get(variable)?;
    Some(match binding {
        BindingValue::Node(id) => {
            node_matches(snapshot, graph_view, row, spec, *id, params, control)
                .map(|matches| if matches { vec![*id] } else { Vec::new() })
        }
        BindingValue::Null => Ok(Vec::new()),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("MERGE variable {variable} is not a Node"),
        )),
    })
}

fn node_matches(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    row: &BindingRow,
    spec: &NodeWriteSpec,
    id: i64,
    params: &BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let connection = snapshot.connection_for_query();
    if let Some(variable) = &spec.variable {
        match row.values.get(variable) {
            Some(BindingValue::Node(bound)) if *bound != id => return Ok(false),
            Some(BindingValue::Node(_)) | None => {}
            Some(BindingValue::Null) => return Ok(false),
            Some(_) => {
                return Err(QueryError::new(
                    QueryErrorKind::Type,
                    format!("MERGE variable {variable} is not a Node"),
                ));
            }
        }
    }
    if !snapshot.node_exists(id)? || !graph_view.visible_node_controlled(snapshot, id, control)? {
        return Ok(false);
    }
    let actual_labels = snapshot.labels(id)?;
    let (labels, _names_memory) = resolve_write_names(
        &spec.labels,
        snapshot,
        row,
        params,
        &graph_view.query_access,
        control,
    )?;
    for label in labels {
        let Some(label_id) = storage::find_label(connection, &label)? else {
            return Ok(false);
        };
        if actual_labels.binary_search(&label_id).is_err() {
            return Ok(false);
        }
    }
    let Some(properties) = &spec.properties else {
        return Ok(true);
    };
    let (expected, _map_memory) = evaluate_map(
        properties,
        snapshot,
        row,
        params,
        &graph_view.query_access,
        control,
    )?;
    properties_match(
        snapshot,
        OwnerKind::Node,
        id,
        &expected,
        &graph_view.query_access,
        control,
    )
}

fn relationship_properties_match(
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    spec: &RelationshipWriteSpec,
    id: i64,
    params: &BTreeMap<String, Value>,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let Some(properties) = &spec.properties else {
        return Ok(true);
    };
    let (expected, _map_memory) =
        evaluate_map(properties, snapshot, row, params, query_access, control)?;
    properties_match(
        snapshot,
        OwnerKind::Relationship,
        id,
        &expected,
        query_access,
        control,
    )
}

fn properties_match(
    snapshot: &Snapshot<'_>,
    owner_kind: OwnerKind,
    id: i64,
    expected: &BTreeMap<String, Value>,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let connection = snapshot.connection_for_query();
    for (key, expected) in expected {
        control.work(1)?;
        if query_access.denies(key) {
            return Ok(false);
        }
        let Some(key_id) = storage::find_property_key(connection, key)? else {
            return Ok(false);
        };
        let equal = snapshot.with_property_checked(
            owner_kind,
            id,
            key_id,
            &|| control.check().map_err(QueryError::into_storage_error),
            |actual| -> QueryResult<bool> {
                let Some(actual) = actual else {
                    return Ok(false);
                };
                let (actual, _memory) =
                    crate::query::graph::borrowed_property_with_control(actual, control)?;
                Ok(
                    expression::cypher_equals_controlled(&actual, expected, Some(control))?
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

#[cfg(test)]
mod tests;

fn bind_node(row: &mut BindingRow, variable: Option<&str>, id: i64) -> QueryResult<()> {
    let Some(variable) = variable else {
        return Ok(());
    };
    match row.values.get(variable) {
        Some(BindingValue::Node(existing)) if *existing == id => Ok(()),
        Some(BindingValue::Node(_) | BindingValue::Null) => Err(QueryError::internal(
            "pattern matcher attempted to overwrite an incompatible Node binding",
        )),
        Some(_) => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("variable {variable} is not a Node"),
        )),
        None => {
            row.insert(variable.to_owned(), BindingValue::Node(id));
            Ok(())
        }
    }
}

fn bind_relationship(
    row: &mut BindingRow,
    variable: Option<&str>,
    record: RelationshipRecord,
) -> QueryResult<()> {
    if let Some(variable) = variable {
        row.insert(variable.to_owned(), BindingValue::Relationship(record));
    }
    row.used_relationships.insert(record.id);
    Ok(())
}

fn clone_binding(
    row: &BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let cloned = crate::query::completeness::execute::RowSet::try_clone_rows(
        &[],
        std::slice::from_ref(row),
        control,
    )?;
    let memory = cloned
        .memory
        .ok_or_else(|| QueryError::internal("cloned binding is missing its memory lease"))?;
    let mut rows = cloned.rows;
    let row = rows
        .pop()
        .ok_or_else(|| QueryError::internal("cloned binding disappeared"))?;
    Ok((row, memory))
}

fn reserve_entries<T>(
    output: &mut Vec<T>,
    additional: usize,
    memory: &mut MemoryReservation,
) -> QueryResult<()> {
    let required = output
        .len()
        .checked_add(additional)
        .ok_or_else(|| crate::query::resources::memory_error("mutation result size overflow"))?;
    if required > output.capacity() {
        memory.grow(crate::query::resources::allocation_bytes::<T>(
            required - output.capacity(),
        )?)?;
        output.reserve_exact(required - output.len());
    }
    Ok(())
}
