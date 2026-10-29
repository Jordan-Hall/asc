//! The arithmetic of a backup schedule and of retention. Pure, so it is tested without a clock.

use std::collections::BTreeSet;

/// Seconds in a day.
const DAY: i64 = 86_400;
const WEEK: i64 = 7 * DAY;

/// The first run strictly after `now`: the anchor is `minute_of_day` UTC of day 0 (1 January
/// 1970), and runs repeat every `interval_hours` from it, so "every 24 hours at 02:00" is the
/// anchor `120`.
pub fn next_run(now: i64, interval_hours: u32, minute_of_day: u32) -> i64 {
    let step = i64::from(interval_hours.clamp(1, 168)) * 3600;
    let anchor = i64::from(minute_of_day.min(1439)) * 60;
    // The smallest anchor + k * step greater than `now`.
    let k = (now - anchor).div_euclid(step) + 1;
    anchor + k * step
}

/// One finished backup, for retention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub id: String,
    pub finished: i64,
    pub succeeded: bool,
}

/// The ids to delete. Rules, in order:
///
/// * a backup younger than `keep_days` stays;
/// * of the older ones, the newest successful backup of each week stays for `keep_weeks` weeks
///   after the daily window;
/// * the newest successful backup always stays, whatever its age;
/// * a failed run older than `keep_days` goes (its record stays, its files are gone).
pub fn expired(runs: &[Kept], now: i64, keep_days: u32, keep_weeks: u32) -> Vec<String> {
    let daily_from = now - i64::from(keep_days) * DAY;
    let weekly_from = daily_from - i64::from(keep_weeks) * WEEK;
    let newest_ok = runs
        .iter()
        .filter(|r| r.succeeded)
        .max_by_key(|r| r.finished)
        .map(|r| r.id.as_str());
    // Newest first, so the first of each week is the one that stays.
    let mut order: Vec<&Kept> = runs.iter().collect();
    order.sort_by_key(|r| std::cmp::Reverse(r.finished));
    let mut weeks_seen = BTreeSet::new();
    let mut out = Vec::new();
    for r in order {
        if r.finished >= daily_from || Some(r.id.as_str()) == newest_ok {
            continue;
        }
        if !r.succeeded {
            out.push(r.id.clone());
            continue;
        }
        let week = r.finished.div_euclid(WEEK);
        if r.finished >= weekly_from && weeks_seen.insert(week) {
            continue;
        }
        out.push(r.id.clone());
    }
    out
}

/// Whether a schedule is overdue: it is on, and the last success is older than one and a half
/// intervals (never succeeded counts from `since`, when the schedule was set).
pub fn overdue(
    enabled: bool,
    now: i64,
    last_success: i64,
    since: i64,
    interval_hours: u32,
) -> bool {
    if !enabled {
        return false;
    }
    let limit = i64::from(interval_hours.max(1)) * 3600 * 3 / 2;
    now - last_success.max(since) > limit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_run_is_the_next_anchor_after_now() {
        // 02:00 UTC daily; now is 01:00 on day 10 -> 02:00 that day.
        let day10 = 10 * DAY;
        assert_eq!(next_run(day10 + 3600, 24, 120), day10 + 7200);
        // Exactly at the anchor -> the next one, a day later.
        assert_eq!(next_run(day10 + 7200, 24, 120), day10 + DAY + 7200);
        // Every 6 hours from 00:30.
        assert_eq!(next_run(day10 + 100, 6, 30), day10 + 1800);
        assert_eq!(next_run(day10 + 1800, 6, 30), day10 + 6 * 3600 + 1800);
        // Out-of-range input is clamped, never a division by zero.
        assert!(next_run(0, 0, 5000) > 0);
    }

    fn k(id: &str, days_ago: i64, ok: bool, now: i64) -> Kept {
        Kept {
            id: id.into(),
            finished: now - days_ago * DAY,
            succeeded: ok,
        }
    }

    #[test]
    fn retention_keeps_the_daily_window_then_one_per_week_then_nothing() {
        let now = 1_000 * DAY;
        let runs: Vec<Kept> = (0..120)
            .map(|d| k(&format!("r{d}"), d, true, now))
            .collect();
        let gone = expired(&runs, now, 30, 8);
        let kept = |id: &str| !gone.contains(&id.to_owned());
        assert!(kept("r0") && kept("r29"));
        // Beyond the daily window only one a week stays.
        let weekly: Vec<&Kept> = runs
            .iter()
            .filter(|r| r.finished < now - 30 * DAY && kept(&r.id))
            .collect();
        assert!(weekly.len() >= 7 && weekly.len() <= 9, "{}", weekly.len());
        // Older than the weekly window: gone.
        assert!(!kept("r119") && !kept("r100"));
    }

    #[test]
    fn the_newest_success_stays_and_old_failures_go() {
        let now = 1_000 * DAY;
        let runs = vec![
            k("old_ok", 400, true, now),
            k("old_fail", 90, false, now),
            k("fresh_fail", 1, false, now),
        ];
        let gone = expired(&runs, now, 30, 0);
        assert_eq!(gone, vec!["old_fail".to_owned()]);
    }

    #[test]
    fn overdue_counts_from_the_last_success_or_the_schedule_start() {
        let now = 1_000_000;
        assert!(!overdue(false, now, 0, 0, 24));
        assert!(!overdue(true, now, now - 3600, 0, 24));
        assert!(overdue(true, now, now - 40 * 3600, 0, 24));
        // Never succeeded, but the schedule is new.
        assert!(!overdue(true, now, 0, now - 3600, 24));
        assert!(overdue(true, now, 0, now - 40 * 3600, 24));
    }
}
