const NANOS_PER_SECOND: i128 = 1_000_000_000;

pub(super) fn format_duration(months: i64, days: i64, seconds: i64, nanoseconds: i64) -> String {
    use std::fmt::Write as _;

    let mut output = String::from("P");
    let years = months / 12;
    let remaining_months = months % 12;
    if years != 0 {
        let _ = write!(output, "{years}Y");
    }
    if remaining_months != 0 {
        let _ = write!(output, "{remaining_months}M");
    }
    if days != 0 {
        let _ = write!(output, "{days}D");
    }

    let total_nanoseconds = i128::from(seconds) * NANOS_PER_SECOND + i128::from(nanoseconds);
    if total_nanoseconds != 0 || output.len() == 1 {
        output.push('T');
        format_duration_time(total_nanoseconds, &mut output);
    }
    output
}

fn format_duration_time(total_nanoseconds: i128, output: &mut String) {
    use std::fmt::Write as _;

    if total_nanoseconds == 0 {
        output.push_str("0S");
        return;
    }
    let negative = total_nanoseconds < 0;
    let mut remaining = total_nanoseconds.abs();
    let hour_nanos = 3_600 * NANOS_PER_SECOND;
    let minute_nanos = 60 * NANOS_PER_SECOND;
    let hours = remaining / hour_nanos;
    remaining %= hour_nanos;
    let minutes = remaining / minute_nanos;
    remaining %= minute_nanos;
    let seconds = remaining / NANOS_PER_SECOND;
    let nanos = remaining % NANOS_PER_SECOND;
    let sign = if negative { "-" } else { "" };

    if hours != 0 {
        let _ = write!(output, "{sign}{hours}H");
    }
    if minutes != 0 {
        let _ = write!(output, "{sign}{minutes}M");
    }
    if seconds != 0 || nanos != 0 {
        if nanos == 0 {
            let _ = write!(output, "{sign}{seconds}S");
        } else {
            let fraction = format!("{nanos:09}").trim_end_matches('0').to_owned();
            let _ = write!(output, "{sign}{seconds}.{fraction}S");
        }
    }
}
