//! An entry's timestamp: UTC, RFC 3339, with microseconds (design 12.2).

use std::time::{SystemTime, UNIX_EPOCH};

/// `time` as `YYYY-MM-DDThh:mm:ss.uuuuuuZ`. A time before 1970 is written as 1970's start; the
/// device clock is not trusted anyway (ADR-348 item 7).
pub(crate) fn utc_timestamp(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        since.subsec_micros()
    )
}

/// The proleptic Gregorian date `days` days after 1970-01-01 (Howard Hinnant's
/// `civil_from_days`, for non-negative days).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn timestamps_are_utc_with_microseconds() {
        assert_eq!(utc_timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000000Z");
        // 2000-02-29 (a leap day of a year divisible by 400) 12:34:56.789012.
        let time = UNIX_EPOCH + Duration::new(951_827_696, 789_012_345);
        assert_eq!(utc_timestamp(time), "2000-02-29T12:34:56.789012Z");
        // 2026-10-24 06:40:07, and the last second of 2024 (a leap year).
        let time = UNIX_EPOCH + Duration::from_secs(1_792_824_007);
        assert_eq!(utc_timestamp(time), "2026-10-24T06:40:07.000000Z");
        let time = UNIX_EPOCH + Duration::from_secs(1_735_689_599);
        assert_eq!(utc_timestamp(time), "2024-12-31T23:59:59.000000Z");
    }

    #[test]
    fn a_time_before_1970_is_1970() {
        let time = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(utc_timestamp(time), "1970-01-01T00:00:00.000000Z");
    }
}
