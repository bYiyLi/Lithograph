use std::cell::Cell;

use super::*;
use crate::query::{QueryErrorKind, QueryResources};

fn definition() -> IndexDefinition {
    IndexDefinition {
        name: "index".to_owned(),
        kind: StandardIndexKind::Range,
        target: IndexTarget::NodeProperties {
            label: "人物".to_owned(),
            properties: vec!["name".to_owned()],
        },
        owning_constraint: None,
        labels_or_types: Vec::new(),
        additional_properties: Vec::new(),
        configuration: None,
    }
}

#[test]
fn checked_equality_encoding_preserves_standard_index_semantics() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut values = vec![
        PropertyValue::Integer(0),
        PropertyValue::Float(-0.0),
        PropertyValue::Float(f64::NAN),
        PropertyValue::String("人物`\0".repeat(100_000)),
        PropertyValue::List(vec![PropertyValue::Integer(1), PropertyValue::Float(1.5)]),
        PropertyValue::Point(storage::PointValue {
            crs: 7203,
            coordinates: vec![1.0, -0.0],
        }),
    ];
    for number in [0.0f32, -0.0, 1.5, f32::INFINITY, f32::NAN] {
        values.push(PropertyValue::Vector(storage::VectorValue {
            coordinate_type: VectorCoordinateType::F32,
            dimension: 1,
            packed: number.to_le_bytes().to_vec(),
        }));
    }
    for number in [0.0f64, -0.0, 1.5, f64::INFINITY, f64::NAN] {
        values.push(PropertyValue::Vector(storage::VectorValue {
            coordinate_type: VectorCoordinateType::F64,
            dimension: 1,
            packed: number.to_le_bytes().to_vec(),
        }));
    }
    values.push(PropertyValue::Vector(storage::VectorValue {
        coordinate_type: VectorCoordinateType::I16,
        dimension: 1,
        packed: 5i16.to_le_bytes().to_vec(),
    }));
    for value in values {
        assert_eq!(
            equality_key(&value, &control).unwrap(),
            property_equality_key(&value).unwrap()
        );
    }
}

#[test]
fn per_value_workspace_rejects_before_encoding_and_returns_its_claim() {
    let resources = QueryResources::new(Some(1024 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let value = PropertyValue::String("x".repeat(128 * 1024));
    let bytes = value_workspace(&value, &control).unwrap();
    let error = control.reserve(bytes).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert_eq!(resources.used(), 0);
    assert_eq!(resources.peak(), 0);

    let resources = QueryResources::new(Some(bytes));
    let claim = resources.reserve(bytes).unwrap();
    assert_eq!(resources.used(), bytes);
    drop(claim);
    assert_eq!(resources.used(), 0);
}

#[test]
fn controlled_equality_checks_long_lists_and_packed_vectors() {
    let resources = QueryResources::default();
    let calls = Cell::new(0);
    let interrupted = || {
        calls.set(calls.get() + 1);
        calls.get() >= 3
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let value = PropertyValue::List(vec![PropertyValue::Integer(1); 4096]);
    assert_eq!(
        equality_key(&value, &control).unwrap_err().kind,
        QueryErrorKind::Interrupted
    );
    assert_eq!(calls.get(), 3);

    calls.set(0);
    let control = ExecutionControl::new(&resources, &interrupted);
    let value = PropertyValue::Vector(storage::VectorValue {
        coordinate_type: VectorCoordinateType::F64,
        dimension: 4096,
        packed: vec![0; 4096 * 8],
    });
    assert_eq!(
        equality_key(&value, &control).unwrap_err().kind,
        QueryErrorKind::Interrupted
    );
    assert_eq!(calls.get(), 3);
}

#[test]
fn definition_preflight_counts_empty_slots_and_is_cancellable() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut index = definition();
    let small = definition_workspace(&index, &control).unwrap();
    index.additional_properties = vec![String::new(); 4096];
    let large = definition_workspace(&index, &control).unwrap();
    assert!(large > small + 4096 * std::mem::size_of::<String>() as u64 * 8);

    let control = ExecutionControl::new(&resources, &|| true);
    let error = definition_workspace(&index, &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(resources.used(), 0);
}
