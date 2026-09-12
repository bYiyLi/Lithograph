use super::*;
use lithograph_core::cypher::{
    DateValue, DurationValue, LocalDateTimeValue, LocalTimeValue, PointValue, TimeValue, UuidValue,
    VectorCoordinateType, VectorValue, VectorValues, ZonedDateTimeValue,
};

#[test]
fn mutation_persists_and_reads_each_supported_property_value_family() {
    let connection = fresh_storage();
    let entries = vec![
        ("boolean", Value::Boolean(true)),
        ("integer", Value::Integer(i64::MIN)),
        ("float", Value::Float(-1.25)),
        ("string", Value::String("typed".to_owned())),
        (
            "list",
            Value::List(vec![Value::Integer(1), Value::Integer(2)]),
        ),
        (
            "date",
            Value::Date(DateValue::parse("2026-09-11").expect("date")),
        ),
        (
            "localTime",
            Value::LocalTime(LocalTimeValue::parse("12:34:56.123456789").expect("local time")),
        ),
        (
            "time",
            Value::Time(TimeValue::parse("12:34:56.123456789+08:00").expect("time")),
        ),
        (
            "localDateTime",
            Value::LocalDateTime(
                LocalDateTimeValue::parse("2026-09-11T12:34:56.123456789").expect("local datetime"),
            ),
        ),
        (
            "zonedDateTime",
            Value::ZonedDateTime(
                ZonedDateTimeValue::parse("2026-09-11T12:34:56.123456789+08:00", "+08:00")
                    .expect("fixed-zone datetime"),
            ),
        ),
        (
            "duration",
            Value::Duration(DurationValue::parse("P1M2DT3.000000004S").expect("duration")),
        ),
        (
            "point",
            Value::Point(PointValue::new("cartesian", vec![1.5, -2.25]).expect("point")),
        ),
        (
            "vector",
            Value::Vector(
                VectorValue::new(
                    VectorCoordinateType::I16,
                    VectorValues::I16(vec![i16::MIN, 0, i16::MAX]),
                )
                .expect("vector"),
            ),
        ),
        (
            "uuid",
            Value::Uuid(UuidValue::parse("550e8400-e29b-41d4-a716-446655440000").expect("uuid")),
        ),
    ];
    let params = entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect();
    let expected = entries
        .iter()
        .map(|(_, value)| value.clone())
        .collect::<Vec<_>>();
    let query = "CREATE (n:TypedValues {boolean:$boolean, integer:$integer, float:$float, string:$string, list:$list, date:$date, localTime:$localTime, time:$time, localDateTime:$localDateTime, zonedDateTime:$zonedDateTime, duration:$duration, point:$point, vector:$vector, uuid:$uuid}) RETURN n.boolean, n.integer, n.float, n.string, n.list, n.date, n.localTime, n.time, n.localDateTime, n.zonedDateTime, n.duration, n.point, n.vector, n.uuid";

    let (rows, summary) =
        execute_with_params(&connection, query, params, ExecutionOptions::default())
            .expect("persist typed values");
    assert_eq!(rows, vec![expected.clone()]);
    assert_eq!(summary.counters.properties_set, 14);
    assert_eq!(
        read_rows(
            &connection,
            "MATCH (n:TypedValues) RETURN n.boolean, n.integer, n.float, n.string, n.list, n.date, n.localTime, n.time, n.localDateTime, n.zonedDateTime, n.duration, n.point, n.vector, n.uuid"
        ),
        vec![expected]
    );
}

#[test]
fn named_zone_persistence_round_trips_with_timezone_rules() {
    let connection = fresh_storage();
    let value = Value::ZonedDateTime(
        ZonedDateTimeValue::parse("2026-09-11T12:34:56+08:00", "Asia/Shanghai")
            .expect("named-zone datetime"),
    );
    let params = BTreeMap::from([("value".to_owned(), value.clone())]);

    let (_, summary) = execute_with_params(
        &connection,
        "CREATE (:NamedZoneValue {value:$value}) FINISH",
        params,
        ExecutionOptions::default(),
    )
    .expect("persist named-zone datetime");

    assert_eq!(summary.counters.nodes_created, 1);
    assert_eq!(summary.counters.properties_set, 1);
    assert_eq!(
        read_rows(&connection, "MATCH (node:NamedZoneValue) RETURN node.value"),
        vec![vec![value]]
    );
}
