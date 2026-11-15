//! Tells the watchers of a repository when its security status moves (docs/DEPENDENCIES.md
//! "Notifications"; the `security_status` event of [`crate::notify`]).
//!
//! A notice goes out when the status changes between clean and findings, when it turns findings
//! from unknown, when a scan opens new critical or high findings, or when it has been unknown for two
//! scans in a row (one unknown scan, for example a registry hiccup, is not told). The first scan of a
//! repository only sets the baseline, unless it opens critical or high findings. The last status told
//! is kept in `deps_scans` ([`store::notified`]); the people are found by the hub (`watchers`).

use std::collections::BTreeMap;

use sqlx::AnyPool;

use super::hub::hex;
use super::status::{RepoStatus, Verdict};
use super::store;
use crate::notify::{self, Event, Kind};

/// Consecutive unknown scans before the move to unknown is told.
pub const UNKNOWN_RUNS: i64 = 2;

/// `deps_scans.status` of a repository nothing was told about yet.
pub const NONE: i64 = 0;

/// The stored code of a verdict.
pub fn code(v: Verdict) -> i64 {
    match v {
        Verdict::Unknown => 1,
        Verdict::Clean => 2,
        Verdict::Findings => 3,
    }
}

fn word(v: Verdict) -> &'static str {
    match v {
        Verdict::Unknown => "unknown",
        Verdict::Clean => "clean",
        Verdict::Findings => "findings",
    }
}

fn count(by_severity: &BTreeMap<String, u32>, key: &str) -> u32 {
    by_severity.get(key).copied().unwrap_or(0)
}

/// Open critical and high findings.
pub fn high_of(by_severity: &BTreeMap<String, u32>) -> u32 {
    count(by_severity, "critical").saturating_add(count(by_severity, "high"))
}

/// What to do about one scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub send: bool,
    /// What to store as the last status and the unknown streak.
    pub status: i64,
    pub unknown_runs: i64,
}

/// Decides from the stored `(status, unknown_runs)`, the new verdict and whether the scan opened
/// critical or high findings. Pure.
pub fn decide(old: (i64, i64), new: Verdict, grew_high: bool) -> Decision {
    let (last, runs) = old;
    if new == Verdict::Unknown {
        let runs = runs.saturating_add(1);
        if last == code(Verdict::Unknown) {
            return Decision {
                send: false,
                status: last,
                unknown_runs: runs,
            };
        }
        if runs >= UNKNOWN_RUNS {
            // From a known status it is told; from none it is the baseline.
            return Decision {
                send: last != NONE,
                status: code(Verdict::Unknown),
                unknown_runs: runs,
            };
        }
        return Decision {
            send: false,
            status: last,
            unknown_runs: runs,
        };
    }
    let moved = match new {
        Verdict::Findings => last == code(Verdict::Clean) || last == code(Verdict::Unknown),
        _ => last == code(Verdict::Findings),
    };
    Decision {
        send: moved || (new == Verdict::Findings && grew_high),
        status: code(new),
        unknown_runs: 0,
    }
}

/// The notice for `status` of `repo`.
pub fn event(repo: &str, status: &RepoStatus) -> Event {
    let by = &status.open_by_severity;
    let mut title = format!("Security status of {repo}: {}", word(status.verdict));
    let all: Vec<String> = ["critical", "high", "moderate", "low", "unknown"]
        .into_iter()
        .filter(|k| count(by, k) > 0)
        .map(|k| format!("{} {k}", count(by, k)))
        .collect();
    match status.verdict {
        Verdict::Findings => {
            let top: Vec<String> = ["critical", "high"]
                .into_iter()
                .filter(|k| count(by, k) > 0)
                .map(|k| format!("{} {k}", count(by, k)))
                .collect();
            let shown = if top.is_empty() {
                all.iter().take(1).cloned().collect()
            } else {
                top
            };
            if !shown.is_empty() {
                title.push_str(&format!(" ({})", shown.join(", ")));
            }
        }
        Verdict::Unknown => {
            let r = status.reason.text();
            if !r.is_empty() {
                title.push_str(&format!(" ({r})"));
            }
        }
        Verdict::Clean => {}
    }
    Event::new(Kind::SecurityStatus, title)
        .detail(all.join(", "))
        .path(notify::paths::deps(repo))
        .dedup(dedup_key(repo, code(status.verdict), &status.scanned))
        .watchers()
}

