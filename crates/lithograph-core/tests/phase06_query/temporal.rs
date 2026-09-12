use super::*;

#[test]
fn named_timezone_rules_validate_dst_and_survive_history_storage() {
    let connection = fresh_storage();
    let created = rows(
        &connection,
        "CREATE (:Event {summer: datetime('2024-07-01T12:00:00+02:00[Europe/Paris]'), overlapEarly: datetime('2024-10-27T02:30:00+02:00[Europe/Paris]'), overlapLate: datetime('2024-10-27T02:30:00+01:00[Europe/Paris]')}) RETURN 1 AS created",
    );
    assert_eq!(created, vec![vec![Value::Integer(1)]]);
    let result = rows(
        &connection,
        "MATCH (event:Event) RETURN event.summer, event.overlapEarly, event.overlapLate",
    );
    assert!(matches!(
        &result[0][0],
        Value::ZonedDateTime(value)
            if value.value() == "2024-07-01T12:00:00+02:00" && value.zone() == "Europe/Paris"
    ));
    assert!(matches!(
        &result[0][1],
        Value::ZonedDateTime(value) if value.value() == "2024-10-27T02:30:00+02:00"
    ));
    assert!(matches!(
        &result[0][2],
        Value::ZonedDateTime(value) if value.value() == "2024-10-27T02:30:00+01:00"
    ));
    assert!(
        execution_error(
            &connection,
            "RETURN datetime('2024-03-31T02:30:00+01:00[Europe/Paris]') AS missingLocalTime",
        )
        .contains("zone rules do not match")
    );
    assert!(
        execution_error(
            &connection,
            "RETURN datetime('2024-07-01T12:00:00+01:00[Europe/Paris]') AS wrongOffset",
        )
        .contains("zone rules do not match")
    );
}

#[test]
fn temporal_arithmetic_differences_truncation_and_patterns_execute() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "RETURN format(date('2024-01-31') + duration('P1M')) AS clamped, format(date.truncate('month', date('2024-02-29'))) AS truncated, format(datetime.fromEpoch(0, 0)) AS epoch, format(duration.between(date('1984-10-11'), date('1985-11-25'))) AS logical, format(duration.inDays(date('1984-10-11'), date('1985-11-25'))) AS days, format(date('1986-11-18'), 'MM/dd/yyyy') AS patterned",
        ),
        vec![vec![
            Value::String("2024-02-29".to_owned()),
            Value::String("2024-02-01".to_owned()),
            Value::String("1970-01-01T00:00:00Z".to_owned()),
            Value::String("P1Y1M14D".to_owned()),
            Value::String("P410D".to_owned()),
            Value::String("11/18/1986".to_owned()),
        ]]
    );
    assert_eq!(
        rows(
            &connection,
            "RETURN format(duration({years: 1, months: 4, weeks: 3, days: 4, hours: 5, minutes: 6, seconds: 7, milliseconds: 8, microseconds: 9, nanoseconds: 10}), \"y 'years' q 'quarters' M 'months' w 'weeks' d 'days' h 'hours' m 'minutes' s 'seconds' N 'nanos'\") AS rendered, format(duration('5 hours 6 minutes', \"h 'hours' m 'minutes'\")) AS parsed",
        ),
        vec![vec![
            Value::String(
                "1 years 1 quarters 1 months 3 weeks 4 days 5 hours 6 minutes 7 seconds 8009010 nanos"
                    .to_owned(),
            ),
            Value::String("PT5H6M".to_owned()),
        ]]
    );

    assert_eq!(
        rows(
            &connection,
            "WITH datetime('1986-11-18T6:04:45.123456789+01:00[Europe/Berlin]') AS dt RETURN format(dt, 'EEEE, MMMM d, G uuuu'), format(dt, \"DDD'nd day of the year,' c'rd day of the week'\"), format(dt, 'k:mm z'), format(dt, 'K:mm O'), format(dt, \"LLL d,' minute 'm', second 's', millisecond of the day 'A\"), format(dt, 'pppYY')",
        ),
        vec![vec![
            Value::String("Tuesday, November 18, AD 1986".to_owned()),
            Value::String("322nd day of the year, 3rd day of the week".to_owned()),
            Value::String("6:04 CET".to_owned()),
            Value::String("6:04 GMT+1".to_owned()),
            Value::String(
                "Nov 18, minute 4, second 45, millisecond of the day 21885123".to_owned(),
            ),
            Value::String(" 86".to_owned()),
        ]]
    );
    assert_eq!(
        rows(
            &connection,
            "RETURN format(date('Tuesday, November 18, AD 1986', 'EEEE, MMMM d, G uuuu')), format(localdatetime('Tuesday, November 18, AD 1986', 'EEEE, MMMM d, G uuuu')), format(datetime('Tuesday, November 18, AD 1986', 'EEEE, MMMM d, G uuuu'))",
        ),
        vec![vec![
            Value::String("1986-11-18".to_owned()),
            Value::String("1986-11-18T00:00:00".to_owned()),
            Value::String("1986-11-18T00:00:00Z".to_owned()),
        ]]
    );
    assert_eq!(
        rows(
            &connection,
            "RETURN format(time('06:04', 'HH:mm')), format(time('06:04+01:00', 'HH:mmXXX'))",
        ),
        vec![vec![
            Value::String("06:04:00Z".to_owned()),
            Value::String("06:04:00+01:00".to_owned()),
        ]]
    );
}

