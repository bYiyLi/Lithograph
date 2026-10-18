use std::cell::Cell;
use std::mem::size_of;
use std::sync::{Arc, Barrier};

use serde_json::{Value as JsonValue, json};

use super::*;
use crate::cypher::Value;
use crate::query::ExecutionOptions;

#[test]
fn leases_follow_live_allocations_and_rejected_growth_preserves_claims() {
    let resources = QueryResources::new(Some(100));
    let mut first = resources.reserve(60).unwrap();
    let second = resources.clone().reserve(40).unwrap();
    let error = first.grow(1).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert_eq!(
        (first.bytes(), resources.used(), resources.peak()),
        (60, 100, 100)
    );
    drop(second);
    first.resize(80).unwrap();
    first.resize(20).unwrap();
    assert_eq!((resources.used(), resources.peak()), (20, 100));
    drop(first);
    assert_eq!(resources.used(), 0);
}

#[test]
fn moving_and_splitting_a_lease_does_not_release_its_budget() {
    let resources = QueryResources::new(Some(100));
    let mut first = resources.reserve(100).unwrap();
    let second = first.split(40).unwrap();
    assert_eq!(
        (first.bytes(), second.bytes(), resources.used()),
        (60, 40, 100)
    );
    assert!(first.split(61).is_err());
    first.merge(second).unwrap();
    assert_eq!((first.bytes(), resources.used()), (100, 100));
    drop(first);
    assert_eq!(resources.used(), 0);
}

#[test]
fn distinct_executions_are_isolated_and_cannot_merge_claims() {
    let left = QueryResources::new(Some(100));
    let right = QueryResources::new(Some(100));
    let mut left_lease = left.reserve(100).unwrap();
    let right_lease = right.reserve(100).unwrap();
    assert!(left_lease.merge(right_lease).is_err());
    assert_eq!(left.used(), 100);
    assert_eq!(right.used(), 0);
}

#[test]
fn concurrent_workers_share_one_limit_without_lost_updates() {
    let resources = QueryResources::new(Some(1_024));
    let barrier = Arc::new(Barrier::new(9));
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let worker_resources = resources.clone();
            let worker_barrier = barrier.clone();
            scope.spawn(move || {
                let claim = worker_resources.reserve(128).unwrap();
                worker_barrier.wait();
                worker_barrier.wait();
                drop(claim);
            });
        }
        barrier.wait();
        assert_eq!(resources.used(), 1_024);
        assert!(resources.reserve(1).is_err());
        barrier.wait();
    });
    assert_eq!((resources.used(), resources.peak()), (0, 1_024));
}

#[test]
fn unlimited_execution_still_checks_arithmetic_and_tracks_peak() {
    let resources = QueryResources::default();
    let mut claim = resources.reserve(u64::MAX).unwrap();
    assert!(claim.grow(1).is_err());
    assert!(resources.reserve(1).is_err());
    assert_eq!(resources.used(), u64::MAX);
    claim.resize(0).unwrap();
    assert_eq!(resources.used(), 0);
    assert!(checked_mul(u64::MAX, 2).is_err());
}

#[test]
fn cancellation_is_checked_within_256_work_units_and_64kib() {
    let resources = QueryResources::default();
    let calls = Cell::new(0);
    let interrupt = || {
        calls.set(calls.get() + 1);
        true
    };
    let control = ExecutionControl::new(&resources, &interrupt);
    for _ in 0..255 {
        control.work(1).unwrap();
    }
    assert_eq!(calls.get(), 0);
    assert_eq!(
        control.work(1).unwrap_err().kind,
        QueryErrorKind::Interrupted
    );
    control.bytes(65_535).unwrap();
    assert_eq!(
        control.bytes(1).unwrap_err().kind,
        QueryErrorKind::Interrupted
    );
    assert_eq!(calls.get(), 2);
    assert!(control.work(u64::MAX).is_err());
    assert!(control.reserve(1).is_err());
    assert_eq!(resources.used(), 0);
}

#[test]
fn sizing_counts_spare_capacity_nested_values_and_json_owners() {
    let text = String::with_capacity(4_096);
    assert_eq!(
        value_bytes(&Value::String(text)).unwrap(),
        size_of::<Value>() as u64 + 4_096
    );
    let mut list = Vec::with_capacity(8);
    list.push(Value::String("payload".to_owned()));
    assert_eq!(
        value_bytes(&Value::List(list)).unwrap(),
        size_of::<Value>() as u64 * 9 + 7
    );
    let json_value = json!({"list": ["one", "two"], "number": 1});
    assert!(json_bytes(&json_value).unwrap() > json_value.to_string().len() as u64);
    assert_eq!(
        row_bytes(&[Value::Null, Value::Integer(2)]).unwrap(),
        (size_of::<Value>() * 2) as u64
    );
}

#[test]
fn point_sizing_counts_the_owned_vector_capacity_instead_of_dimension() {
    let mut coordinates = Vec::with_capacity(32_768);
    coordinates.extend([12.0, 34.0]);
    let point = crate::cypher::PointValue::new("cartesian", coordinates).unwrap();
    let bytes = value_bytes(&Value::Point(point)).unwrap();
    assert!(bytes >= (32_768 * size_of::<f64>() + size_of::<Value>()) as u64);
}

