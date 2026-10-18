use std::cell::Cell;

use super::*;
use crate::query::resources::{QueryResources, value_bytes};

fn render(values: &[Value], control: &ExecutionControl<'_>) -> QueryResult<Value> {
    let mut memory = None;
    let result = evaluate_with_memory("format", values, Some(control), &mut memory).unwrap();
    if let Ok(value) = &result
        && !matches!(value, Value::Null)
    {
        assert!(memory.as_ref().unwrap().bytes() >= value_bytes(value).unwrap());
    }
    result
}

#[test]
fn controlled_format_preserves_temporal_pattern_and_error_semantics() {
    let values = [
        Value::Date(DateValue::parse("1986-11-18").unwrap()),
        Value::Date(DateValue::parse("-0001-01-01").unwrap()),
        Value::LocalTime(LocalTimeValue::parse("06:04:45.123456789").unwrap()),
        Value::Duration(DurationValue::parse("P1Y4M25DT5H6M7.00800901S").unwrap()),
        Value::Duration(DurationValue::parse("PT-1S").unwrap()),
        Value::ZonedDateTime(
            ZonedDateTimeValue::parse("1986-11-18T06:04:45+01:00", "Europe/Berlin").unwrap(),
        ),
        Value::Integer(1),
        Value::Null,
    ];
    let patterns = [
        "",
        "y yy yyy yyyy",
        "MM/dd/yyyy",
        "EEEE, MMMM d, G uuuu",
        "DDD'nd day of the year,' c'rd day of the week'",
        "k:mm z",
        "K:mm O",
        "pppYY",
        "pppMM",
        "ppMM",
        "pMMMM",
        "N '纳秒'",
        "''",
        "'a''b'",
        "'unterminated",
        "#",
        "pp",
        "VV",
        "SSSSSSSSS",
    ];
    let resources = QueryResources::new(Some(1024 * 1024));
    let never = || false;
    let control = ExecutionControl::new(&resources, &never);
    for value in values {
        assert_eq!(
            render(std::slice::from_ref(&value), &control),
            format_value(std::slice::from_ref(&value))
        );
        for pattern in patterns {
            let arguments = [value.clone(), Value::String(pattern.into())];
            assert_eq!(
                render(&arguments, &control),
                format_value(&arguments),
                "{arguments:?}"
            );
            assert_eq!(resources.used(), 0);
        }
    }
}

#[test]
fn long_numeric_width_is_rejected_before_token_or_output_allocation() {
    let resources = QueryResources::new(Some(4096));
    let never = || false;
    let control = ExecutionControl::new(&resources, &never);
    let values = [
        Value::Date(DateValue::parse("1986-11-18").unwrap()),
        Value::String("y".repeat(100_000)),
    ];
    assert_eq!(
        render(&values, &control).unwrap_err().kind,
        QueryErrorKind::Resource
    );
    assert_eq!(resources.peak(), 128);
    assert_eq!(resources.used(), 0);
}

#[test]
fn long_literals_padding_and_duration_presence_scans_are_cancelled() {
    let date = Value::Date(DateValue::parse("1986-11-18").unwrap());
    let duration = Value::Duration(DurationValue::parse("PT1S").unwrap());
    for (value, pattern) in [
        (date.clone(), "y".repeat(100_000)),
        (date.clone(), format!("'{}'", "人".repeat(100_000))),
        (date, format!("{}yy", "p".repeat(100_000))),
        (duration, "y".repeat(100_000)),
    ] {
        let checks = Cell::new(0);
        let interrupt = || {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        };
        let resources = QueryResources::new(Some(4 * 1024 * 1024));
        let control = ExecutionControl::new(&resources, &interrupt);
        assert_eq!(
            render(&[value, Value::String(pattern)], &control)
                .unwrap_err()
                .kind,
            QueryErrorKind::Interrupted
        );
        assert_eq!(resources.used(), 0);
    }
}

#[test]
fn large_format_result_keeps_its_exact_memory_lease_until_the_value_is_released() {
    let resources = QueryResources::new(Some(1024 * 1024));
    let never = || false;
    let control = ExecutionControl::new(&resources, &never);
    let mut memory = None;
    let pattern = "y".repeat(65_536);
    let values = [
        Value::Date(DateValue::parse("1986-11-18").unwrap()),
        Value::String(pattern),
    ];
    let value = evaluate_with_memory("format", &values, Some(&control), &mut memory)
        .unwrap()
        .unwrap();
    let Value::String(text) = &value else {
        panic!("expected String");
    };
    assert_eq!(text.len(), 65_536);
    assert_eq!(text.capacity(), text.len());
    assert!(text.ends_with("1986"));
    assert_eq!(resources.used(), value_bytes(&value).unwrap());
    drop(value);
    drop(memory);
    assert_eq!(resources.used(), 0);
}

