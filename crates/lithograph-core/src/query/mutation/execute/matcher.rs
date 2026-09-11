use super::*;

pub(super) fn match_pattern(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    seed: &BindingRow,
    pattern: &WritePatternPart,
    params: &BTreeMap<String, Value>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let mut seed = seed.clone();
    seed.used_relationships.clear();
    let matcher = PathMatchContext {
        connection,
        snapshot,
        graph_view,
        pattern,
        params,
        is_interrupted,
    };
    let starts = node_candidates(
        connection,
        snapshot,
        graph_view,
        &seed,
        &pattern.nodes[0],
        params,
        is_interrupted,
    )?;
    let mut output = Vec::new();
    for start in starts {
        check_interrupted(is_interrupted)?;
        let mut row = seed.clone();
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
    Ok(output)
}

struct PathMatchContext<'a, 'snapshot> {
    connection: &'a Connection,
    snapshot: &'a Snapshot<'snapshot>,
    graph_view: &'a ResolvedGraphView,
    pattern: &'a WritePatternPart,
    params: &'a BTreeMap<String, Value>,
    is_interrupted: &'a dyn Fn() -> bool,
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
        output.push(complete_path(context.pattern, row, nodes, relationships));
        return Ok(());
    }
    let rel_spec = &context.pattern.relationships[index];
    for relationship in path_relationship_candidates(context, current, rel_spec)? {
        check_interrupted(context.is_interrupted)?;
        if !relationship_is_usable(context, &row, rel_spec, relationship)? {
            continue;
        }
        if !relationship_binding_matches(&row, rel_spec, relationship) {
            continue;
        }
        let next = match rel_spec.direction {
            Direction::Outgoing => relationship.target,
            Direction::Incoming => relationship.source,
            Direction::Undirected => unreachable!(),
        };
        let mut next_row = row.clone();
        if !node_matches(
            context.connection,
            context.snapshot,
            context.graph_view,
            &next_row,
            &context.pattern.nodes[index + 1],
            next,
            context.params,
        )? {
            continue;
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
    }
    Ok(())
}

fn complete_path(
    pattern: &WritePatternPart,
    mut row: BindingRow,
    nodes: &[i64],
    relationships: &[RelationshipRecord],
) -> BindingRow {
    if let Some(variable) = &pattern.path_variable {
        row.values.insert(
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
) -> QueryResult<Vec<RelationshipRecord>> {
    let Some(type_id) =
        storage::find_relationship_type(context.connection, &spec.relationship_type)?
    else {
        return Ok(Vec::new());
    };
    let mut after = 0;
    let mut output = Vec::new();
    loop {
        check_interrupted(context.is_interrupted)?;
        let page = match spec.direction {
            Direction::Outgoing => {
                context
                    .snapshot
                    .scan_outgoing_after(current, Some(type_id), after, SCAN_BATCH)?
            }
            Direction::Incoming => {
                context
                    .snapshot
                    .scan_incoming_after(current, Some(type_id), after, SCAN_BATCH)?
            }
            Direction::Undirected => return Ok(Vec::new()),
        };
        output.extend(page.items);
        let Some(next) = page.next_after else {
            return Ok(output);
        };
        after = next;
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
    if !context
        .graph_view
        .visible_relationship(context.snapshot, relationship)?
    {
        return Ok(false);
    }
    relationship_properties_match(context.snapshot, row, spec, relationship.id, context.params)
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
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<i64>> {
    if let Some(candidates) =
        bound_node_candidates(connection, snapshot, graph_view, row, spec, params)
    {
        return candidates;
    }
    let first_label = spec
        .labels
        .first()
        .map(|label| storage::find_label(connection, label))
        .transpose()?
        .flatten();
    if !spec.labels.is_empty() && first_label.is_none() {
        return Ok(Vec::new());
    }
    let mut after = 0;
    let mut output = Vec::new();
    loop {
        check_interrupted(is_interrupted)?;
        let page = match first_label {
            Some(label_id) => snapshot.scan_label_after(label_id, after, SCAN_BATCH)?,
            None => snapshot.scan_nodes_after(after, SCAN_BATCH)?,
        };
        for id in page.items {
            check_interrupted(is_interrupted)?;
            if node_matches(connection, snapshot, graph_view, row, spec, id, params)? {
                output.push(id);
            }
        }
        let Some(next) = page.next_after else {
            break;
        };
        after = next;
    }
    Ok(output)
}

fn bound_node_candidates(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    row: &BindingRow,
    spec: &NodeWriteSpec,
    params: &BTreeMap<String, Value>,
) -> Option<QueryResult<Vec<i64>>> {
    let variable = spec.variable.as_ref()?;
    let binding = row.values.get(variable)?;
    Some(match binding {
        BindingValue::Node(id) => {
            node_matches(connection, snapshot, graph_view, row, spec, *id, params)
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
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    row: &BindingRow,
    spec: &NodeWriteSpec,
    id: i64,
    params: &BTreeMap<String, Value>,
) -> QueryResult<bool> {
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
    if !snapshot.node_exists(id)? || !graph_view.visible_node(snapshot, id)? {
        return Ok(false);
    }
    let labels = snapshot.labels(id)?;
    for label in &spec.labels {
        let Some(label_id) = storage::find_label(connection, label)? else {
            return Ok(false);
        };
        if labels.binary_search(&label_id).is_err() {
            return Ok(false);
        }
    }
    let Some(properties) = &spec.properties else {
        return Ok(true);
    };
    let expected = evaluate_map(properties, snapshot, row, params)?;
    properties_match(connection, snapshot, OwnerKind::Node, id, &expected)
}

fn relationship_properties_match(
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    spec: &RelationshipWriteSpec,
    id: i64,
    params: &BTreeMap<String, Value>,
) -> QueryResult<bool> {
    let Some(properties) = &spec.properties else {
        return Ok(true);
    };
    let expected = evaluate_map(properties, snapshot, row, params)?;
    properties_match(
        snapshot.connection_for_query(),
        snapshot,
        OwnerKind::Relationship,
        id,
        &expected,
    )
}

fn properties_match(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    owner_kind: OwnerKind,
    id: i64,
    expected: &BTreeMap<String, Value>,
) -> QueryResult<bool> {
    for (key, expected) in expected {
        let Some(key_id) = storage::find_property_key(connection, key)? else {
            return Ok(false);
        };
        let Some(actual) = snapshot.property(owner_kind, id, key_id)? else {
            return Ok(matches!(expected, Value::Null));
        };
        let actual = property_value(actual)?;
        if crate::cypher::cypher_equals(&actual, expected)? != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

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
            row.values
                .insert(variable.to_owned(), BindingValue::Node(id));
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
        row.values
            .insert(variable.to_owned(), BindingValue::Relationship(record));
    }
    row.used_relationships.insert(record.id);
    Ok(())
}