#[test]
fn singleton_maps_account_for_whole_tree_nodes() {
    let value = Value::Map(std::collections::BTreeMap::from([(
        "x".to_owned(),
        Value::Null,
    )]));
    let leaf_bound = (11 * size_of::<(String, Value)>() + 128) as u64;
    assert!(value_bytes(&value).unwrap() >= leaf_bound);
    let json_value = json!({"x": null});
    let json_leaf_bound = (11 * size_of::<(String, JsonValue)>() + 128) as u64;
    assert!(json_bytes(&json_value).unwrap() >= json_leaf_bound);
}

#[test]
fn encoded_bounds_cover_tagged_values_maps_graphs_and_vectors() {
    use crate::cypher::{
        DateValue, NodeValue, PathValue, RelationshipValue, VectorCoordinateType, VectorValue,
        VectorValues, encode_json,
    };
    use std::collections::BTreeMap;

    let node = NodeValue {
        element_id: "n:1".to_owned(),
        labels: vec!["人物".to_owned()],
        properties: BTreeMap::from([("$type".to_owned(), Value::Integer(i64::MAX))]),
    };
    let relationship = RelationshipValue {
        element_id: "r:1".to_owned(),
        relationship_type: "关联".to_owned(),
        start: "n:1".to_owned(),
        end: "n:2".to_owned(),
        properties: BTreeMap::new(),
    };
    let values = vec![
        Value::Null,
        Value::Integer(i64::MAX),
        Value::Float(f64::NAN),
        Value::String("quote\"slash\\control\n".to_owned()),
        Value::Map(node.properties.clone()),
        Value::Date(DateValue::parse("2026-10-18").unwrap()),
        Value::Node(node.clone()),
        Value::Relationship(relationship.clone()),
        Value::Path(PathValue {
            nodes: vec![node.clone(), node],
            relationships: vec![relationship],
        }),
        Value::Vector(
            VectorValue::new(
                VectorCoordinateType::I64,
                VectorValues::I64(vec![i64::MAX, 1]),
            )
            .unwrap(),
        ),
        Value::List(vec![Value::Null]),
    ];
    for value in &values {
        let actual = json_bytes(&encode_json(value)).unwrap();
        let estimate = encoded_json_bytes(value).unwrap();
        assert!(
            estimate >= actual,
            "estimate {estimate} below tree charge {actual}: {value:?}"
        );
    }
    assert!(
        encoded_row_bytes(&values).unwrap()
            >= json_bytes(&JsonValue::Array(values.iter().map(encode_json).collect())).unwrap()
    );
}

#[test]
fn sizing_large_values_is_itself_cancelable() {
    let resources = QueryResources::default();
    let interrupted = || true;
    let control = ExecutionControl::new(&resources, &interrupted);
    let value = Value::List(vec![Value::Integer(1); 1_000]);
    assert_eq!(
        value_bytes_controlled(&value, &control).unwrap_err().kind,
        QueryErrorKind::Interrupted
    );
    let control = ExecutionControl::new(&resources, &interrupted);
    assert_eq!(
        encoded_row_bytes_controlled(&[value], &control)
            .unwrap_err()
            .kind,
        QueryErrorKind::Interrupted
    );
}

#[test]
fn owned_rows_keep_spare_capacity_charged() {
    let mut values = Vec::with_capacity(8_192);
    values.push(Value::Integer(1));
    assert_eq!(
        owned_row_bytes(&values).unwrap(),
        (8_192 * size_of::<Value>()) as u64
    );
    let resources = QueryResources::default();
    let never_interrupted = || false;
    let control = ExecutionControl::new(&resources, &never_interrupted);
    assert_eq!(
        owned_row_bytes_controlled(&values, &control).unwrap(),
        owned_row_bytes(&values).unwrap()
    );
}

#[test]
fn memory_limit_options_accept_positive_signed_integer_range_only() {
    assert_eq!(
        ExecutionOptions::parse(&json!({}))
            .unwrap()
            .memory_limit_bytes,
        None
    );
    for value in [1, 67_108_864, i64::MAX] {
        assert_eq!(
            ExecutionOptions::parse(&json!({"memoryLimitBytes": value}))
                .unwrap()
                .memory_limit_bytes,
            Some(value as u64)
        );
    }
    for text in [
        "null",
        "0",
        "-1",
        "1.0",
        "true",
        "\"32\"",
        "9223372036854775808",
        "[]",
        "{}",
    ] {
        let text = format!("{{\"memoryLimitBytes\":{text}}}");
        assert_eq!(
            ExecutionOptions::parse_text(&text).unwrap_err().kind,
            QueryErrorKind::InvalidArgument
        );
    }
    let options = ExecutionOptions::parse(&json!({
        "memoryLimitBytes": 1,
        "readOnlyNoExternalIO": true,
        "readAccess": {"allowExternalIO": false}
    }))
    .unwrap();
    assert!(options.read_only_no_external_io);
    assert!(!options.access.read.allow_external_io);
    assert_eq!(options.memory_limit_bytes, Some(1));
}
