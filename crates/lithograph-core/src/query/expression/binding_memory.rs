use super::{BindingRow, BindingValue, QueryResult, RelationshipRecord};
use crate::query::resources::{
    ExecutionControl, allocation_bytes, checked_add, checked_mul, map_entry_bytes,
};

pub(crate) fn binding_row_bytes(row: &BindingRow) -> QueryResult<u64> {
    binding_row_bytes_controlled(row, None)
}

pub(crate) fn binding_row_bytes_controlled(
    row: &BindingRow,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    let mut bytes = allocation_bytes::<BindingRow>(1)?;
    for (key, value) in &row.values {
        if let Some(control) = control {
            control.work(1)?;
        }
        bytes = checked_add(
            bytes,
            checked_add(key.capacity() as u64, map_entry_bytes::<BindingValue>()?)?,
        )?;
        bytes = checked_add(bytes, binding_value_bytes(value, control)?)?;
    }
    checked_add(bytes, binding_metadata_bytes(row, control)?)
}

pub(crate) fn binding_value_bytes(
    value: &BindingValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    match value {
        BindingValue::Scalar(value) => super::value_bytes_with_control(value, control),
        BindingValue::Path {
            nodes,
            relationships,
        } => checked_add(
            allocation_bytes::<i64>(nodes.capacity())?,
            allocation_bytes::<RelationshipRecord>(relationships.capacity())?,
        ),
        _ => Ok(0),
    }
}

fn binding_metadata_bytes(
    row: &BindingRow,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    let mut bytes = allocation_bytes::<String>(row.order.capacity())?;
    for key in &row.order {
        if let Some(control) = control {
            control.work(1)?;
        }
        bytes = checked_add(bytes, key.capacity() as u64)?;
    }
    // A singleton BTreeSet already owns an entire leaf. Reuse the shared
    // conservative BTree node allowance instead of amortizing sparse sets.
    bytes = checked_add(
        bytes,
        checked_mul(
            row.used_relationships.len() as u64,
            map_entry_bytes::<i64>()?,
        )?,
    )?;
    if let Some(csv) = &row.load_csv_context
        && let Some(file) = &csv.file
    {
        bytes = checked_add(bytes, file.capacity() as u64)?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryErrorKind;
    use crate::query::resources::{MemoryReservation, QueryResources};

    fn clone_row(
        row: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<(BindingRow, MemoryReservation)> {
        let memory = control.reserve(binding_row_bytes_controlled(row, Some(control))?)?;
        let copied = super::super::clone_binding_row_unreserved_controlled(row, Some(control))?;
        Ok((copied, memory))
    }

    #[test]
    fn singleton_relationship_set_requires_a_whole_node_before_copying() {
        let mut row = BindingRow::default();
        row.used_relationships.insert(17);
        let former_amortized_limit = allocation_bytes::<BindingRow>(1).unwrap() + 40;
        let resources = QueryResources::new(Some(former_amortized_limit));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);

        let error = clone_row(&row, &control).unwrap_err();

        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
        assert_eq!(resources.peak(), 0);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn singleton_relationship_copy_holds_its_node_claim_until_release() {
        let mut row = BindingRow::default();
        row.used_relationships.insert(17);
        let required = binding_row_bytes(&row).unwrap();
        let resources = QueryResources::new(Some(required));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);

        let (copied, memory) = clone_row(&row, &control).unwrap();

        assert_eq!(copied.used_relationships, row.used_relationships);
        assert_eq!(resources.used(), required);
        assert!(resources.reserve(1).is_err());
        drop(copied);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }
}
