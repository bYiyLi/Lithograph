//! Borrowed canonical reads with checked ownership of runtime graph values.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::rc::Rc;

use super::super::resources::{
    ExecutionControl, MemoryReservation, QueryResources, allocation_bytes, checked_add,
    checked_mul, map_entry_bytes, value_bytes_controlled,
};
use super::{PropertyRules, QueryError, QueryResult, property_value, snapshot_connection};
use crate::cypher::{NodeValue, RelationshipValue, Value};
use crate::storage::{self, OwnerKind, RelationshipRecord, Snapshot, StorageError, StorageResult};

#[derive(Debug)]
struct QuerySnapshotObserver(QueryResources);

impl storage::SnapshotReadObserver for QuerySnapshotObserver {
    fn reserve(&self, bytes: u64) -> StorageResult<Box<dyn Debug>> {
        self.0
            .reserve(bytes)
            .map(|lease| Box::new(lease) as Box<dyn Debug>)
            .map_err(storage_error)
    }

    fn checkpoint(&self) -> StorageResult<()> {
        // The retained observer owns no borrowed host callback. Each observed
        // operation receives its current callback separately through *_checked.
        Ok(())
    }
}

pub(crate) fn snapshot_observer(
    resources: &QueryResources,
) -> Rc<dyn storage::SnapshotReadObserver> {
    Rc::new(QuerySnapshotObserver(resources.clone()))
}

fn storage_error(error: QueryError) -> StorageError {
    error.into_storage_error()
}

pub(crate) fn resolve_snapshot_with_control<'connection>(
    connection: &'connection rusqlite::Connection,
    commit: storage::HashId,
    control: &ExecutionControl<'_>,
) -> QueryResult<Snapshot<'connection>> {
    let check = || control.check().map_err(storage_error);
    Snapshot::resolve_observed(
        connection,
        commit,
        snapshot_observer(control.resources()),
        &check,
    )
    .map_err(Into::into)
}

pub(crate) fn resolve_snapshot_with_layer_and_schema_controlled<'connection>(
    connection: &'connection rusqlite::Connection,
    commit: storage::HashId,
    layer: &storage::LayerBuilder,
    schema: storage::SchemaState,
    control: &ExecutionControl<'_>,
) -> QueryResult<Snapshot<'connection>> {
    let check = || control.check().map_err(storage_error);
    Snapshot::resolve_with_layer_and_schema_observed(
        connection,
        commit,
        layer,
        schema,
        snapshot_observer(control.resources()),
        &check,
    )
    .map_err(Into::into)
}

