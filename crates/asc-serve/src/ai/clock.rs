//! Time as the AI accounting needs it: unix seconds, and the UTC day and month names.

use std::time::{SystemTime, UNIX_EPOCH};

/// Now, in unix seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `(year, month, day)` in UTC of a unix time.
pub fn ymd(unix: i64) -> (i64, u32, u32) {
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD` (UTC).
pub fn day(unix: i64) -> String {
    let (y, m, d) = ymd(unix);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM` (UTC).
pub fn month(unix: i64) -> String {
    let (y, m, _) = ymd(unix);
    format!("{y:04}-{m:02}")
}

/// The unix time of the first second of the month `unix` falls in.
pub fn month_start(unix: i64) -> i64 {
    let (y, m, _) = ymd(unix);
    days_from_civil(y, m, 1) * 86_400
}

/// The unix time of the first second of month `m` (1 to 12) of year `y`.
pub fn month_start_of(y: i64, m: u32) -> i64 {
    days_from_civil(y, m, 1) * 86_400
}

/// The first second of the month after the one `start` (a month start) begins.
pub fn next_month_start(start: i64) -> i64 {
    let (y, m, _) = ymd(start);
    if m == 12 {
        month_start_of(y + 1, 1)
    } else {
        month_start_of(y, m + 1)
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_and_months_are_utc() {
        assert_eq!(day(0), "1970-01-01");
        assert_eq!(day(86_399), "1970-01-01");
        assert_eq!(day(86_400), "1970-01-02");
        assert_eq!(day(1_792_983_600), "2026-10-26");
        assert_eq!(month(1_792_983_600), "2026-10");
        assert_eq!(day(951_782_400), "2000-02-29");
        assert_eq!(month(-1), "1969-12");
    }

    #[test]
    fn month_start_is_the_first_second() {
        assert_eq!(month_start(1_792_983_600), 1_790_812_800); // 2026-10-01T00:00:00Z
        assert_eq!(day(month_start(1_792_983_600)), "2026-10-01");
        assert_eq!(month_start(0), 0);
        assert_eq!(day(month_start(951_782_400)), "2000-02-01");
    }
}