#[test]
fn truncate_checks_during_invalid_deep_field_copy_before_constructor_type_validation() {
    // The function path for date.truncate('year', date('2000-06-01'), {month:$values}).
    let arguments = [
        Value::String("year".into()),
        Value::Date(DateValue::parse("2000-06-01").unwrap()),
        Value::Map(BTreeMap::from([(
            "month".into(),
            Value::List((0..100_000).map(Value::Integer).collect()),
        )])),
    ];
    let resources = QueryResources::new(Some(256 * 1024 * 1024));
    let bytes = arguments
        .iter()
        .map(|value| value_bytes(value).unwrap())
        .sum::<u64>()
        * 4
        + 1024;
    let scratch = resources.reserve(bytes).unwrap();
    let checks = Cell::new(0);
    let interrupt = || {
        checks.set(checks.get() + 1);
        checks.get() >= 2
    };
    let control = ExecutionControl::new(&resources, &interrupt);
    assert_eq!(
        truncate(&arguments, TemporalFamily::Date, Some(&control))
            .unwrap_err()
            .kind,
        QueryErrorKind::Interrupted
    );
    assert_eq!(checks.get(), 2);
    drop(scratch);
    assert_eq!(resources.used(), 0);
    let never = || false;
    let control = ExecutionControl::new(&resources, &never);
    let mut memory = None;
    let controlled =
        evaluate_with_memory("date.truncate", &arguments, Some(&control), &mut memory).unwrap();
    assert_eq!(controlled, evaluate("date.truncate", &arguments).unwrap());
    assert_eq!(controlled.unwrap_err().kind, QueryErrorKind::Type);
    assert_eq!(resources.used(), 0);
}

#[test]
fn truncate_units_preserve_case_null_type_and_error_semantics_without_string_copies() {
    for (name, value, unit) in [
        (
            "date.truncate",
            Value::Date(DateValue::parse("2000-06-15").unwrap()),
            "mOnTh",
        ),
        (
            "localtime.truncate",
            Value::LocalTime(LocalTimeValue::parse("12:34:56.123456789").unwrap()),
            "mIcRoSeCoNd",
        ),
        (
            "time.truncate",
            Value::Time(TimeValue::parse("12:34:56+08:00").unwrap()),
            "hOuR",
        ),
        (
            "localdatetime.truncate",
            Value::LocalDateTime(LocalDateTimeValue::parse("2000-06-15T12:34:56").unwrap()),
            "wEeK",
        ),
        (
            "datetime.truncate",
            Value::ZonedDateTime(ZonedDateTimeValue::parse("2000-06-15T12:34:56Z", "Z").unwrap()),
            "yEaR",
        ),
    ] {
        let arguments = [Value::String(unit.into()), value.clone()];
        let canonical = canonical_truncate_unit(unit).unwrap();
        assert_eq!(
            evaluate(name, &arguments).unwrap(),
            evaluate(name, &[Value::String(canonical.into()), value.clone()]).unwrap(),
        );
        for unit in ["x".repeat(1024 * 1024), "ｍｏｎｔｈ".into()] {
            assert!(canonical_truncate_unit(&unit).is_none());
            assert_eq!(
                evaluate(name, &[Value::String(unit), value.clone()])
                    .unwrap()
                    .unwrap_err()
                    .kind,
                QueryErrorKind::Semantic
            );
        }
        assert_eq!(
            evaluate(name, &[Value::Null, value.clone()])
                .unwrap()
                .unwrap(),
            Value::Null
        );
        assert_eq!(
            evaluate(name, &[Value::Integer(1), value])
                .unwrap()
                .unwrap_err()
                .kind,
            QueryErrorKind::Type
        );
    }
    for unit in [
        "millennium",
        "century",
        "decade",
        "year",
        "weekyear",
        "quarter",
        "month",
        "week",
        "day",
        "hour",
        "minute",
        "second",
        "millisecond",
        "microsecond",
        "nanosecond",
    ] {
        assert_eq!(
            canonical_truncate_unit(&unit.to_ascii_uppercase()),
            Some(unit)
        );
    }
}
