//! The two total projection forms retained on factor packets. Values are only
//! materialized at an actual consumer boundary, borrowing one reusable row.

use std::collections::BTreeMap;

use super::{GroupPacket, GroupProjection};
use crate::cypher::Value;
use crate::query::access::PropertyRules;
use crate::query::expression::{self, BindingRow, BindingValue, Expr};
use crate::query::plan::Projection;
use crate::query::resources::{allocation_bytes, checked_add, map_entry_bytes};
use crate::query::{ExecutionControl, MemoryReservation, QueryError, QueryResult};
use crate::storage::Snapshot;

#[cfg(test)]
mod tests;

pub(crate) fn compile_projection(
    packet: &GroupPacket,
    projections: &[Projection],
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(Vec<GroupProjection>, MemoryReservation)>> {
    // Reject unsupported/error-bearing expressions before allocating optional
    // descriptions: a failed grant must not replace their original Type error.
    for projection in projections {
        control.work(1)?;
        let Some((variable, element_id)) = projection_input(&projection.expression) else {
            return Ok(None);
        };
        if resolve_input(packet, variable, element_id).is_none() {
            return Ok(None);
        }
    }
    let mut memory = control.reserve(allocation_bytes::<GroupProjection>(projections.len())?)?;
    let mut output = Vec::new();
    output.try_reserve_exact(projections.len()).map_err(|_| {
        crate::query::resources::memory_error("unable to allocate factor projection")
    })?;
    for projection in projections {
        control.work(1)?;
        let Some((variable, element_id)) = projection_input(&projection.expression) else {
            return Ok(None);
        };
        let Some((name, element_id)) = resolve_input(packet, variable, element_id) else {
            return Ok(None);
        };
        let bytes = checked_add(name.len() as u64, projection.column.len() as u64)?;
        control.bytes(bytes)?;
        memory.grow(bytes)?;
        let name = expression::clone_string_controlled(name, Some(control))?;
        let output_name = expression::clone_string_controlled(&projection.column, Some(control))?;
        output.push(if element_id {
            GroupProjection::ElementId {
                name,
                output: output_name,
            }
        } else {
            GroupProjection::Slot {
                name,
                output: output_name,
            }
        });
    }
    Ok(Some((output, memory)))
}

fn projection_input(expression: &Expr) -> Option<(&str, bool)> {
    match expression {
        Expr::Variable(name) => Some((name, false)),
        Expr::Function {
            name,
            args,
            distinct: false,
            star: false,
        } if name.eq_ignore_ascii_case("elementId") => match args.as_slice() {
            [Expr::Variable(name)] => Some((name, true)),
            _ => None,
        },
        _ => None,
    }
}

fn resolve_input<'a>(
    packet: &'a GroupPacket,
    variable: &'a str,
    element_id: bool,
) -> Option<(&'a str, bool)> {
    let (source, already_element_id) = match packet.projection.as_deref() {
        Some(projections) => match projections.iter().find(|item| item.names().1 == variable)? {
            GroupProjection::Slot { name, .. } => (name.as_str(), false),
            GroupProjection::ElementId { name, .. } => (name.as_str(), true),
        },
        None => (variable, false),
    };
    if !packet.table.prefix().values.contains_key(source)
        && !packet
            .plan
            .as_ref()
            .is_some_and(|plan| plan.bindings.iter().any(|binding| binding.name == source))
    {
        return None;
    }
    // Renames are composed against the old input, never an evolving output.
    // elementId(elementId(entity)) is not total and must retain flat evaluation.
    if element_id && (already_element_id || !entity_slot(packet, source)) {
        return None;
    }
    Some((source, element_id || already_element_id))
}

fn entity_slot(packet: &GroupPacket, name: &str) -> bool {
    packet
        .plan
        .as_ref()
        .is_some_and(|plan| plan.bindings.iter().any(|binding| binding.name == name))
        || matches!(
            packet.table.prefix().values.get(name),
            Some(BindingValue::Node(_) | BindingValue::Relationship(_) | BindingValue::Null)
                | Some(BindingValue::Scalar(Value::Null))
        )
}

