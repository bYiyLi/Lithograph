use crate::cypher::DurationValue;

use super::super::QueryResult;
use super::temporal::{invalid_pattern_component, pad_number, render_pattern};

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
    let parts = duration_parts(value, pattern);
    render_pattern(pattern, |character, count| {
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
    })
}

fn duration_parts(value: &DurationValue, pattern: &str) -> DurationParts {
    let (months, days, seconds, nanoseconds) = value.components();
    let has = |characters: &[char]| pattern.chars().any(|value| characters.contains(&value));
    let (years, quarters, months) =
        split_months(i128::from(months), has(&['y', 'Y', 'u']), has(&['q', 'Q']));
    let (weeks, days) = if has(&['w', 'W']) {
        (i128::from(days) / 7, i128::from(days) % 7)
    } else {
        (0, i128::from(days))
    };
    let total_nanoseconds = i128::from(seconds) * NANOS_PER_SECOND + i128::from(nanoseconds);
    let (hours, minutes, seconds, nanoseconds) = split_seconds(total_nanoseconds, pattern);
    DurationParts {
        years,
        quarters,
        months,
        weeks,
        days,
        hours,
        minutes,
        seconds,
        nanoseconds,
    }
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

fn split_seconds(total: i128, pattern: &str) -> (i128, i128, i128, i128) {
    let hours_present = pattern
        .chars()
        .any(|value| matches!(value, 'h' | 'H' | 'k' | 'K'));
    let minutes_present = pattern.contains('m');
    let seconds_present = pattern.contains('s');
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