#[test]
fn temporal_constructors_truncation_and_component_properties_match_the_profile() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "RETURN format(date('2015-W30-2')), format(date('2015202')), format(date('2015-Q2-60')), format(date({year: 1984, week: 10, dayOfWeek: 3})), format(localdatetime('2015-W30-2T214032.142')), format(datetime('2015-07-21T21:40:32.142[Europe/London]'))",
        ),
        vec![vec![
            Value::String("2015-07-21".to_owned()),
            Value::String("2015-07-21".to_owned()),
            Value::String("2015-05-30".to_owned()),
            Value::String("1984-03-07".to_owned()),
            Value::String("2015-07-21T21:40:32.142".to_owned()),
            Value::String("2015-07-21T21:40:32.142+01:00[Europe/London]".to_owned()),
        ]]
    );

    assert_eq!(
        rows(
            &connection,
            "WITH datetime({year: 1984, month: 11, day: 11, hour: 12, minute: 31, second: 14, nanosecond: 645876123, timezone: 'Europe/Stockholm'}) AS d, duration('P1Y5M10DT12H34M56.123456789S') AS span, point({longitude: 181, latitude: 30}) AS p RETURN d.weekYear, d.week, d.dayOfWeek, d.dayOfQuarter, d.millisecond, d.microsecond, d.offset, d.offsetMinutes, span.years, span.monthsOfYear, span.weeks, span.daysOfWeek, span.minutesOfHour, span.nanosecondsOfSecond, p.longitude, p.latitude, p.srid",
        ),
        vec![vec![
            Value::Integer(1984),
            Value::Integer(45),
            Value::Integer(7),
            Value::Integer(42),
            Value::Integer(645),
            Value::Integer(645_876),
            Value::String("+01:00".to_owned()),
            Value::Integer(60),
            Value::Integer(1),
            Value::Integer(5),
            Value::Integer(1),
            Value::Integer(3),
            Value::Integer(34),
            Value::Integer(123_456_789),
            Value::Float(-179.0),
            Value::Float(30.0),
            Value::Integer(4_326),
        ]]
    );

    assert_eq!(
        rows(
            &connection,
            "WITH datetime({year: 2017, month: 11, day: 11, hour: 12, minute: 31, second: 14, nanosecond: 645876123, timezone: '+03:00'}) AS d, time({hour: 12, minute: 31, second: 14, nanosecond: 645876123, timezone: '-01:00'}) AS t RETURN format(date.truncate('year', d, {day: 5})), format(date.truncate('week', d, {dayOfWeek: 2})), format(datetime.truncate('millennium', d, {timezone: 'Europe/Stockholm'})), format(datetime.truncate('day', d, {millisecond: 2})), format(localtime.truncate('minute', t, {millisecond: 2})), format(time.truncate('millisecond', t, {nanosecond: 2}))",
        ),
        vec![vec![
            Value::String("2017-01-05".to_owned()),
            Value::String("2017-11-07".to_owned()),
            Value::String("2000-01-01T00:00:00+01:00[Europe/Stockholm]".to_owned()),
            Value::String("2017-11-11T00:00:00.002+03:00".to_owned()),
            Value::String("12:31:00.002".to_owned()),
            Value::String("12:31:14.645000002-01:00".to_owned()),
        ]]
    );

    assert!(
        execution_error(
            &connection,
            "RETURN date.truncate('day', date('2024-01-01'), {month: 2})",
        )
        .contains("must be smaller")
    );

    assert_eq!(
        rows(
            &connection,
            "WITH time('12:31:14+01:00') AS t, datetime('1984-10-11T12:31:14+01:00') AS d RETURN format(time({time: t, timezone: '+05:00'})), format(datetime({datetime: d, timezone: '+05:00'})), format(time.truncate('hour', t, {timezone: '+05:00'})), toString(d), s\"{d}\"",
        ),
        vec![vec![
            Value::String("16:31:14+05:00".to_owned()),
            Value::String("1984-10-11T16:31:14+05:00".to_owned()),
            Value::String("12:00:00+05:00".to_owned()),
            Value::String("1984-10-11T12:31:14+01:00".to_owned()),
            Value::String("1984-10-11T12:31:14+01:00".to_owned()),
        ]]
    );

    for query in [
        "RETURN time({hour: 12, second: 1})",
        "RETURN localtime({nanosecond: 1})",
        "RETURN date({year: 2024, dayOfWeek: 2})",
        "RETURN date({year: 2024, weekDay: 2})",
        "RETURN localdatetime({year: 2024, timezone: '+01:00'})",
        "RETURN datetime({year: 2024, nonsense: 1})",
        "RETURN datetime({epochSeconds: 0, year: 2024})",
        "RETURN datetime({epochSeconds: 0, epochMillis: 0})",
    ] {
        assert!(
            !query_error(&connection, query).is_empty(),
            "query must fail: {query}"
        );
    }
}