pub(crate) fn project_values(
    projections: &[GroupProjection],
    row: &BindingRow,
    snapshot: &Snapshot<'_>,
    _params: &BTreeMap<String, Value>,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<Value>, MemoryReservation)> {
    let mut memory = control.reserve(allocation_bytes::<Value>(projections.len())?)?;
    let mut values = Vec::new();
    values.try_reserve_exact(projections.len()).map_err(|_| {
        crate::query::resources::memory_error("unable to allocate factor output values")
    })?;
    for projection in projections {
        control.work(1)?;
        let evaluated = projection_value(projection, row, snapshot, query_access, control)?;
        memory.merge(evaluated.reservation)?;
        values.push(evaluated.value);
    }
    Ok((values, memory))
}

fn projection_value(
    projection: &GroupProjection,
    row: &BindingRow,
    snapshot: &Snapshot<'_>,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<expression::EvaluatedValue> {
    let (name, _) = projection.names();
    let value = row.values.get(name).unwrap_or(&BindingValue::Null);
    match (projection, value) {
        (GroupProjection::ElementId { .. }, BindingValue::Node(id)) => {
            expression::metadata_element_id("n", *id, control)
        }
        (GroupProjection::ElementId { .. }, BindingValue::Relationship(record)) => {
            expression::metadata_element_id("r", record.id, control)
        }
        (
            GroupProjection::ElementId { .. },
            BindingValue::Null | BindingValue::Scalar(Value::Null),
        )
        | (GroupProjection::Slot { .. }, _) => {
            expression::binding_value_with_memory(snapshot, value, query_access, control)
        }
        _ => Err(QueryError::internal(
            "factor elementId lost its entity binding proof",
        )),
    }
}

pub(crate) fn project_binding(
    projections: &[GroupProjection],
    row: &BindingRow,
    snapshot: &Snapshot<'_>,
    _params: &BTreeMap<String, Value>,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let mut memory = control.reserve(allocation_bytes::<BindingRow>(1)?)?;
    let mut output = BindingRow::default();
    if let Some(context) = &row.load_csv_context {
        memory.grow(context.file.as_ref().map_or(0, |file| file.len()) as u64)?;
        output.load_csv_context =
            expression::clone_load_csv_context_controlled(&row.load_csv_context, Some(control))?;
    }
    for projection in projections {
        control.work(1)?;
        let (_, column) = projection.names();
        let value = binding_projection_value(
            projection,
            row,
            snapshot,
            query_access,
            control,
            &mut memory,
        )?;
        let names = checked_add(
            allocation_bytes::<String>(2)?,
            (column.len() as u64).saturating_mul(2),
        )?;
        memory.grow(checked_add(map_entry_bytes::<BindingValue>()?, names)?)?;
        output.insert(
            expression::clone_string_controlled(column, Some(control))?,
            value,
        );
    }
    Ok((output, memory))
}

fn binding_projection_value(
    projection: &GroupProjection,
    row: &BindingRow,
    snapshot: &Snapshot<'_>,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
    memory: &mut MemoryReservation,
) -> QueryResult<BindingValue> {
    match projection {
        GroupProjection::Slot { name, .. } => {
            let value = row.values.get(name).unwrap_or(&BindingValue::Null);
            memory.grow(expression::binding_value_bytes(value, Some(control))?)?;
            expression::clone_binding_value_unreserved_controlled(value, Some(control))
        }
        GroupProjection::ElementId { .. } => {
            let evaluated = projection_value(projection, row, snapshot, query_access, control)?;
            memory.merge(evaluated.reservation)?;
            expression::binding_from_value_controlled(snapshot, evaluated.value, control)
        }
    }
}