/// `deps:<repo>:<status>:<scanned hex>`: one notice per status of one scanned changeset.
pub fn dedup_key(repo: &str, status: i64, scanned: &[u8]) -> String {
    format!("deps:{repo}:{status}:{}", hex(scanned))
}

/// Called after a trunk scan was recorded: decides, stores the new last status and queues the
/// notice. Best effort: a failure is logged and the scan goes on.
pub async fn status_changed(
    pool: &AnyPool,
    repo: &str,
    repo_id: i64,
    status: &RepoStatus,
    grew_high: bool,
) {
    let old = match store::notified(pool, repo_id).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(event = "deps.notice", repo, error = %e, "last status not read");
            return;
        }
    };
    let d = decide(old, status.verdict, grew_high);
    // Queue first and store after: a notice that could not be queued is tried again by the next scan.
    if d.send
        && let Err(e) = notify::record(pool, repo, event(repo, status)).await
    {
        tracing::warn!(event = "deps.notice", repo, error = %e, "notice not queued");
        return;
    }
    if let Err(e) = store::set_notified(pool, repo_id, d.status, d.unknown_runs).await {
        tracing::warn!(event = "deps.notice", repo, error = %e, "status not stored");
    }
}

#[cfg(test)]
mod tests {
    use super::super::status::Reason;
    use super::*;
    use crate::migrate;
    use crate::notify::tests::notes;

    const CLEAN: i64 = 2;
    const FINDINGS: i64 = 3;
    const UNKNOWN: i64 = 1;

    fn st(v: Verdict, crit: u32) -> RepoStatus {
        let mut m = BTreeMap::new();
        m.insert("critical".to_owned(), crit);
        m.insert("high".to_owned(), 0);
        RepoStatus {
            verdict: v,
            reason: if v == Verdict::Unknown {
                Reason::Stale
            } else {
                Reason::None
            },
            open_by_severity: m,
            moves: Vec::new(),
            scanned_at: 10,
            scanned: vec![0xab, 0xcd],
        }
    }

    async fn setup() -> AnyPool {
        let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        sqlx::query("INSERT INTO repos (name, ns, created) VALUES ('acme/api', $1, 1)")
            .bind(vec![7u8; 4])
            .execute(&pool)
            .await
            .unwrap();
        store::put_scan(
            &pool,
            1,
            &store::ScanRow {
                at: 1,
                rev: vec![1],
                lookup: 1,
                packages: 1,
                unknown: 0,
                by_ecosystem: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        pool
    }

    #[test]
    fn clean_and_findings_move_but_the_same_status_is_silent() {
        assert!(decide((CLEAN, 0), Verdict::Findings, false).send);
        assert!(decide((FINDINGS, 0), Verdict::Clean, false).send);
        assert!(decide((UNKNOWN, 2), Verdict::Findings, false).send);
        assert!(!decide((CLEAN, 0), Verdict::Clean, false).send);
        assert!(!decide((FINDINGS, 0), Verdict::Findings, false).send);
        assert!(!decide((UNKNOWN, 2), Verdict::Clean, false).send);
        // The first scan is a baseline.
        assert!(!decide((NONE, 0), Verdict::Clean, false).send);
        assert!(!decide((NONE, 0), Verdict::Findings, false).send);
        assert_eq!(decide((NONE, 0), Verdict::Findings, false).status, FINDINGS);
    }

    #[test]
    fn new_critical_or_high_findings_are_told_again() {
        assert!(decide((FINDINGS, 0), Verdict::Findings, true).send);
        assert!(decide((NONE, 0), Verdict::Findings, true).send);
        assert!(!decide((CLEAN, 0), Verdict::Clean, true).send);
    }

    #[test]
    fn unknown_is_told_only_after_two_in_a_row() {
        let first = decide((CLEAN, 0), Verdict::Unknown, false);
        assert!(!first.send);
        assert_eq!((first.status, first.unknown_runs), (CLEAN, 1));
        let second = decide((first.status, first.unknown_runs), Verdict::Unknown, false);
        assert!(second.send);
        assert_eq!((second.status, second.unknown_runs), (UNKNOWN, 2));
        // Staying unknown is silent; a known scan between resets the streak.
        assert!(!decide((UNKNOWN, 2), Verdict::Unknown, false).send);
        let back = decide((CLEAN, 1), Verdict::Clean, false);
        assert_eq!(back.unknown_runs, 0);
        assert!(!decide((NONE, 0), Verdict::Unknown, false).send);
        assert!(!decide((NONE, 1), Verdict::Unknown, false).send);
    }

    #[test]
    fn the_dedup_key_and_text_are_stable() {
        let a = event("acme/api", &st(Verdict::Findings, 2));
        let b = event("acme/api", &st(Verdict::Findings, 5));
        assert_eq!(a.dedup, "deps:acme/api:3:abcd");
        assert_eq!(a.dedup, b.dedup);
        assert_eq!(
            dedup_key("acme/api", 2, &[0xab, 0xcd]),
            "deps:acme/api:2:abcd"
        );
        assert!(a.watchers && a.users.is_empty());
        assert_eq!(a.kind, Kind::SecurityStatus);
        assert_eq!(
            a.title,
            "Security status of acme/api: findings (2 critical)"
        );
        assert_eq!(a.path, "/acme/api/asc/deps");
    }

    #[tokio::test]
    async fn a_change_is_emitted_once_and_the_same_status_is_silent() {
        let pool = setup().await;
        status_changed(&pool, "acme/api", 1, &st(Verdict::Clean, 0), false).await;
        assert!(notes(&pool).await.is_empty(), "baseline");
        status_changed(&pool, "acme/api", 1, &st(Verdict::Findings, 2), false).await;
        let n = notes(&pool).await;
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].0, "security_status");
        assert!(n[0].1.is_empty());
        assert_eq!(n[0].2, "Security status of acme/api: findings (2 critical)");
        status_changed(&pool, "acme/api", 1, &st(Verdict::Findings, 2), false).await;
        assert_eq!(notes(&pool).await.len(), 1);
        assert_eq!(store::notified(&pool, 1).await.unwrap(), (FINDINGS, 0));
        // Unknown twice: one notice, on the second.
        status_changed(&pool, "acme/api", 1, &st(Verdict::Unknown, 0), false).await;
        assert_eq!(notes(&pool).await.len(), 1);
        status_changed(&pool, "acme/api", 1, &st(Verdict::Unknown, 0), false).await;
        assert_eq!(notes(&pool).await.len(), 2);
    }

