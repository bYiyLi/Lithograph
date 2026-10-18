use crate::cypher::DurationValue;

use super::super::QueryResult;
use super::temporal::{invalid_pattern_component, pad_number, render_pattern};
use crate::query::resources::{ExecutionControl, MemoryReservation};

const NANOS_PER_SECOND: i128 = 1_000_000_000;

#[derive(Clone, Copy)]
struct DurationParts {
    years: i128,
    quarters: i128,
    months: i128,
    weeks: i128,
    days: i128,
    hours: i128,
    minutes: i128,
    seconds: i128,
    nanoseconds: i128,
}

pub(super) fn format_duration(value: &DurationValue, pattern: &str) -> QueryResult<String> {
    let parts = duration_parts(value, pattern, None)?;
    render_pattern(pattern, |character, count| {
        duration_component(parts, character, count)
    })
}

pub(super) fn format_duration_controlled(
    value: &DurationValue,
    pattern: &str,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<String> {
    let parts = duration_parts(value, pattern, control)?;
    super::temporal::controlled_pattern::render(
        pattern,
        control,
        128,
        result_memory,
        |character, count| {
            let width = if count > 2 { 1 } else { count };
            Ok((duration_component(parts, character, width)?, true))
        },
    )
}

fn duration_component(parts: DurationParts, character: char, count: usize) -> QueryResult<String> {
    let value = match character {
        'y' | 'Y' | 'u' => parts.years,
        'q' | 'Q' => parts.quarters,
        'M' | 'L' => parts.months,
        'w' | 'W' => parts.weeks,
        'd' | 'D' => parts.days,
        'h' | 'H' | 'k' | 'K' => parts.hours,
        'm' => parts.minutes,
        's' => parts.seconds,
        'n' | 'S' => parts.nanoseconds,
        'A' => parts.nanoseconds / 1_000_000,
        'N' => parts.nanoseconds,
        _ => return Err(invalid_pattern_component(character)),
    };
    Ok(pad_number(value, count))
}

fn duration_parts(
    value: &DurationValue,
    pattern: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<DurationParts> {
    let (months, days, seconds, nanoseconds) = value.components();
    let mut present = [false; 6];
    for character in pattern.chars() {
        if let Some(control) = control {
            control.work(1)?;
        }
        match character {
            'y' | 'Y' | 'u' => present[0] = true,
            'q' | 'Q' => present[1] = true,
            'w' | 'W' => present[2] = true,
            'h' | 'H' | 'k' | 'K' => present[3] = true,
            'm' => present[4] = true,
            's' => present[5] = true,
            _ => {}
        }
    }
    let (years, quarters, months) = split_months(i128::from(months), present[0], present[1]);
    let (weeks, days) = if present[2] {
        (i128::from(days) / 7, i128::from(days) % 7)
    } else {
        (0, i128::from(days))
    };
    let total_nanoseconds = i128::from(seconds) * NANOS_PER_SECOND + i128::from(nanoseconds);
    let (hours, minutes, seconds, nanoseconds) =
        split_seconds(total_nanoseconds, present[3], present[4], present[5]);
    Ok(DurationParts {
        years,
        quarters,
        months,
        weeks,
        days,
        hours,
        minutes,
        seconds,
        nanoseconds,
    })
}

fn split_months(total: i128, years_present: bool, quarters_present: bool) -> (i128, i128, i128) {
    let years = if years_present { total / 12 } else { 0 };
    let after_years = if years_present { total % 12 } else { total };
    let quarters = if quarters_present { after_years / 3 } else { 0 };
    let months = if quarters_present {
        after_years % 3
    } else {
        after_years
    };
    (years, quarters, months)
}

fn split_seconds(
    total: i128,
    hours_present: bool,
    minutes_present: bool,
    seconds_present: bool,
) -> (i128, i128, i128, i128) {
    let mut remaining = total;
    let hours = if hours_present {
        let value = remaining / (3_600 * NANOS_PER_SECOND);
        remaining %= 3_600 * NANOS_PER_SECOND;
        value
    } else {
        0
    };
    let minutes = if minutes_present {
        let value = remaining / (60 * NANOS_PER_SECOND);
        remaining %= 60 * NANOS_PER_SECOND;
        value
    } else {
        0
    };
    let seconds = if seconds_present {
        let value = remaining / NANOS_PER_SECOND;
        remaining %= NANOS_PER_SECOND;
        value
    } else {
        0
    };
    (hours, minutes, seconds, remaining)
}
