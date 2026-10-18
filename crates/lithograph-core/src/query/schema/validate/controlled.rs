//! Schema DDL validation over observed, borrowed Snapshot state.

use super::*;
use crate::query::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, checked_mul, memory_error,
};
use crate::storage::{PropertyType, RelationshipRecord, VectorCoordinateType};

mod conflicts;
pub(crate) use conflicts::validation_conflicts_controlled;

mod affected;
pub(crate) use affected::validate_layer_against_commit_schema_controlled;

const PAGE_SIZE: usize = 64;
// Bound the base/overlay vectors, merge tree nodes and returned page together.
// A fixed small page avoids a graph-sized preflight or retained corpus copy.
const PAGE_ITEM_BYTES: u64 = 1_024;

pub(crate) fn validate_schema_transition_controlled(
    connection: &Connection,
    previous: &SchemaState,
    next: &SchemaState,
    snapshot: &Snapshot<'_>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let validation = Validation {
        connection,
        snapshot,
        control,
    };
    validation.changed_nodes(previous, next)?;
    validation.changed_relationships(previous, next)?;
    validation.changed_constraints(previous, next)?;
    control.check()
}

fn changed<T>(
    previous: Option<&T>,
    same: impl FnOnce(&T) -> QueryResult<bool>,
) -> QueryResult<bool> {
    match previous {
        Some(previous) => same(previous).map(|same| !same),
        None => Ok(true),
    }
}

fn same_node(
    left: &GraphNodeType,
    right: &GraphNodeType,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    Ok(same_text(&left.label, &right.label, control)?
        && same_texts(
            left.implied_labels.iter().map(String::as_str),
            right.implied_labels.iter().map(String::as_str),
            control,
        )?
        && same_properties(&left.properties, &right.properties, control)?)
}

fn same_relationship(
    left: &GraphRelationshipType,
    right: &GraphRelationshipType,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    Ok(left.source_identifying == right.source_identifying
        && left.target_identifying == right.target_identifying
        && same_text(&left.relationship_type, &right.relationship_type, control)?
        && same_optional_text(
            left.source_label.as_deref(),
            right.source_label.as_deref(),
            control,
        )?
        && same_optional_text(
            left.target_label.as_deref(),
            right.target_label.as_deref(),
            control,
        )?
        && same_properties(&left.properties, &right.properties, control)?)
}

fn same_constraint(
    left: &ConstraintDefinition,
    right: &ConstraintDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    Ok(same_text(&left.name, &right.name, control)?
        && same_target(&left.target, &right.target, control)?
        && same_texts(
            left.properties.iter().map(String::as_str),
            right.properties.iter().map(String::as_str),
            control,
        )?
        && same_constraint_kind(&left.kind, &right.kind, control)?
        && same_optional_text(left.origin.as_deref(), right.origin.as_deref(), control)?)
}

fn same_target(
    left: &SchemaTarget,
    right: &SchemaTarget,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    match (left, right) {
        (SchemaTarget::Node { label: left }, SchemaTarget::Node { label: right }) => {
            same_text(left, right, control)
        }
        (
            SchemaTarget::Relationship {
                relationship_type: left,
            },
            SchemaTarget::Relationship {
                relationship_type: right,
            },
        ) => same_text(left, right, control),
        _ => Ok(false),
    }
}

fn same_constraint_kind(
    left: &ConstraintDefinitionKind,
    right: &ConstraintDefinitionKind,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    match (left, right) {
        (
            ConstraintDefinitionKind::Type { rule: left },
            ConstraintDefinitionKind::Type { rule: right },
        ) => same_rule(left, right, control),
        _ => Ok(left == right),
    }
}

