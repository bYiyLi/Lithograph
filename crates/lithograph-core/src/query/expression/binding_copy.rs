//! Cooperative copies of owned bindings under a caller-held memory reservation.

use super::{BindingRow, BindingValue, LoadCsvContext, QueryResult};
use crate::query::resources::ExecutionControl;

pub(crate) fn clone_binding_value_unreserved_controlled(
    value: &BindingValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<BindingValue> {
    if let Some(control) = control {
        control.checkpoint(1)?;
    }
    Ok(match value {
        BindingValue::Scalar(value) => {
            BindingValue::Scalar(super::clone_value_unreserved_controlled(value, control)?)
        }
        BindingValue::Path {
            nodes,
            relationships,
        } => BindingValue::Path {
            nodes: super::controlled::clone_copy_slice_controlled(nodes, control)?,
            relationships: super::controlled::clone_copy_slice_controlled(relationships, control)?,
        },
        value => value.clone(),
    })
}

pub(crate) fn clone_load_csv_context_controlled(
    context: &Option<LoadCsvContext>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<LoadCsvContext>> {
    context
        .as_ref()
        .map(|context| {
            Ok(LoadCsvContext {
                file: context
                    .file
                    .as_ref()
                    .map(|file| super::clone_string_controlled(file, control))
                    .transpose()?,
                line: context.line,
            })
        })
        .transpose()
}

pub(crate) fn clone_binding_row_unreserved_controlled(
    row: &BindingRow,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<BindingRow> {
    let mut copied = BindingRow {
        order: Vec::with_capacity(row.order.len()),
        ..BindingRow::default()
    };
    for (name, value) in &row.values {
        if let Some(control) = control {
            control.checkpoint(1)?;
        }
        copied.values.insert(
            super::clone_string_controlled(name, control)?,
            clone_binding_value_unreserved_controlled(value, control)?,
        );
    }
    for name in &row.order {
        if let Some(control) = control {
            control.checkpoint(1)?;
        }
        copied
            .order
            .push(super::clone_string_controlled(name, control)?);
    }
    for id in &row.used_relationships {
        if let Some(control) = control {
            control.checkpoint(1)?;
        }
        copied.used_relationships.insert(*id);
    }
    copied.load_csv_context = clone_load_csv_context_controlled(&row.load_csv_context, control)?;
    Ok(copied)
}
