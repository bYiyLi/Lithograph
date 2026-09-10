use std::cmp::Ordering;
use std::collections::BTreeMap;

use lithograph_core::cypher::{
    DateValue, DurationValue, LocalDateTimeValue, LocalTimeValue, NodeValue, PathValue, PointValue,
    RelationshipValue, TimeValue, UuidValue, Value, VectorCoordinateType, VectorValue,
    VectorValues, ZonedDateTimeValue, cypher_compare, cypher_order_compare,
};

#[test]
fn order_by_is_distinct_from_direct_comparison() {
    assert_eq!(
        cypher_order_compare(&Value::String("z".into()), &Value::Boolean(false)),
        Ok(Ordering::Less)
    );
    assert_eq!(
        cypher_order_compare(&Value::Integer(1), &Value::Null),
        Ok(Ordering::Less)
    );
    assert_eq!(
        cypher_order_compare(
            &Value::List(vec![Value::Integer(1)]),
            &Value::List(vec![Value::Integer(1), Value::Null]),
        ),
        Ok(Ordering::Less)
    );
    assert_eq!(
        cypher_order_compare(
            &Value::Map(BTreeMap::from([("a".into(), Value::Integer(1))])),
            &Value::Map(BTreeMap::from([
                ("a".into(), Value::Integer(1)),
                ("b".into(), Value::Integer(0)),
            ])),
        ),
        Ok(Ordering::Less)
    );

    let month = Value::Duration(DurationValue::parse("P1M").expect("duration"));
    let thirty_days = Value::Duration(DurationValue::parse("P30D").expect("duration"));
    assert_eq!(cypher_compare(&month, &thirty_days), Ok(None));
    assert_eq!(
        cypher_order_compare(&month, &thirty_days),
        Ok(Ordering::Greater)
    );

    let vector_i8 = vector_i8(&[1, 2]);
    let vector_i16 = Value::Vector(
        VectorValue::new(VectorCoordinateType::I16, VectorValues::I16(vec![0])).expect("vector"),
    );
    assert!(cypher_compare(&vector_i8, &vector_i16).is_err());
    assert_eq!(
        cypher_order_compare(&vector_i8, &vector_i16),
        Ok(Ordering::Less)
    );

    let wgs = Value::Point(PointValue::new("wgs-84", vec![0.0, 0.0]).expect("point"));
    let cartesian = Value::Point(PointValue::new("cartesian", vec![0.0, 0.0]).expect("point"));
    assert!(cypher_compare(&wgs, &cartesian).is_err());
    assert_eq!(cypher_order_compare(&wgs, &cartesian), Ok(Ordering::Less));

    let uuid_a = uuid("00000000-0000-0000-0000-000000000001");
    let uuid_b = uuid("00000000-0000-0000-0000-000000000002");
    assert!(cypher_order_compare(&uuid_a, &uuid_b).is_err());
    assert!(cypher_order_compare(&uuid_a, &Value::String("x".into())).is_err());
}

#[test]
fn order_by_structural_same_family_ordering_is_stable() {
    let map_a = Value::Map(BTreeMap::from([("a".into(), Value::Integer(1))]));
    let map_b = Value::Map(BTreeMap::from([("b".into(), Value::Integer(1))]));
    let map_c = Value::Map(BTreeMap::from([("a".into(), Value::Integer(2))]));
    assert_eq!(cypher_order_compare(&map_a, &map_b), Ok(Ordering::Less));
    assert_eq!(cypher_order_compare(&map_a, &map_c), Ok(Ordering::Less));

    let node_a = node("n:1");
    let node_b = node("n:2");
    assert_eq!(
        cypher_order_compare(&Value::Node(node_a.clone()), &Value::Node(node_b.clone())),
        Ok(Ordering::Less)
    );

    let relationship_a = relationship("r:1");
    let relationship_b = relationship("r:2");
    assert_eq!(
        cypher_order_compare(
            &Value::Relationship(relationship_a.clone()),
            &Value::Relationship(relationship_b.clone()),
        ),
        Ok(Ordering::Less)
    );
    assert_eq!(
        cypher_order_compare(
            &Value::List(vec![Value::Integer(1), Value::Integer(2)]),
            &Value::List(vec![Value::Integer(1), Value::Integer(3)]),
        ),
        Ok(Ordering::Less)
    );

    let short = Value::Path(PathValue {
        nodes: vec![node_a.clone()],
        relationships: vec![],
    });
    let long = Value::Path(PathValue {
        nodes: vec![node_a.clone(), node_b.clone()],
        relationships: vec![relationship_a],
    });
    let other = Value::Path(PathValue {
        nodes: vec![node_a, node_b],
        relationships: vec![relationship_b],
    });
    assert_eq!(cypher_order_compare(&short, &long), Ok(Ordering::Less));
    assert_eq!(cypher_order_compare(&long, &other), Ok(Ordering::Less));
}