pub(crate) fn materialize_node_with_control(
    snapshot: &Snapshot<'_>,
    node_id: i64,
    access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(NodeValue, MemoryReservation)> {
    control.check()?;
    if !snapshot.node_exists(node_id)? {
        return Err(QueryError::semantic(format!(
            "Node n:{node_id} is no longer present in the active graph state"
        )));
    }
    let mut memory = control.reserve(checked_add(std::mem::size_of::<Value>() as u64, 32)?)?;
    let labels = node_labels(snapshot, node_id, control, &mut memory)?;
    let properties = properties(
        snapshot,
        OwnerKind::Node,
        node_id,
        access,
        control,
        &mut memory,
    )?;
    let value = Value::Node(NodeValue {
        element_id: format!("n:{node_id}"),
        labels,
        properties,
    });
    memory.resize(value_bytes_controlled(&value, control)?)?;
    let Value::Node(value) = value else {
        unreachable!()
    };
    Ok((value, memory))
}

fn label_count(
    snapshot: &Snapshot<'_>,
    node_id: i64,
    control: &ExecutionControl<'_>,
) -> QueryResult<usize> {
    let mut count = 0usize;
    let check = || control.check().map_err(storage_error);
    snapshot.visit_node_labels_checked(node_id, &check, |_| {
        control.work(1)?;
        count = count
            .checked_add(1)
            .ok_or_else(|| super::super::resources::memory_error("node label count overflow"))?;
        Ok::<_, QueryError>(())
    })?;
    Ok(count)
}

fn node_labels(
    snapshot: &Snapshot<'_>,
    node_id: i64,
    control: &ExecutionControl<'_>,
    memory: &mut MemoryReservation,
) -> QueryResult<Vec<String>> {
    let count = label_count(snapshot, node_id, control)?;
    memory.grow(allocation_bytes::<String>(count)?)?;
    let mut labels = Vec::with_capacity(count);
    let check = || control.check().map_err(storage_error);
    snapshot.visit_node_labels_checked(node_id, &check, |id| {
        control.work(1)?;
        storage::with_label_name(snapshot_connection(snapshot), id, |name| {
            let name =
                name.ok_or_else(|| QueryError::internal("Label dictionary entry is missing"))?;
            memory.grow(name.len() as u64)?;
            labels.push(copy_text(name, control)?);
            Ok::<_, QueryError>(())
        })
    })?;
    Ok(labels)
}

pub(crate) fn materialize_relationship_with_control(
    snapshot: &Snapshot<'_>,
    record: RelationshipRecord,
    access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(RelationshipValue, MemoryReservation)> {
    control.check()?;
    if snapshot.relationship(record.id)?.is_none() {
        return Err(QueryError::semantic(format!(
            "Relationship r:{} is no longer present in the active graph state",
            record.id
        )));
    }
    let mut memory = control.reserve(checked_add(std::mem::size_of::<Value>() as u64, 96)?)?;
    let relationship_type = storage::with_relationship_type_name(
        snapshot_connection(snapshot),
        record.type_id,
        |name| {
            let name = name.ok_or_else(|| {
                QueryError::internal("Relationship Type dictionary entry is missing")
            })?;
            memory.grow(name.len() as u64)?;
            copy_text(name, control)
        },
    )?;
    let properties = properties(
        snapshot,
        OwnerKind::Relationship,
        record.id,
        access,
        control,
        &mut memory,
    )?;
    let value = Value::Relationship(RelationshipValue {
        element_id: format!("r:{}", record.id),
        relationship_type,
        start: format!("n:{}", record.source),
        end: format!("n:{}", record.target),
        properties,
    });
    memory.resize(value_bytes_controlled(&value, control)?)?;
    let Value::Relationship(value) = value else {
        unreachable!()
    };
    Ok((value, memory))
}

pub(crate) fn node_property_with_control(
    snapshot: &Snapshot<'_>,
    id: i64,
    key: &str,
    access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Value, MemoryReservation)> {
    control.check()?;
    if !snapshot.node_exists(id)? {
        return Err(QueryError::semantic(format!(
            "Node n:{id} is no longer present in the active graph state"
        )));
    }
    element_property(snapshot, OwnerKind::Node, id, key, access, control)
}

pub(crate) fn relationship_property_with_control(
    snapshot: &Snapshot<'_>,
    id: i64,
    key: &str,
    access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Value, MemoryReservation)> {
    control.check()?;
    if snapshot.relationship(id)?.is_none() {
        return Err(QueryError::semantic(format!(
            "Relationship r:{id} is no longer present in the active graph state"
        )));
    }
    element_property(snapshot, OwnerKind::Relationship, id, key, access, control)
}

fn properties(
    snapshot: &Snapshot<'_>,
    kind: OwnerKind,
    id: i64,
    access: &PropertyRules,
    control: &ExecutionControl<'_>,
    memory: &mut MemoryReservation,
) -> QueryResult<BTreeMap<String, Value>> {
    let mut values = BTreeMap::new();
    let check = || control.check().map_err(storage_error);
    snapshot.visit_owner_properties_checked(kind, id, &check, |key, property| {
        control.work(1)?;
        storage::with_property_key_name(snapshot_connection(snapshot), key, |name| {
            let name =
                name.ok_or_else(|| QueryError::internal("Property dictionary entry is missing"))?;
            if access.denies(name) {
                return Ok(());
            }
            let mut entry_memory =
                control.reserve(checked_add(map_entry_bytes::<Value>()?, name.len() as u64)?)?;
            let name = copy_text(name, control)?;
            let (value, mut value_memory) = borrowed_property(property, control)?;
            values.insert(name, value);
            // The tree-node allowance already owns the inserted Value slot.
            value_memory.resize(value_memory.bytes() - std::mem::size_of::<Value>() as u64)?;
            entry_memory.merge(value_memory)?;
            memory.merge(entry_memory)?;
            Ok::<_, QueryError>(())
        })
    })?;
    Ok(values)
}

fn element_property(
    snapshot: &Snapshot<'_>,
    kind: OwnerKind,
    id: i64,
    key: &str,
    access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Value, MemoryReservation)> {
    if access.denies(key) {
        return Ok((
            Value::Null,
            control.reserve(std::mem::size_of::<Value>() as u64)?,
        ));
    }
    let key_id = storage::find_property_key(snapshot_connection(snapshot), key)?;
    let Some(key_id) = key_id else {
        return Ok((
            Value::Null,
            control.reserve(std::mem::size_of::<Value>() as u64)?,
        ));
    };
    let check = || control.check().map_err(storage_error);
    snapshot.with_property_checked(kind, id, key_id, &check, |value| match value {
        Some(value) => borrowed_property(value, control),
        None => Ok((
            Value::Null,
            control.reserve(std::mem::size_of::<Value>() as u64)?,
        )),
    })
}

pub(crate) fn borrowed_property(
    value: &storage::PropertyValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Value, MemoryReservation)> {
    control.work(1)?;
    match value {
        storage::PropertyValue::String(value) => {
            let memory = control.reserve(checked_add(
                std::mem::size_of::<Value>() as u64,
                value.len() as u64,
            )?)?;
            Ok((Value::String(copy_text(value, control)?), memory))
        }
        storage::PropertyValue::List(values) => {
            let initial = checked_add(
                std::mem::size_of::<Value>() as u64,
                allocation_bytes::<Value>(values.len())?,
            )?;
            let mut memory = control.reserve(initial)?;
            let mut output = Vec::with_capacity(values.len());
            for value in values {
                let (value, mut child_memory) = borrowed_property(value, control)?;
                output.push(value);
                // Parent Vec capacity already accounts for this inline slot.
                child_memory.resize(child_memory.bytes() - std::mem::size_of::<Value>() as u64)?;
                memory.merge(child_memory)?;
            }
            Ok((Value::List(output), memory))
        }
        _ => borrowed_scalar_property(value, control),
    }
}

fn borrowed_scalar_property(
    value: &storage::PropertyValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Value, MemoryReservation)> {
    // The scalar formats use bounded text temporaries. Vector/Point account for
    // the owned storage copy and converted coordinates being alive together.
    let mut memory = control.reserve(checked_add(
        checked_add(std::mem::size_of::<Value>() as u64, 1_024)?,
        scalar_extra_bytes(value)?,
    )?)?;
    control.check()?;
    let result = match value {
        storage::PropertyValue::Vector(vector) => borrowed_vector(vector, control)?,
        _ => property_value(value.clone())?,
    };
    control.check()?;
    memory.resize(value_bytes_controlled(&result, control)?)?;
    Ok((result, memory))
}

fn scalar_extra_bytes(value: &storage::PropertyValue) -> QueryResult<u64> {
    match value {
        storage::PropertyValue::Vector(value) => checked_mul(value.packed.len() as u64, 2),
        storage::PropertyValue::Point(value) => {
            checked_mul(allocation_bytes::<f64>(value.coordinates.len())?, 2)
        }
        storage::PropertyValue::ZonedDateTime(value) => checked_mul(value.zone_id.len() as u64, 3),
        _ => Ok(0),
    }
}

fn copy_text(value: &str, control: &ExecutionControl<'_>) -> QueryResult<String> {
    let mut output = String::with_capacity(value.len());
    let mut start = 0;
    while start < value.len() {
        let mut end = value.len().min(start.saturating_add(64 * 1024));
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        control.bytes((end - start) as u64)?;
        output.push_str(&value[start..end]);
        start = end;
    }
    Ok(output)
}

fn borrowed_vector(
    value: &storage::VectorValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    use crate::cypher::{VectorCoordinateType as Ct, VectorValue as Cv, VectorValues as Vs};
    use storage::VectorCoordinateType as St;
    let (coordinate_type, values) = match value.coordinate_type {
        St::I8 => (
            Ct::I8,
            Vs::I8(vector_coordinates::<1, _>(
                &value.packed,
                control,
                |bytes| bytes[0] as i8,
            )?),
        ),
        St::I16 => (
            Ct::I16,
            Vs::I16(vector_coordinates(
                &value.packed,
                control,
                i16::from_le_bytes,
            )?),
        ),
        St::I32 => (
            Ct::I32,
            Vs::I32(vector_coordinates(
                &value.packed,
                control,
                i32::from_le_bytes,
            )?),
        ),
        St::I64 => (
            Ct::I64,
            Vs::I64(vector_coordinates(
                &value.packed,
                control,
                i64::from_le_bytes,
            )?),
        ),
        St::F32 => (
            Ct::F32,
            Vs::F32(vector_coordinates(
                &value.packed,
                control,
                f32::from_le_bytes,
            )?),
        ),
        St::F64 => (
            Ct::F64,
            Vs::F64(vector_coordinates(
                &value.packed,
                control,
                f64::from_le_bytes,
            )?),
        ),
    };
    Ok(Value::Vector(Cv::new(coordinate_type, values)?))
}

fn vector_coordinates<const WIDTH: usize, T>(
    bytes: &[u8],
    control: &ExecutionControl<'_>,
    convert: impl Fn([u8; WIDTH]) -> T,
) -> QueryResult<Vec<T>> {
    let chunks = bytes.as_chunks::<WIDTH>().0;
    let mut values = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        control.work(1)?;
        values.push(convert(*chunk));
    }
    Ok(values)
}

#[cfg(test)]
mod tests;