    #[test]
    fn the_watchers_flag_is_sent_only_when_set() {
        use crate::notify::worker::Outgoing;
        let mut o = Outgoing {
            event: "security_status".into(),
            repo: "acme/api".into(),
            users: Vec::new(),
            title: "t".into(),
            detail: String::new(),
            path: String::new(),
            watchers: true,
        };
        let on = serde_json::to_value(&o).unwrap();
        assert_eq!(on["watchers"], true);
        assert_eq!(on["users"], serde_json::json!([]));
        o.watchers = false;
        let off = serde_json::to_value(&o).unwrap();
        assert!(off.get("watchers").is_none());
    }

    #[tokio::test]
    async fn the_queued_row_keeps_the_watchers_flag() {
        let pool = setup().await;
        status_changed(&pool, "acme/api", 1, &st(Verdict::Clean, 0), false).await;
        status_changed(&pool, "acme/api", 1, &st(Verdict::Findings, 1), false).await;
        let w: i64 = sqlx::query_scalar("SELECT CAST(watchers AS BIGINT) FROM notify_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(w, 1);
    }

    #[tokio::test]
    async fn a_notice_that_could_not_be_queued_is_tried_again_by_the_next_scan() {
        let pool = setup().await;
        status_changed(&pool, "acme/api", 1, &st(Verdict::Clean, 0), false).await;
        sqlx::query("ALTER TABLE notify_events RENAME TO notify_events_off")
            .execute(&pool)
            .await
            .unwrap();
        status_changed(&pool, "acme/api", 1, &st(Verdict::Findings, 2), false).await;
        assert_eq!(store::notified(&pool, 1).await.unwrap(), (CLEAN, 0));
        sqlx::query("ALTER TABLE notify_events_off RENAME TO notify_events")
            .execute(&pool)
            .await
            .unwrap();
        status_changed(&pool, "acme/api", 1, &st(Verdict::Findings, 2), false).await;
        assert_eq!(notes(&pool).await.len(), 1);
        assert_eq!(store::notified(&pool, 1).await.unwrap(), (FINDINGS, 0));
    }
}