#[test]
fn fractional_duration_forms_and_arithmetic_carry_into_smaller_components() {
    let connection = fresh_storage();
    assert_eq!(
        rows(
            &connection,
            "RETURN format(duration('P5M1.5D')), format(duration('P0.75M')), format(duration('PT0.75M')), format(duration('P2012-02-02T14:37:21.545')), format(duration('P1M') / 2), format(duration('PT1S') * 1.5)",
        ),
        vec![vec![
            Value::String("P5M1DT12H".to_owned()),
            Value::String("P22DT19H51M49.5S".to_owned()),
            Value::String("PT45S".to_owned()),
            Value::String("P2012Y2M2DT14H37M21.545S".to_owned()),
            Value::String("P15DT5H14M33S".to_owned()),
            Value::String("PT1.5S".to_owned()),
        ]]
    );
}

#[test]
fn statement_clocks_are_stable_across_cursor_batches_and_accept_timezones() {
    let connection = fresh_storage();
    let result = rows(
        &connection,
        "UNWIND range(1, 5) AS x RETURN timestamp() AS first, timestamp() AS second, datetime.statement('America/Los_Angeles') AS zoned, datetime.transaction() = datetime.statement() AS same_execution_clock",
    );
    assert_eq!(result.len(), 5);
    let first_timestamp = result[0][0].clone();
    for row in result {
        assert_eq!(row[0], first_timestamp);
        assert_eq!(row[1], first_timestamp);
        assert!(matches!(
            &row[2],
            Value::ZonedDateTime(value) if value.zone() == "America/Los_Angeles"
        ));
        assert_eq!(row[3], Value::Boolean(true));
    }
}

#[test]
fn relationship_endpoint_functions_and_procedure_signatures_are_checked() {
    let connection = fresh_storage();
    let _ = rows(
        &connection,
        "CREATE (a:N {name:'a'}), (b:N {name:'b'}) CREATE (a)-[:R]->(b) FINISH",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH ()-[r:R]->() RETURN startNode(r).name AS start, endNode(r).name AS end",
        ),
        vec![vec![
            Value::String("a".to_owned()),
            Value::String("b".to_owned()),
        ]]
    );
    assert!(
        execution_error(&connection, "CALL db.labels(1) YIELD label RETURN label")
            .contains("expects no arguments")
    );
}
