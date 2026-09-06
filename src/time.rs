//! RFC 3339 timestamps in UTC. The journal, `doctor`, and the later receipt and
//! bench chunks all record a time, so the conversion lives in one place. The C30
//! gh cache parses its own stamps back, so the conversion goes both ways.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current time as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn now_rfc3339() -> String {
    rfc3339(SystemTime::now())
}

/// `time` as `YYYY-MM-DDTHH:MM:SSZ` in UTC. Sub-second digits are dropped, so
/// two calls in the same second give the same text. A leap second is not
/// represented; Unix time has none.
pub fn rfc3339(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        // A clock before 1970 still produces a valid string.
        Err(err) => -i64::try_from(err.duration().as_secs()).unwrap_or(i64::MAX),
    };
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (rest / 3600, (rest % 3600) / 60, rest % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert a day count since 1970-01-01 into a proleptic Gregorian date.
/// The algorithm is Howard Hinnant's `civil_from_days`, which shifts the era
/// so that a leap day lands at the end of a 400-year cycle.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    // Move the epoch to 0000-03-01, the start of an era.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097); // 0 to 146096
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153; // 0 = March
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Parse the exact form `rfc3339` writes, `YYYY-MM-DDTHH:MM:SSZ`, back into
/// Unix seconds. klon only reads stamps of its own, so another RFC 3339 shape
/// (an offset, a fraction, lowercase) counts as unreadable.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return None;
    }
    let digits = |from: usize, to: usize| text.get(from..to)?.parse::<i64>().ok();
    let year = digits(0, 4)?;
    let month = digits(5, 7)?;
    let day = digits(8, 10)?;
    let hour = digits(11, 13)?;
    let minute = digits(14, 16)?;
    let second = digits(17, 19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// The inverse of `civil_from_days`: Howard Hinnant's `days_from_civil`, which
/// puts the leap day at the end of a 400-year era again.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100;
    era * 146_097 + day_of_era + day_of_year - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> String {
        rfc3339(UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn known_instants_match() {
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(1), "1970-01-01T00:00:01Z");
        assert_eq!(at(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(at(86_400), "1970-01-02T00:00:00Z");
        // 2000-02-29: the century leap day.
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        // 2100 is not a leap year: 2100-03-01 follows 2100-02-28.
        assert_eq!(at(4_107_456_000), "2100-02-28T00:00:00Z");
        assert_eq!(at(4_107_542_400), "2100-03-01T00:00:00Z");
        assert_eq!(at(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(at(1_767_225_599), "2025-12-31T23:59:59Z");
    }

    #[test]
    fn sub_second_digits_are_dropped() {
        let base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(rfc3339(base), rfc3339(base + Duration::from_millis(999)));
    }

    #[test]
    fn a_time_before_the_epoch_still_formats() {
        assert_eq!(
            rfc3339(UNIX_EPOCH - Duration::from_secs(1)),
            "1969-12-31T23:59:59Z"
        );
    }

    #[test]
    fn every_written_stamp_parses_back() {
        for secs in [
            0u64,
            1,
            951_782_400,
            1_700_000_000,
            1_767_225_599,
            4_107_542_400,
        ] {
            let text = at(secs);
            assert_eq!(parse_rfc3339(&text), Some(secs as i64), "{text}");
        }
    }

    #[test]
    fn a_stamp_klon_did_not_write_is_refused() {
        assert_eq!(parse_rfc3339(""), None);
        assert_eq!(parse_rfc3339("2026-09-06 10:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-09-06T10:00:00"), None);
        assert_eq!(parse_rfc3339("2026-09-06T10:00:00+00:00"), None);
        assert_eq!(parse_rfc3339("2026-9-06T10:00:00Z"), None);
        assert_eq!(parse_rfc3339("not a stamp"), None);
    }

    #[test]
    fn an_impossible_date_is_refused() {
        assert_eq!(parse_rfc3339("2026-13-06T10:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-00-06T10:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-09-32T10:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-09-06T24:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-09-06T10:60:00Z"), None);
        assert_eq!(parse_rfc3339("2026-09-06T10:00:60Z"), None);
    }

    #[test]
    fn days_and_dates_are_inverse() {
        for days in [-719_468i64, -1, 0, 1, 19_000, 25_000] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days, "{days}");
        }
    }
}
