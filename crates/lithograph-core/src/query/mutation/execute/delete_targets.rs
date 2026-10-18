use super::*;

pub(super) fn delete_target_variables(expressions: &[Expr]) -> Vec<String> {
    (0..expressions.len())
        .map(|index| format!("__lithograph_delete_{index}"))
        .collect()
}

pub(super) fn evaluate_delete_targets(
    context: &MutationContext<'_, '_>,
    expressions: &[Expr],
    rows: &[BindingRow],
    state: &MutationClauseState<'_>,
) -> QueryResult<(Vec<String>, Vec<BindingRow>, MemoryReservation)> {
    let variable_bytes = crate::query::resources::checked_mul(expressions.len() as u64, 128)?;
    let mut memory = context
        .control
        .reserve(crate::query::resources::checked_add(
            variable_bytes,
            crate::query::resources::allocation_bytes::<BindingRow>(rows.len())?,
        )?)?;
    let variables = delete_target_variables(expressions);
    let mut targets = Vec::with_capacity(rows.len());
    for row in rows {
        let (target, target_memory) =
            evaluate_delete_target_row(context, expressions, &variables, row, &state.clause_input)?;
        memory.merge(target_memory)?;
        targets.push(target);
    }
    Ok((variables, targets, memory))
}

fn evaluate_delete_target_row(
    context: &MutationContext<'_, '_>,
    expressions: &[Expr],
    variables: &[String],
    row: &BindingRow,
    snapshot: &Snapshot<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let mut memory = context
        .control
        .reserve(crate::query::resources::checked_mul(
            expressions.len() as u64,
            512,
        )?)?;
    let mut targets = BindingRow::default();
    for (variable, expression) in variables.iter().zip(expressions) {
        let evaluated = expression::evaluate_with_access_control(
            expression,
            snapshot,
            row,
            context.params,
            &context.access.read.query,
            &context.control,
        )?;
        memory.merge(evaluated.reservation)?;
        targets.insert(
            variable.clone(),
            expression::binding_from_value_controlled(snapshot, evaluated.value, &context.control)?,
        );
    }
    memory.resize(expression::binding_row_bytes(&targets)?)?;
    Ok((targets, memory))
}
