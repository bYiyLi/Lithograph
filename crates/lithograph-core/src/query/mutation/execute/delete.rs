use super::*;

pub(super) fn apply_delete(
    context: &mut MutationContext<'_, '_>,
    rows: &[BindingRow],
    variables: &[String],
    detach: bool,
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    for relationship_pass in [true, false] {
        for row in rows {
            for variable in variables
                .iter()
                .filter(|variable| is_relationship_binding(row, variable) == relationship_pass)
            {
                check_interrupted(is_interrupted)?;
                apply_delete_value(
                    context,
                    row,
                    variable,
                    detach,
                    clause_input,
                    graph_view,
                    is_interrupted,
                )?;
            }
        }
    }
    Ok(())
}

fn is_relationship_binding(row: &BindingRow, variable: &str) -> bool {
    matches!(
        row.values.get(variable),
        Some(BindingValue::Relationship(_))
    )
}

fn apply_delete_value(
    context: &mut MutationContext<'_, '_>,
    row: &BindingRow,
    variable: &str,
    detach: bool,
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let staged = context.staged_snapshot()?;
    match row.values.get(variable) {
        Some(BindingValue::Null) | None => Ok(()),
        Some(BindingValue::Node(id)) => delete_bound_node(
            context,
            clause_input,
            &staged,
            *id,
            detach,
            graph_view,
            is_interrupted,
        ),
        Some(BindingValue::Relationship(record)) => delete_bound_relationship(
            context,
            clause_input,
            &staged,
            *record,
            graph_view,
            is_interrupted,
        ),
        Some(BindingValue::Path { .. }) => Err(QueryError::new(
            QueryErrorKind::Type,
            "DELETE requires Node, Relationship, or null values",
        )),
    }
}

fn delete_bound_node(
    context: &mut MutationContext<'_, '_>,
    clause_input: &Snapshot<'_>,
    staged: &Snapshot<'_>,
    id: i64,
    detach: bool,
    graph_view: &ResolvedGraphView,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    require_visible_delete_node(clause_input, staged, id, graph_view)?;
    if !staged.node_exists(id)? {
        return Ok(());
    }
    delete_incident_relationships(
        context,
        clause_input,
        staged,
        id,
        detach,
        graph_view,
        is_interrupted,
    )?;
    delete_node(context, staged, id, is_interrupted)
}

fn require_visible_delete_node(
    clause_input: &Snapshot<'_>,
    staged: &Snapshot<'_>,
    id: i64,
    graph_view: &ResolvedGraphView,
) -> QueryResult<()> {
    if clause_input.node_exists(id)? && !graph_view.visible_node(clause_input, id)? {
        return Err(QueryError::graph_view_violation(format!(
            "DELETE cannot target Node {id} outside the active Graph View"
        )));
    }
    if !clause_input.node_exists(id)? && !graph_view.visible_node(staged, id)? {
        return Err(QueryError::graph_view_violation(format!(
            "DELETE cannot target Node {id} outside the active Graph View"
        )));
    }
    Ok(())
}

fn delete_incident_relationships(
    context: &mut MutationContext<'_, '_>,
    clause_input: &Snapshot<'_>,
    staged: &Snapshot<'_>,
    id: i64,
    detach: bool,
    graph_view: &ResolvedGraphView,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let incident = all_incident(staged, id, is_interrupted)?;
    if !incident.is_empty() && !detach {
        return Err(QueryError::constraint(format!(
            "cannot DELETE Node {id} while Relationships still reference it; use DETACH DELETE"
        )));
    }
    for relationship in incident {
        check_interrupted(is_interrupted)?;
        let visible = match clause_input.relationship(relationship.id)? {
            Some(input_record) => graph_view.visible_relationship(clause_input, input_record)?,
            None => graph_view.visible_relationship(staged, relationship)?,
        };
        if !visible {
            return Err(QueryError::graph_view_violation(format!(
                "deleting Node {id} would implicitly delete Relationship {} outside the active Graph View",
                relationship.id
            )));
        }
        delete_relationship(context, staged, relationship, is_interrupted)?;
    }
    Ok(())
}

fn delete_bound_relationship(
    context: &mut MutationContext<'_, '_>,
    clause_input: &Snapshot<'_>,
    staged: &Snapshot<'_>,
    record: RelationshipRecord,
    graph_view: &ResolvedGraphView,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    if let Some(input_record) = clause_input.relationship(record.id)?
        && !graph_view.visible_relationship(clause_input, input_record)?
    {
        return Err(QueryError::graph_view_violation(format!(
            "DELETE cannot target Relationship {} outside the active Graph View",
            record.id
        )));
    }
    let Some(current) = staged.relationship(record.id)? else {
        return Ok(());
    };
    if clause_input.relationship(record.id)?.is_none()
        && !graph_view.visible_relationship(staged, current)?
    {
        return Err(QueryError::graph_view_violation(format!(
            "DELETE cannot target Relationship {} outside the active Graph View",
            record.id
        )));
    }
    delete_relationship(context, staged, current, is_interrupted)
}

fn delete_node(
    context: &mut MutationContext<'_, '_>,
    staged: &Snapshot<'_>,
    id: i64,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    for label_id in staged.labels(id)? {
        check_interrupted(is_interrupted)?;
        context
            .delta
            .set_label(&context.base, id, label_id, false)?;
    }
    for (key_id, _) in staged.properties(OwnerKind::Node, id)? {
        check_interrupted(is_interrupted)?;
        context
            .delta
            .set_property(&context.base, OwnerKind::Node, id, key_id, None)?;
    }
    if staged.node_exists(id)? {
        context.delta.set_node(&context.base, id, false)?;
    }
    Ok(())
}

fn delete_relationship(
    context: &mut MutationContext<'_, '_>,
    staged: &Snapshot<'_>,
    record: RelationshipRecord,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    for (key_id, _) in staged.properties(OwnerKind::Relationship, record.id)? {
        check_interrupted(is_interrupted)?;
        context.delta.set_property(
            &context.base,
            OwnerKind::Relationship,
            record.id,
            key_id,
            None,
        )?;
    }
    if staged.relationship(record.id)?.is_some() {
        context
            .delta
            .set_relationship(&context.base, record.id, None)?;
    }
    Ok(())
}

fn all_incident(
    snapshot: &Snapshot<'_>,
    node_id: i64,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<RelationshipRecord>> {
    let mut after = 0;
    let mut output = Vec::new();
    loop {
        check_interrupted(is_interrupted)?;
        let page = snapshot.scan_incident_after(node_id, None, after, SCAN_BATCH)?;
        output.extend(page.items);
        let Some(next) = page.next_after else {
            break;
        };
        after = next;
    }
    Ok(output)
}