fn same_properties(
    left: &BTreeMap<String, PropertyRule>,
    right: &BTreeMap<String, PropertyRule>,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for ((name, rule), (other_name, other_rule)) in left.iter().zip(right) {
        control.work(1)?;
        if !same_text(name, other_name, control)? || !same_rule(rule, other_rule, control)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn same_rule(
    left: &PropertyRule,
    right: &PropertyRule,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    Ok(left.required == right.required
        && same_type(&left.property_type, &right.property_type, control)?)
}

fn same_type(
    left: &PropertyType,
    right: &PropertyType,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    control.work(1)?;
    match (left, right) {
        (PropertyType::List { element: left }, PropertyType::List { element: right }) => {
            same_type(left, right, control)
        }
        (PropertyType::Union { members: left }, PropertyType::Union { members: right }) => {
            if left.len() != right.len() {
                return Ok(false);
            }
            for (left, right) in left.iter().zip(right) {
                if !same_type(left, right, control)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (
            PropertyType::Vector {
                coordinate: left,
                dimension,
            },
            PropertyType::Vector {
                coordinate: right,
                dimension: other,
            },
        ) => Ok(dimension == other && same_text(left, right, control)?),
        _ => Ok(left == right),
    }
}

fn same_optional_text(
    left: Option<&str>,
    right: Option<&str>,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    match (left, right) {
        (Some(left), Some(right)) => same_text(left, right, control),
        (None, None) => Ok(true),
        _ => Ok(false),
    }
}

fn same_texts<'a>(
    left: impl ExactSizeIterator<Item = &'a str>,
    right: impl ExactSizeIterator<Item = &'a str>,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left.zip(right) {
        control.work(1)?;
        if !same_text(left, right, control)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn same_text(left: &str, right: &str, control: &ExecutionControl<'_>) -> QueryResult<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(64 * 1024)
        .zip(right.as_bytes().chunks(64 * 1024))
    {
        control.bytes(left.len() as u64)?;
        if left != right {
            return Ok(false);
        }
    }
    Ok(true)
}

struct Validation<'a, 'connection, 'control> {
    connection: &'a Connection,
    snapshot: &'a Snapshot<'connection>,
    control: &'a ExecutionControl<'control>,
}

impl Validation<'_, '_, '_> {
    fn changed_nodes(&self, previous: &SchemaState, next: &SchemaState) -> QueryResult<()> {
        for (label, definition) in &next.graph_nodes {
            self.control.work(1)?;
            if changed(previous.graph_nodes.get(label), |old| {
                same_node(old, definition, self.control)
            })? {
                self.graph_node(definition)?;
            }
        }
        Ok(())
    }

    fn changed_relationships(&self, previous: &SchemaState, next: &SchemaState) -> QueryResult<()> {
        for (relationship_type, definition) in &next.graph_relationships {
            self.control.work(1)?;
            if changed(previous.graph_relationships.get(relationship_type), |old| {
                same_relationship(old, definition, self.control)
            })? {
                self.graph_relationship(definition)?;
            }
        }
        Ok(())
    }

    fn changed_constraints(&self, previous: &SchemaState, next: &SchemaState) -> QueryResult<()> {
        for (name, constraint) in &next.constraints {
            self.control.work(1)?;
            if changed(previous.constraints.get(name), |old| {
                same_constraint(old, constraint, self.control)
            })? {
                self.constraint(constraint)?;
            }
        }
        Ok(())
    }

    fn graph_node(&self, definition: &GraphNodeType) -> QueryResult<()> {
        let Some(label_id) = storage::find_label(self.connection, &definition.label)? else {
            return Ok(());
        };
        self.nodes(label_id, |node_id| {
            for implied in &definition.implied_labels {
                self.control.work(1)?;
                if !self.has_label(node_id, storage::find_label(self.connection, implied)?)? {
                    return Err(QueryError::constraint(format!(
                        "Graph Type node {} is missing implied label {implied}",
                        definition.label
                    )));
                }
            }
            self.properties(
                &definition.label,
                OwnerKind::Node,
                node_id,
                &definition.properties,
            )
        })
    }

    fn graph_relationship(&self, definition: &GraphRelationshipType) -> QueryResult<()> {
        let Some(type_id) =
            storage::find_relationship_type(self.connection, &definition.relationship_type)?
        else {
            return Ok(());
        };
        let source = resolve_optional_label(self.connection, definition.source_label.as_deref())?;
        let target = resolve_optional_label(self.connection, definition.target_label.as_deref())?;
        self.relationships(type_id, |relationship| {
            self.endpoint(definition, relationship.source, source, "source")?;
            self.endpoint(definition, relationship.target, target, "target")?;
            self.properties(
                &definition.relationship_type,
                OwnerKind::Relationship,
                relationship.id,
                &definition.properties,
            )
        })
    }

    fn nodes(
        &self,
        label_id: i64,
        mut visit: impl FnMut(i64) -> QueryResult<()>,
    ) -> QueryResult<()> {
        let mut after = 0;
        loop {
            let _page_memory = self.page_memory()?;
            let page = self.snapshot.scan_label_after(label_id, after, PAGE_SIZE)?;
            for node in page.items {
                self.control.work(1)?;
                visit(node)?;
            }
            let Some(next) = page.next_after else {
                return Ok(());
            };
            after = next;
        }
    }

    fn relationships(
        &self,
        type_id: i64,
        mut visit: impl FnMut(RelationshipRecord) -> QueryResult<()>,
    ) -> QueryResult<()> {
        let mut after = 0;
        loop {
            let _page_memory = self.page_memory()?;
            // The legacy type scan filters the entire overlay before yielding;
            // bounded identity pages also allow cancellation for sparse types.
            let page = self.snapshot.scan_relationships_after(after, PAGE_SIZE)?;
            for relationship in page.items {
                self.control.work(1)?;
                if relationship.type_id == type_id {
                    visit(relationship)?;
                }
            }
            let Some(next) = page.next_after else {
                return Ok(());
            };
            after = next;
        }
    }

    fn page_memory(&self) -> QueryResult<MemoryReservation> {
        self.control
            .reserve(checked_mul(PAGE_SIZE as u64, PAGE_ITEM_BYTES)?)
    }

    fn has_label(&self, node_id: i64, expected: Option<i64>) -> QueryResult<bool> {
        let Some(expected) = expected else {
            return Ok(false);
        };
        let mut found = false;
        let check = || self.control.check().map_err(QueryError::into_storage_error);
        self.snapshot
            .visit_node_labels_checked(node_id, &check, |label| {
                self.control.work(1)?;
                found |= label == expected;
                Ok::<_, QueryError>(())
            })?;
        Ok(found)
    }

    fn endpoint(
        &self,
        definition: &GraphRelationshipType,
        node_id: i64,
        expected: Option<(&str, Option<i64>)>,
        endpoint: &str,
    ) -> QueryResult<()> {
        let Some((label, label_id)) = expected else {
            return Ok(());
        };
        if !self.has_label(node_id, label_id)? {
            return Err(QueryError::constraint(format!(
                "Graph Type relationship {} {endpoint} is missing implied label {label}",
                definition.relationship_type
            )));
        }
        Ok(())
    }

    fn with_property<T>(
        &self,
        owner: OwnerKind,
        id: i64,
        property: &str,
        visit: impl FnOnce(Option<&PropertyValue>) -> QueryResult<T>,
    ) -> QueryResult<T> {
        self.control.work(1)?;
        let Some(key_id) = storage::find_property_key(self.connection, property)? else {
            return visit(None);
        };
        let check = || self.control.check().map_err(QueryError::into_storage_error);
        self.snapshot
            .with_property_checked(owner, id, key_id, &check, visit)
    }

    fn properties(
        &self,
        owner: &str,
        kind: OwnerKind,
        id: i64,
        rules: &BTreeMap<String, PropertyRule>,
    ) -> QueryResult<()> {
        for (property, rule) in rules {
            self.with_property(kind, id, property, |value| {
                validate_rule(
                    owner,
                    element(kind),
                    id,
                    property,
                    rule,
                    value,
                    self.control,
                )
            })?;
        }
        Ok(())
    }

    fn constraint(&self, constraint: &ConstraintDefinition) -> QueryResult<()> {
        let validate = |kind, id| self.constraint_instance(constraint, kind, id);
        with_constraint_uniqueness_state(self.connection, constraint, || match &constraint.target {
            SchemaTarget::Node { label } => {
                if let Some(label_id) = storage::find_label(self.connection, label)? {
                    self.nodes(label_id, |id| validate(OwnerKind::Node, id))?;
                }
                Ok(())
            }
            SchemaTarget::Relationship { relationship_type } => {
                if let Some(type_id) =
                    storage::find_relationship_type(self.connection, relationship_type)?
                {
                    self.relationships(type_id, |record| {
                        validate(OwnerKind::Relationship, record.id)
                    })?;
                }
                Ok(())
            }
        })
    }

    fn constraint_instance(
        &self,
        constraint: &ConstraintDefinition,
        kind: OwnerKind,
        id: i64,
    ) -> QueryResult<()> {
        match &constraint.kind {
            ConstraintDefinitionKind::Key | ConstraintDefinitionKind::Unique => {
                if let Some(key) = self.uniqueness(constraint, kind, id)? {
                    self.control.check()?;
                    record_uniqueness_key(
                        self.connection,
                        constraint,
                        element(kind),
                        id,
                        &key.bytes,
                    )?;
                    self.control.check()?;
                }
                Ok(())
            }
            ConstraintDefinitionKind::NotNull => {
                let property = first_property(constraint)?;
                self.with_property(kind, id, property, |value| {
                    if value.is_none() {
                        Err(missing_property(constraint, kind, id, property))
                    } else {
                        Ok(())
                    }
                })
            }
            ConstraintDefinitionKind::Type { rule } => {
                let property = first_property(constraint)?;
                self.with_property(kind, id, property, |value| {
                    validate_rule(
                        &constraint.name,
                        element(kind),
                        id,
                        property,
                        rule,
                        value,
                        self.control,
                    )
                })
            }
        }
    }

    fn uniqueness(
        &self,
        constraint: &ConstraintDefinition,
        kind: OwnerKind,
        id: i64,
    ) -> QueryResult<Option<KeyBytes>> {
        let mut key = KeyBytes::new(self.control)?;
        key.append(
            &(constraint.properties.len() as u64).to_le_bytes(),
            self.control,
        )?;
        for property in &constraint.properties {
            let part = self.with_property(kind, id, property, |value| {
                let Some(value) = value else {
                    return if matches!(constraint.kind, ConstraintDefinitionKind::Key) {
                        Err(missing_property(constraint, kind, id, property))
                    } else {
                        Ok(None)
                    };
                };
                equality_key(value, self.control)?.map_or_else(
                    || non_reflexive_key(id, self.control).map(Some),
                    |key| Ok(Some(key)),
                )
            })?;
            let Some(part) = part else {
                return Ok(None);
            };
            key.append(&(part.bytes.len() as u64).to_le_bytes(), self.control)?;
            key.append(&part.bytes, self.control)?;
        }
        Ok(Some(key))
    }
}

fn element(kind: OwnerKind) -> &'static str {
    match kind {
        OwnerKind::Node => "node",
        OwnerKind::Relationship => "relationship",
    }
}

fn first_property(constraint: &ConstraintDefinition) -> QueryResult<&str> {
    constraint
        .properties
        .first()
        .map(String::as_str)
        .ok_or_else(|| QueryError::internal("constraint has no property"))
}

fn missing_property(
    constraint: &ConstraintDefinition,
    kind: OwnerKind,
    id: i64,
    property: &str,
) -> QueryError {
    QueryError::constraint(format!(
        "constraint {} requires {} {id} property {property}",
        constraint.name,
        element(kind)
    ))
}

fn validate_rule(
    owner: &str,
    element: &str,
    element_id: i64,
    property: &str,
    rule: &PropertyRule,
    value: Option<&PropertyValue>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let Some(value) = value else {
        return validate_property_rule(owner, element, element_id, property, rule, None);
    };
    if !accepts(&rule.property_type, value, control)? {
        return Err(QueryError::constraint(format!(
            "{owner} rejects the type of {element} {element_id} property {property}"
        )));
    }
    Ok(())
}

fn accepts(
    rule: &PropertyType,
    value: &PropertyValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    control.work(1)?;
    match (rule, value) {
        (PropertyType::List { element }, PropertyValue::List(values)) => {
            for value in values {
                if !accepts(element, value, control)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (PropertyType::Union { members }, value) => {
            for member in members {
                if accepts(member, value, control)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => Ok(rule.accepts(value)),
    }
}

struct KeyBytes {
    bytes: Vec<u8>,
    memory: MemoryReservation,
}

impl KeyBytes {
    fn new(control: &ExecutionControl<'_>) -> QueryResult<Self> {
        Ok(Self {
            bytes: Vec::new(),
            memory: control.reserve(0)?,
        })
    }

    fn append(&mut self, bytes: &[u8], control: &ExecutionControl<'_>) -> QueryResult<()> {
        control.check()?;
        let required = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| memory_error("constraint key size overflow"))?;
        if required > self.bytes.capacity() {
            let capacity = required
                .max(self.bytes.capacity().saturating_mul(2))
                .max(32);
            // During a moving reallocation both old and new buffers can exist.
            self.memory.grow(allocation_bytes::<u8>(capacity)?)?;
            self.bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .map_err(|_| memory_error("cannot allocate constraint key"))?;
            self.memory
                .resize(allocation_bytes::<u8>(self.bytes.capacity())?)?;
        }
        for chunk in bytes.chunks(64 * 1024) {
            control.check()?;
            self.bytes.extend_from_slice(chunk);
        }
        Ok(())
    }
}

fn equality_key(
    value: &PropertyValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<KeyBytes>> {
    control.work(1)?;
    match value {
        PropertyValue::Integer(_) | PropertyValue::Float(_) => {
            let _temporary = control.reserve(128)?;
            let Some(bytes) = property_equality_key(value)? else {
                return Ok(None);
            };
            let mut key = KeyBytes::new(control)?;
            key.append(&bytes, control)?;
            Ok(Some(key))
        }
        PropertyValue::List(values) => list_key(values, control),
        PropertyValue::Point(point) => point_key(point, control),
        PropertyValue::Vector(vector) => vector_key(value, vector, control),
        _ => typed_key(value, control).map(Some),
    }
}

fn typed_key(value: &PropertyValue, control: &ExecutionControl<'_>) -> QueryResult<KeyBytes> {
    let check = || control.check().map_err(QueryError::into_storage_error);
    let bytes = storage::property_value_memory_bytes(value, &check)?;
    // Canonical record fields and the assembled record coexist. For small
    // scalar payloads the fixed allowance covers field-vector overhead.
    let _workspace = control.reserve(checked_add(checked_mul(bytes, 8)?, 512)?)?;
    let canonical = value.canonical_bytes_checked(&check)?;
    let mut key = KeyBytes::new(control)?;
    key.append(b"typed", control)?;
    key.append(&canonical, control)?;
    Ok(key)
}

fn list_key(
    values: &[PropertyValue],
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<KeyBytes>> {
    let mut key = KeyBytes::new(control)?;
    key.append(b"list", control)?;
    for value in values {
        let Some(part) = equality_key(value, control)? else {
            return Ok(None);
        };
        key.append(&(part.bytes.len() as u64).to_le_bytes(), control)?;
        key.append(&part.bytes, control)?;
    }
    Ok(Some(key))
}

fn point_key(
    point: &storage::PointValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<KeyBytes>> {
    let mut key = KeyBytes::new(control)?;
    key.append(b"point", control)?;
    key.append(&point.crs.to_le_bytes(), control)?;
    key.append(&(point.coordinates.len() as u64).to_le_bytes(), control)?;
    for coordinate in &point.coordinates {
        control.work(1)?;
        if coordinate.is_nan() {
            return Ok(None);
        }
        let bits = if *coordinate == 0.0 {
            0
        } else {
            coordinate.to_bits()
        };
        key.append(&bits.to_le_bytes(), control)?;
    }
    Ok(Some(key))
}

fn vector_key(
    value: &PropertyValue,
    vector: &storage::VectorValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<KeyBytes>> {
    let check = || control.check().map_err(QueryError::into_storage_error);
    let bytes = storage::property_value_memory_bytes(value, &check)?;
    let _workspace = control.reserve(checked_add(checked_mul(bytes, 8)?, 512)?)?;
    // Retain legacy canonical validation without cloning the vector payload.
    let _canonical = value.canonical_bytes_checked(&check)?;
    if !matches!(
        vector.coordinate_type,
        VectorCoordinateType::F32 | VectorCoordinateType::F64
    ) {
        return typed_key(value, control).map(Some);
    }
    let mut key = KeyBytes::new(control)?;
    key.append(b"vector", control)?;
    key.append(&[vector.coordinate_type as u8], control)?;
    key.append(&vector.dimension.to_le_bytes(), control)?;
    let reflexive = match vector.coordinate_type {
        VectorCoordinateType::F32 => {
            coordinate_keys(&vector.packed, &mut key, control, normalized_f32)?
        }
        VectorCoordinateType::F64 => {
            coordinate_keys(&vector.packed, &mut key, control, normalized_f64)?
        }
        _ => unreachable!(),
    };
    Ok(reflexive.then_some(key))
}

fn coordinate_keys<const N: usize>(
    packed: &[u8],
    key: &mut KeyBytes,
    control: &ExecutionControl<'_>,
    normalize: impl Fn([u8; N]) -> Option<[u8; N]>,
) -> QueryResult<bool> {
    for chunk in packed.as_chunks::<N>().0 {
        control.work(1)?;
        let Some(bytes) = normalize(*chunk) else {
            return Ok(false);
        };
        key.append(&bytes, control)?;
    }
    Ok(true)
}

fn normalized_f32(bytes: [u8; 4]) -> Option<[u8; 4]> {
    let value = f32::from_le_bytes(bytes);
    if value.is_nan() {
        None
    } else {
        Some(if value == 0.0 { [0; 4] } else { bytes })
    }
}

fn normalized_f64(bytes: [u8; 8]) -> Option<[u8; 8]> {
    let value = f64::from_le_bytes(bytes);
    if value.is_nan() {
        None
    } else {
        Some(if value == 0.0 { [0; 8] } else { bytes })
    }
}

fn non_reflexive_key(id: i64, control: &ExecutionControl<'_>) -> QueryResult<KeyBytes> {
    let mut key = KeyBytes::new(control)?;
    key.append(b"non_reflexive", control)?;
    key.append(&id.to_le_bytes(), control)?;
    Ok(key)
}

#[cfg(test)]
mod tests;
