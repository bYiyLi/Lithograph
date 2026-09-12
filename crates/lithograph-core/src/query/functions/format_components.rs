use chrono::{Offset, TimeZone};

use super::super::{QueryError, QueryResult};
use super::temporal::{InstantParts, invalid_pattern_component, pad_number};

pub(super) fn format_localized_offset(offset: i32, width: usize) -> QueryResult<String> {
    if !matches!(width, 1 | 4) {
        return Err(QueryError::semantic(
            "localized timezone offset requires width one or four",
        ));
    }
    if offset == 0 {
        return Ok("GMT".to_owned());
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let absolute = offset.unsigned_abs();
    let hour = absolute / 3_600;
    let minute = absolute % 3_600 / 60;
    let second = absolute % 60;
    if width == 1 && minute == 0 && second == 0 {
        Ok(format!("GMT{sign}{hour}"))
    } else if second == 0 {
        Ok(format!("GMT{sign}{hour:02}:{minute:02}"))
    } else {
        Ok(format!("GMT{sign}{hour:02}:{minute:02}:{second:02}"))
    }
}

pub(super) fn format_era(year: i64, width: usize) -> QueryResult<String> {
    let common_era = year > 0;
    Ok(match width {
        1..=3 => if common_era { "AD" } else { "BC" }.to_owned(),
        4 => if common_era {
            "Anno Domini"
        } else {
            "Before Christ"
        }
        .to_owned(),
        5 => if common_era { "A" } else { "B" }.to_owned(),
        _ => return Err(QueryError::semantic("unsupported era format width")),
    })
}

pub(super) fn year_of_era(year: i64) -> i64 {
    if year > 0 { year } else { 1 - year }
}

pub(super) fn format_year(year: i64, width: usize) -> String {
    if width == 2 {
        return format!("{:02}", year.rem_euclid(100));
    }
    pad_number(i128::from(year), width)
}

pub(super) fn format_quarter(quarter: u32, width: usize) -> QueryResult<String> {
    Ok(match width {
        1 => quarter.to_string(),
        2 => format!("{quarter:02}"),
        3 => format!("Q{quarter}"),
        4 => format!("{quarter}{} quarter", ordinal_suffix(quarter)),
        5 => quarter.to_string(),
        _ => return Err(QueryError::semantic("unsupported quarter format width")),
    })
}

fn ordinal_suffix(value: u32) -> &'static str {
    match value % 100 {
        11..=13 => "th",
        _ => match value % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    }
}

pub(super) fn format_weekday(days: i64, width: usize, localized: bool) -> QueryResult<String> {
    let iso = (days + 3).rem_euclid(7) as u32 + 1;
    if localized && width <= 2 {
        let sunday_first = iso % 7 + 1;
        return Ok(if width == 2 {
            format!("{sunday_first:02}")
        } else {
            sunday_first.to_string()
        });
    }
    Ok(match width {
        1..=3 => weekday_name(days, false).to_owned(),
        4 => weekday_name(days, true).to_owned(),
        5 => weekday_name(days, true)[..1].to_owned(),
        6 => weekday_name(days, false).to_owned(),
        _ => return Err(QueryError::semantic("unsupported weekday format width")),
    })
}

pub(super) fn iso_week_parts(days: i64) -> (i64, u32) {
    let weekday = (days + 3).rem_euclid(7) + 1;
    let thursday = days + 4 - weekday;
    let week_year = crate::cypher::civil_from_days(thursday).0;
    let january_fourth = crate::cypher::days_from_civil(week_year, 1, 4);
    let first_monday = january_fourth - (january_fourth + 3).rem_euclid(7);
    (week_year, ((days - first_monday) / 7 + 1) as u32)
}

pub(super) fn period_of_day(hour: u32) -> &'static str {
    match hour {
        0..=5 | 21..=23 => "at night",
        6..=11 => "in the morning",
        12..=17 => "in the afternoon",
        _ => "in the evening",
    }
}

pub(super) fn zone_id(parts: InstantParts<'_>) -> QueryResult<String> {
    parts
        .zone
        .map(str::to_owned)
        .or_else(|| parts.offset.map(crate::cypher::format_offset))
        .ok_or_else(|| invalid_pattern_component('V'))
}

pub(super) fn zone_name(parts: InstantParts<'_>) -> QueryResult<String> {
    let Some(zone) = parts.zone else {
        return parts
            .offset
            .map(crate::cypher::format_offset)
            .ok_or_else(|| invalid_pattern_component('z'));
    };
    let (Some((year, month, day, _)), Some(time), Some(expected_offset)) =
        (parts.date, parts.time, parts.offset)
    else {
        return Ok(zone.to_owned());
    };
    let Ok(timezone) = zone.parse::<chrono_tz::Tz>() else {
        return Ok(zone.to_owned());
    };
    let Ok(year) = i32::try_from(year) else {
        return Ok(zone.to_owned());
    };
    let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month, day) else {
        return Ok(zone.to_owned());
    };
    let Some(clock) = chrono::NaiveTime::from_num_seconds_from_midnight_opt(
        (time / 1_000_000_000) as u32,
        (time % 1_000_000_000) as u32,
    ) else {
        return Ok(zone.to_owned());
    };
    let local = date.and_time(clock);
    let candidate = match timezone.from_local_datetime(&local) {
        chrono::LocalResult::Single(value) => Some(value),
        chrono::LocalResult::Ambiguous(first, second) => [first, second]
            .into_iter()
            .find(|value| value.offset().fix().local_minus_utc() == expected_offset),
        chrono::LocalResult::None => None,
    };
    Ok(candidate.map_or_else(|| zone.to_owned(), |value| value.format("%Z").to_string()))
}

pub(super) fn month_name(month: u32, long: bool) -> &'static str {
    const SHORT: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const LONG: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    if long {
        LONG[(month - 1) as usize]
    } else {
        SHORT[(month - 1) as usize]
    }
}

fn weekday_name(days: i64, long: bool) -> &'static str {
    const SHORT: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const LONG: [&str; 7] = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    let index = (days + 3).rem_euclid(7) as usize;
    if long { LONG[index] } else { SHORT[index] }
}