#[test]
fn order_by_vector_and_point_same_family_ordering_is_stable() {
    assert_eq!(
        cypher_order_compare(&vector_i8(&[1, 2]), &vector_i8(&[1, 3])),
        Ok(Ordering::Less)
    );
    assert_eq!(
        cypher_order_compare(&vector_i8(&[1, 2]), &vector_i8(&[1, 2, 3])),
        Ok(Ordering::Less)
    );
    let f32_a = Value::Vector(
        VectorValue::new(VectorCoordinateType::F32, VectorValues::F32(vec![1.0])).expect("vector"),
    );
    let f32_b = Value::Vector(
        VectorValue::new(VectorCoordinateType::F32, VectorValues::F32(vec![2.0])).expect("vector"),
    );
    assert_eq!(cypher_order_compare(&f32_a, &f32_b), Ok(Ordering::Less));

    let point_a = Value::Point(PointValue::new("cartesian", vec![1.0, 2.0]).expect("point"));
    let point_b = Value::Point(PointValue::new("cartesian", vec![1.0, 3.0]).expect("point"));
    assert_eq!(cypher_order_compare(&point_a, &point_b), Ok(Ordering::Less));
}

#[test]
fn order_by_scalar_and_temporal_same_family_ordering_is_stable() {
    assert_less(
        Value::Date(DateValue::parse("2026-09-09").expect("date")),
        Value::Date(DateValue::parse("2026-09-10").expect("date")),
    );
    assert_less(
        Value::LocalTime(LocalTimeValue::parse("12:00:00").expect("time")),
        Value::LocalTime(LocalTimeValue::parse("13:00:00").expect("time")),
    );
    assert_less(
        Value::Time(TimeValue::parse("12:00:00+00:00").expect("time")),
        Value::Time(TimeValue::parse("13:00:00+00:00").expect("time")),
    );
    assert_less(
        Value::LocalDateTime(LocalDateTimeValue::parse("2026-09-09T12:00:00").expect("datetime")),
        Value::LocalDateTime(LocalDateTimeValue::parse("2026-09-10T12:00:00").expect("datetime")),
    );
    assert_less(
        Value::ZonedDateTime(
            ZonedDateTimeValue::parse("2026-09-09T12:00:00+00:00", "UTC").expect("datetime"),
        ),
        Value::ZonedDateTime(
            ZonedDateTimeValue::parse("2026-09-10T12:00:00+00:00", "UTC").expect("datetime"),
        ),
    );
    assert_less(Value::String("a".into()), Value::String("b".into()));
    assert_less(Value::Boolean(false), Value::Boolean(true));
    assert_less(Value::Integer(1), Value::Float(1.5));
    assert_eq!(
        cypher_order_compare(&Value::Float(1.5), &Value::Integer(1)),
        Ok(Ordering::Greater)
    );
    assert_eq!(
        cypher_order_compare(&Value::Float(f64::NAN), &Value::Float(1.0)),
        Ok(Ordering::Greater)
    );
    assert_eq!(
        cypher_order_compare(&Value::Null, &Value::Null),
        Ok(Ordering::Equal)
    );
}

fn assert_less(left: Value, right: Value) {
    assert_eq!(cypher_order_compare(&left, &right), Ok(Ordering::Less));
}

fn node(element_id: &str) -> NodeValue {
    NodeValue {
        element_id: element_id.into(),
        labels: vec![],
        properties: BTreeMap::new(),
    }
}

fn relationship(element_id: &str) -> RelationshipValue {
    RelationshipValue {
        element_id: element_id.into(),
        relationship_type: "R".into(),
        start: "n:1".into(),
        end: "n:2".into(),
        properties: BTreeMap::new(),
    }
}

fn vector_i8(values: &[i8]) -> Value {
    Value::Vector(
        VectorValue::new(VectorCoordinateType::I8, VectorValues::I8(values.to_vec()))
            .expect("vector"),
    )
}

fn uuid(value: &str) -> Value {
    Value::Uuid(UuidValue::parse(value).expect("uuid"))
}
