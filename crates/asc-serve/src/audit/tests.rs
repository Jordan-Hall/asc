//! Audit log tests over SQLite: append, scopes, filters, the hash chain under tampering,
//! retention, export and read access.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use asc_rpc::auth::{Access, StaticTokens, parse_authorization};
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::audit_server::Audit as AuditApi;
use sqlx::{AnyPool, Row};
use tonic::Request;

use super::export::{self, Format};
use super::retention;
use super::rpc::AuditService;
use super::store::{self, Filter};
use super::{Actor, Audit, AuditConfig, NewEvent, Outcome, Scope, catalogue, chain::GENESIS, meta};
use crate::migrate;
use crate::registry::Registry;

async fn pool() -> AnyPool {
    let p = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&p).await.unwrap();
    p
}

fn alice() -> Actor {
    Actor::new("user", "alice")
}

fn push(repo: &str, n: u32) -> NewEvent {
    NewEvent::new("repo.ref.update", alice())
        .repo(repo)
        .target(&format!("refs/heads/b{n}"))
        .detail("ref", format!("refs/heads/b{n}"))
        .detail("via", "asc-push")
}

fn all() -> Filter {
    Filter::default()
}

async fn list_all(pool: &AnyPool, scope: &Scope) -> Vec<super::Event> {
    store::list(pool, scope, &all(), true, 200, None)
        .await
        .unwrap()
        .events
}

#[tokio::test]
async fn an_event_goes_to_the_repository_the_owner_and_the_instance() {
    let p = pool().await;
    store::record(&p, &push("acme/app", 1)).await.unwrap();
    store::record(&p, &push("acme/other", 2)).await.unwrap();
    store::record(&p, &push("zed/app", 3)).await.unwrap();
    let seqs = |s: Scope| {
        let p = p.clone();
        async move {
            list_all(&p, &s)
                .await
                .iter()
                .map(|e| e.seq)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(seqs(Scope::repo("acme/app")).await, vec![1]);
    assert_eq!(seqs(Scope::repo("acme/other")).await, vec![1]);
    assert_eq!(seqs(Scope::owner("acme")).await, vec![1, 2]);
    assert_eq!(seqs(Scope::owner("zed")).await, vec![1]);
    assert_eq!(seqs(Scope::instance()).await, vec![1, 2, 3]);
    // The same event has the same id everywhere.
    let a = list_all(&p, &Scope::repo("acme/app")).await;
    let b = list_all(&p, &Scope::owner("acme")).await;
    let c = list_all(&p, &Scope::instance()).await;
    assert_eq!(a[0].id, b[0].id);
    assert_eq!(a[0].id, c[0].id);
    for s in [
        Scope::repo("acme/app"),
        Scope::owner("acme"),
        Scope::instance(),
    ] {
        let v = store::verify_chain(&p, &s, 0, 0).await.unwrap();
        assert!(v.ok, "{s}: {}", v.reason);
    }
}

#[tokio::test]
async fn an_event_without_a_repository_goes_to_its_owner_and_the_instance() {
    let p = pool().await;
    let e = NewEvent::new("auth.token.created", alice())
        .owner("alice")
        .detail("token", "t1")
        .detail("kind", "personal");
    store::record(&p, &e).await.unwrap();
    assert_eq!(list_all(&p, &Scope::owner("alice")).await.len(), 1);
    assert_eq!(list_all(&p, &Scope::instance()).await.len(), 1);
    let bare = NewEvent::new("vault.rotated", Actor::system("vault")).detail("generation", "2");
    store::record(&p, &bare).await.unwrap();
    assert_eq!(list_all(&p, &Scope::instance()).await.len(), 2);
    assert_eq!(list_all(&p, &Scope::owner("alice")).await.len(), 1);
}

#[tokio::test]
async fn a_rolled_back_action_leaves_no_event_and_no_gap() {
    let p = pool().await;
    store::record(&p, &push("a/b", 1)).await.unwrap();
    {
        let mut tx = store::begin(&p).await.unwrap();
        store::append_in_tx(&mut tx, &push("a/b", 2)).await.unwrap();
        // Dropped without a commit.
    }
    store::record(&p, &push("a/b", 3)).await.unwrap();
    let seqs: Vec<u64> = list_all(&p, &Scope::repo("a/b"))
        .await
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(seqs, vec![1, 2]);
    assert!(
        store::verify_chain(&p, &Scope::repo("a/b"), 0, 0)
            .await
            .unwrap()
            .ok
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_are_gap_free_and_chained() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite:{}?mode=rwc",
        dir.path()
            .join("audit.db")
            .display()
            .to_string()
            .replace('\\', "/")
    );
    let p = migrate::connect(&url, 6).await.unwrap();
    migrate::migrate(&p).await.unwrap();
    let mut tasks = Vec::new();
    for n in 0..24u32 {
        let p = p.clone();
        tasks.push(tokio::spawn(async move {
            store::record(&p, &push("acme/app", n)).await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    for s in [
        Scope::repo("acme/app"),
        Scope::owner("acme"),
        Scope::instance(),
    ] {
        let seqs: Vec<u64> = list_all(&p, &s).await.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (1..=24).collect::<Vec<_>>(), "{s}");
        let v = store::verify_chain(&p, &s, 0, 0).await.unwrap();
        assert!(v.ok, "{s}: {}", v.reason);
        assert_eq!(v.checked, 24);
    }
}

#[tokio::test]
async fn bad_events_are_refused() {
    let p = pool().await;
    let unknown = NewEvent::new("nope.nothing", alice());
    assert!(matches!(
        store::record(&p, &unknown).await,
        Err(super::AuditError::UnknownAction(_))
    ));
    let bad_repo = NewEvent::new("repo.ref.update", alice()).repo("not a repo");
    assert!(store::record(&p, &bad_repo).await.is_err());
    let bad_actor = NewEvent::new("repo.ref.update", Actor::new("alien", "x")).repo("a/b");
    assert!(store::record(&p, &bad_actor).await.is_err());
    let empty_actor = NewEvent::new("repo.ref.update", Actor::new("user", "")).repo("a/b");
    assert!(store::record(&p, &empty_actor).await.is_err());
    let bad_owner = NewEvent::new("auth.token.created", alice()).owner("a/b");
    assert!(store::record(&p, &bad_owner).await.is_err());
    // Nothing was written, not even a head.
    assert!(list_all(&p, &Scope::instance()).await.is_empty());
}

#[tokio::test]
async fn details_keep_only_allowed_keys_and_never_a_credential() {
    let p = pool().await;
    let secret = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
    let e = NewEvent::new("storage.target.created", alice())
        .owner("alice")
        .detail("target", "t1")
        .detail(
            "endpoint",
            "https://AKIDEXAMPLE:hunter2secret@s3.example.com/b?X-Amz-Signature=abcdef",
        )
        .detail("bucket", secret)
        .detail("secret_key", "hunter2secret")
        .detail("kind", "s3\nx");
    store::record(&p, &e).await.unwrap();
    let got = &list_all(&p, &Scope::owner("alice")).await[0];
    assert_eq!(got.details["endpoint"], "https://s3.example.com/b");
    assert_eq!(got.details["target"], "t1");
    assert_eq!(got.details["kind"], "s3 x");
    assert!(
        !got.details.contains_key("bucket"),
        "a credential-like value is dropped"
    );
    assert!(
        !got.details.contains_key("secret_key"),
        "an unlisted key is dropped"
    );
    // Neither the planted secrets nor the signature appear anywhere in the table.
    let rows = sqlx::query("SELECT * FROM audit_events")
        .fetch_all(&p)
        .await
        .unwrap();
    for row in rows {
        for col in [
            "actor_name",
            "target",
            "details_json",
            "user_agent",
            "request_id",
            "repo",
        ] {
            let text: String = row.try_get(col).unwrap();
            for needle in [secret, "hunter2secret", "AKIDEXAMPLE", "X-Amz-Signature"] {
                assert!(!text.contains(needle), "{col} holds {needle}");
            }
        }
    }
}

#[tokio::test]
async fn request_facts_are_recorded() {
    let p = pool().await;
    let e = push("a/b", 1).with_meta(meta::RequestMeta {
        remote_addr: "203.0.113.0/24".into(),
        user_agent: "asc/1.2 (linux)\u{7}".into(),
        request_id: "req-9".into(),
    });
    store::record(&p, &e).await.unwrap();
    let got = &list_all(&p, &Scope::repo("a/b")).await[0];
    assert_eq!(got.remote_addr, "203.0.113.0/24");
    assert_eq!(got.user_agent, "asc/1.2 (linux)");
    assert_eq!(got.request_id, "req-9");
    assert_eq!(got.actor, alice());
    assert_eq!(got.outcome, Outcome::Success);
    // The request in progress is the default.
    let seen = meta::scope("curl/8", "r-1", async {
        store::record(&p, &push("a/b", 2)).await.unwrap();
    });
    seen.await;
    let got = &list_all(&p, &Scope::repo("a/b")).await[1];
    assert_eq!(
        (got.user_agent.as_str(), got.request_id.as_str()),
        ("curl/8", "r-1")
    );
}

async fn seeded() -> AnyPool {
    let p = pool().await;
    let actors = [("user", "alice"), ("agent", "bot-1"), ("user", "bob")];
    for n in 0..12u32 {
        let (k, name) = actors[(n % 3) as usize];
        let mut e = NewEvent::new(
            if n % 4 == 0 {
                "push.refused"
            } else {
                "repo.ref.update"
            },
            Actor::new(k, name),
        )
        .repo("acme/app")
        .target(&format!(
            "refs/heads/{}/{n}",
            if n % 2 == 0 { "main" } else { "feat" }
        ))
        .at(1_000 + i64::from(n) * 10);
        if n % 4 == 0 {
            e = e.outcome(Outcome::Denied);
        }
        store::record(&p, &e).await.unwrap();
    }
    p
}

#[tokio::test]
async fn filters_narrow_the_list() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    let count = |f: Filter| {
        let (p, s) = (p.clone(), s.clone());
        async move {
            store::list(&p, &s, &f, true, 200, None)
                .await
                .unwrap()
                .events
        }
    };
    assert_eq!(count(all()).await.len(), 12);
    let by_name = count(Filter {
        actors: vec!["alice".into()],
        ..all()
    })
    .await;
    assert_eq!(by_name.len(), 4);
    let by_label = count(Filter {
        actors: vec!["agent:bot-1".into()],
        ..all()
    })
    .await;
    assert_eq!(by_label.len(), 4);
    assert!(by_label.iter().all(|e| e.actor.kind == "agent"));
    assert_eq!(
        count(Filter {
            actors: vec!["user:bot-1".into()],
            ..all()
        })
        .await
        .len(),
        0
    );
    let two = count(Filter {
        actors: vec!["alice".into(), "bob".into()],
        ..all()
    })
    .await;
    assert_eq!(two.len(), 8);
    assert_eq!(
        count(Filter {
            actions: vec!["push.refused".into()],
            ..all()
        })
        .await
        .len(),
        3
    );
    assert_eq!(
        count(Filter {
            actions: vec!["push.*".into()],
            ..all()
        })
        .await
        .len(),
        3
    );
    assert_eq!(
        count(Filter {
            actions: vec!["repo.*".into()],
            ..all()
        })
        .await
        .len(),
        9
    );
    assert_eq!(
        count(Filter {
            actions: vec!["repo".into()],
            ..all()
        })
        .await
        .len(),
        0
    );
    assert_eq!(
        count(Filter {
            actions: vec!["push.*".into(), "repo.ref.update".into()],
            ..all()
        })
        .await
        .len(),
        12
    );
    assert_eq!(
        count(Filter {
            outcome: Some(Outcome::Denied),
            ..all()
        })
        .await
        .len(),
        3
    );
    assert_eq!(
        count(Filter {
            outcome: Some(Outcome::Failed),
            ..all()
        })
        .await
        .len(),
        0
    );
    assert_eq!(
        count(Filter {
            target: "refs/heads/main/".into(),
            ..all()
        })
        .await
        .len(),
        6
    );
    assert_eq!(
        count(Filter {
            target: "REFS/heads/main/".into(),
            ..all()
        })
        .await
        .len(),
        0
    );
    let window = count(Filter {
        since_ms: 1_030,
        until_ms: 1_060,
        ..all()
    })
    .await;
    assert_eq!(
        window.iter().map(|e| e.time_ms).collect::<Vec<_>>(),
        vec![1_030, 1_040, 1_050]
    );
    let combined = count(Filter {
        actors: vec!["alice".into()],
        actions: vec!["push.*".into()],
        outcome: Some(Outcome::Denied),
        ..all()
    })
    .await;
    assert_eq!(combined.len(), 1, "only n = 0 is alice and refused");
}

#[tokio::test]
async fn pages_walk_both_directions_without_overlap() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    for oldest in [false, true] {
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let page = store::list(&p, &s, &all(), oldest, 5, after).await.unwrap();
            assert!(page.events.len() <= 5);
            seen.extend(page.events.iter().map(|e| e.seq));
            match page.next {
                Some(n) => after = Some(n),
                None => break,
            }
        }
        let mut want: Vec<u64> = (1..=12).collect();
        if !oldest {
            want.reverse();
        }
        assert_eq!(seen, want, "oldest_first = {oldest}");
    }
    // A limit of 0 is the default, and the limit is capped.
    assert_eq!(
        store::list(&p, &s, &all(), true, 0, None)
            .await
            .unwrap()
            .events
            .len(),
        12
    );
    assert_eq!(
        store::list(&p, &s, &all(), true, 100_000, None)
            .await
            .unwrap()
            .events
            .len(),
        12
    );
}

#[tokio::test]
async fn editing_an_event_is_found_at_that_event() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    sqlx::query(
        "UPDATE audit_events SET actor_name = 'mallory' WHERE scope_kind = 'repo' AND seq = 7",
    )
    .execute(&p)
    .await
    .unwrap();
    let v = store::verify_chain(&p, &s, 0, 0).await.unwrap();
    assert!(!v.ok);
    assert_eq!(v.first_bad_seq, 7);
    assert_eq!(v.checked, 6);
    // The other scopes hold their own copy and are intact.
    assert!(
        store::verify_chain(&p, &Scope::instance(), 0, 0)
            .await
            .unwrap()
            .ok
    );
    // A range before the edit verifies; one that covers it does not.
    assert!(store::verify_chain(&p, &s, 1, 6).await.unwrap().ok);
    assert_eq!(
        store::verify_chain(&p, &s, 5, 9)
            .await
            .unwrap()
            .first_bad_seq,
        7
    );
}

#[tokio::test]
async fn editing_details_or_hash_is_found() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    sqlx::query("UPDATE audit_events SET details_json = '{\"x\":\"y\"}' WHERE scope_kind = 'repo' AND seq = 3")
        .execute(&p)
        .await
        .unwrap();
    assert_eq!(
        store::verify_chain(&p, &s, 0, 0)
            .await
            .unwrap()
            .first_bad_seq,
        3
    );
    let p = seeded().await;
    sqlx::query("UPDATE audit_events SET hash = prev_hash WHERE scope_kind = 'repo' AND seq = 2")
        .execute(&p)
        .await
        .unwrap();
    let v = store::verify_chain(&p, &s, 0, 0).await.unwrap();
    assert_eq!(v.first_bad_seq, 2);
}

#[tokio::test]
async fn deleting_an_event_is_found_at_the_gap() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    sqlx::query("DELETE FROM audit_events WHERE scope_kind = 'repo' AND seq = 5")
        .execute(&p)
        .await
        .unwrap();
    let v = store::verify_chain(&p, &s, 0, 0).await.unwrap();
    assert!(!v.ok);
    assert_eq!(v.first_bad_seq, 5);
    assert!(v.reason.contains("missing"), "{}", v.reason);
}

#[tokio::test]
async fn deleting_the_newest_events_is_found_through_the_head() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    sqlx::query("DELETE FROM audit_events WHERE scope_kind = 'repo' AND seq >= 11")
        .execute(&p)
        .await
        .unwrap();
    let v = store::verify_chain(&p, &s, 0, 0).await.unwrap();
    assert!(!v.ok);
    assert_eq!(v.first_bad_seq, 11);
    // Deleting everything is found too.
    sqlx::query("DELETE FROM audit_events WHERE scope_kind = 'repo'")
        .execute(&p)
        .await
        .unwrap();
    assert_eq!(
        store::verify_chain(&p, &s, 0, 0)
            .await
            .unwrap()
            .first_bad_seq,
        1
    );
}

#[tokio::test]
async fn an_event_inserted_past_the_head_is_found() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    sqlx::query("UPDATE audit_heads SET seq = 11 WHERE scope_kind = 'repo'")
        .execute(&p)
        .await
        .unwrap();
    let v = store::verify_chain(&p, &s, 0, 0).await.unwrap();
    assert!(!v.ok);
}

#[tokio::test]
async fn an_untouched_chain_reports_its_head() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    let v = store::verify_chain(&p, &s, 0, 0).await.unwrap();
    assert!(v.ok);
    assert_eq!(v.checked, 12);
    assert_eq!(v.head_hash, store::head(&p, &s).await.unwrap().unwrap().1);
    // A scope that was never written verifies as empty.
    let none = store::verify_chain(&p, &Scope::repo("no/such"), 0, 0)
        .await
        .unwrap();
    assert!(none.ok && none.checked == 0);
    assert_eq!(none.head_hash, GENESIS);
}

#[tokio::test]
async fn retention_drops_old_events_and_the_chain_still_verifies() {
    let p = seeded().await; // events at 1_000, 1_010 .. 1_110
    let s = Scope::repo("acme/app");
    let cfg = AuditConfig {
        retention_days: 1,
        ..AuditConfig::default()
    };
    // `now` is one day and 1_055 ms after the epoch: events 0..=5 are older than a day.
    let now = 86_400_000 + 1_055;
    let dropped = retention::run_once(&p, &cfg, now).await.unwrap();
    assert!(
        dropped
            .iter()
            .any(|d| d.scope == s && d.events == 6 && d.until_seq == 6)
    );
    let left: Vec<u64> = list_all(&p, &s).await.iter().map(|e| e.seq).collect();
    assert_eq!(left, (7..=12).collect::<Vec<_>>());
    let v = store::verify_chain(&p, &s, 0, 0).await.unwrap();
    assert!(v.ok, "{}", v.reason);
    assert_eq!(v.checked, 6);
    for scope in [Scope::owner("acme"), Scope::instance()] {
        assert!(store::verify_chain(&p, &scope, 0, 0).await.unwrap().ok);
    }
    // The drop is itself in the instance log, and a new event continues the chain.
    let pruned = store::list(
        &p,
        &Scope::instance(),
        &Filter {
            actions: vec!["audit.retention.pruned".into()],
            ..all()
        },
        true,
        50,
        None,
    )
    .await
    .unwrap();
    assert!(!pruned.events.is_empty());
    assert_eq!(pruned.events[0].actor.kind, "system");
    store::record(&p, &push("acme/app", 99)).await.unwrap();
    assert_eq!(list_all(&p, &s).await.last().unwrap().seq, 13);
    assert!(store::verify_chain(&p, &s, 0, 0).await.unwrap().ok);
    // Running again drops nothing more from this scope.
    let again = retention::run_once(&p, &cfg, now).await.unwrap();
    assert!(again.iter().all(|d| d.scope != s));
}

#[tokio::test]
async fn retention_can_drop_a_whole_scope_and_keeps_the_head() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    let cfg = AuditConfig {
        retention_days: 1,
        ..AuditConfig::default()
    };
    retention::run_once(&p, &cfg, 10 * 86_400_000)
        .await
        .unwrap();
    assert!(list_all(&p, &s).await.is_empty());
    let v = store::verify_chain(&p, &s, 0, 0).await.unwrap();
    assert!(v.ok, "{}", v.reason);
    store::record(&p, &push("acme/app", 1)).await.unwrap();
    let e = list_all(&p, &s).await;
    assert_eq!(e[0].seq, 13);
    assert!(store::verify_chain(&p, &s, 0, 0).await.unwrap().ok);
}

#[tokio::test]
async fn zero_retention_keeps_everything() {
    let p = seeded().await;
    let cfg = AuditConfig {
        retention_days: 0,
        ..AuditConfig::default()
    };
    assert!(
        retention::run_once(&p, &cfg, i64::MAX / 2)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(list_all(&p, &Scope::repo("acme/app")).await.len(), 12);
}

#[tokio::test]
async fn retention_leaves_a_broken_chain_alone() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    sqlx::query("DELETE FROM audit_events WHERE scope_kind = 'repo' AND seq = 1")
        .execute(&p)
        .await
        .unwrap();
    let cfg = AuditConfig {
        retention_days: 1,
        ..AuditConfig::default()
    };
    retention::run_once(&p, &cfg, 10 * 86_400_000)
        .await
        .unwrap();
    // Event 1 was removed by hand; retention refuses to hide that.
    assert_eq!(list_all(&p, &s).await.len(), 11);
    assert!(!store::verify_chain(&p, &s, 0, 0).await.unwrap().ok);
}

#[tokio::test]
async fn export_matches_its_digest_and_is_one_row_per_event() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    let chunks = Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let sink = chunks.clone();
    let summary = export::run(&p, &s, &all(), Format::Jsonl, 1_000, move |c| {
        let sink = sink.clone();
        async move {
            sink.lock().unwrap().push(c);
            true
        }
    })
    .await
    .unwrap()
    .unwrap();
    let bytes: Vec<u8> = chunks.lock().unwrap().concat();
    assert_eq!(summary.events, 12);
    assert!(!summary.truncated);
    assert_eq!(summary.digest, *blake3::hash(&bytes).as_bytes());
    let lines: Vec<&str> = std::str::from_utf8(&bytes).unwrap().lines().collect();
    assert_eq!(lines.len(), 12);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["seq"], 1);
    assert_eq!(first["scope"], "repo:acme/app");
    let last: serde_json::Value = serde_json::from_str(lines[11]).unwrap();
    assert_eq!(last["seq"], 12);
}

#[tokio::test]
async fn export_honours_filters_the_bound_and_csv() {
    let p = seeded().await;
    let s = Scope::repo("acme/app");
    let run = |f: Filter, fmt: Format, max: u64| {
        let p = p.clone();
        let s = s.clone();
        async move {
            let out = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
            let sink = out.clone();
            let summary = export::run(&p, &s, &f, fmt, max, move |c| {
                let sink = sink.clone();
                async move {
                    sink.lock().unwrap().extend_from_slice(&c);
                    true
                }
            })
            .await
            .unwrap()
            .unwrap();
            let bytes = out.lock().unwrap().clone();
            (summary, String::from_utf8(bytes).unwrap())
        }
    };
    let (sum, text) = run(
        Filter {
            actions: vec!["push.*".into()],
            ..all()
        },
        Format::Jsonl,
        100,
    )
    .await;
    assert_eq!((sum.events, text.lines().count()), (3, 3));
    let (sum, text) = run(all(), Format::Jsonl, 5).await;
    assert_eq!(
        (sum.events, sum.truncated, text.lines().count()),
        (5, true, 5)
    );
    let (sum, text) = run(all(), Format::Jsonl, 12).await;
    assert_eq!(
        (sum.events, sum.truncated, text.lines().count()),
        (12, false, 12)
    );
    let (sum, text) = run(all(), Format::Csv, 100).await;
    assert_eq!(sum.events, 12);
    assert_eq!(text.lines().count(), 13, "a header and a row per event");
    assert!(text.starts_with("scope,seq,id,"));
    // Nothing matches: JSON lines are empty, CSV has the header.
    let (sum, text) = run(
        Filter {
            actors: vec!["nobody".into()],
            ..all()
        },
        Format::Jsonl,
        10,
    )
    .await;
    assert_eq!((sum.events, text.len()), (0, 0));
}

#[tokio::test]
async fn a_reader_that_goes_away_stops_the_export() {
    let p = seeded().await;
    let out = export::run(
        &p,
        &Scope::repo("acme/app"),
        &all(),
        Format::Jsonl,
        100,
        |_| async { false },
    )
    .await
    .unwrap();
    // Everything fits one chunk, which is only sent at the end; the sink refuses it.
    assert!(out.is_none());
}

// ------------------------------------------------------------------------------ read access

const ALICE_ADMIN: &str = "tok-alice-admin";
const BOB_WRITE: &str = "tok-bob-write";
const CAROL_SITE: &str = "tok-carol-site";
const HOST: &str = "tok-host";

async fn service() -> (AuditService, AnyPool) {
    let p = pool().await;
    let registry = Arc::new(Registry::new(p.clone(), "memory:///").unwrap());
    for name in ["acme/app", "acme/lib", "zed/app"] {
        registry
            .create(
                &asc_rpc::server::RepoName::parse(name).unwrap(),
                &crate::registry::NewRepo::default(),
            )
            .await
            .unwrap();
    }
    let cfg = AuditConfig {
        admins: vec!["carol".into()],
        ..AuditConfig::default()
    };
    let audit = Arc::new(Audit::new(registry, cfg, Arc::default()));
    // Alice administers acme/app and acme/lib only; bob writes; carol is a site admin; the host
    // service credential is trusted to have checked its own permissions.
    let auth = Arc::new(
        StaticTokens::new()
            .with_token(ALICE_ADMIN, "alice", Access::Admin)
            .with_token(BOB_WRITE, "bob", Access::Write)
            .with_token(CAROL_SITE, "carol", Access::Read)
            .with_token(HOST, "host-service", Access::Admin),
    );
    (AuditService::new(audit, auth, ServerInfo::default()), p)
}

fn req<T>(msg: T, token: Option<&str>) -> Request<T> {
    let mut r = Request::new(msg);
    if let Some(t) = token {
        let header = format!("Bearer {t}");
        let cred = parse_authorization([header.as_bytes()]).unwrap().unwrap();
        r.extensions_mut().insert(cred);
    }
    r
}

fn scope(kind: wire::OwnerKind, name: &str) -> Option<wire::OwnerRef> {
    Some(wire::OwnerRef {
        kind: kind as i32,
        name: name.into(),
    })
}

async fn try_list(
    svc: &AuditService,
    token: Option<&str>,
    s: Option<wire::OwnerRef>,
) -> Result<usize, tonic::Code> {
    svc.list(req(
        wire::ListAuditRequest {
            scope: s,
            ..Default::default()
        },
        token,
    ))
    .await
    .map(|r| r.into_inner().events.len())
    .map_err(|e| e.code())
}

#[tokio::test]
async fn who_may_read_which_scope() {
    use tonic::Code::{InvalidArgument, NotFound, Unauthenticated};
    use wire::OwnerKind::{Instance, Org, Repo, User};
    let (svc, p) = service().await;
    store::record(&p, &push("acme/app", 1)).await.unwrap();
    store::record(&p, &push("zed/app", 2)).await.unwrap();

    // Repository: admin of that repository, site admins and the host; nobody else.
    for (token, repo, want) in [
        (Some(ALICE_ADMIN), "acme/app", Ok(1)),
        (Some(CAROL_SITE), "zed/app", Ok(1)),
        (Some(HOST), "zed/app", Ok(1)),
    ] {
        assert_eq!(
            try_list(&svc, token, scope(Repo, repo)).await,
            want,
            "{repo}"
        );
    }
    // StaticTokens grants alice Admin on every repository; the NOT_FOUND cases are the ones whose
    // credential lacks the access: bob has Write only.
    assert_eq!(
        try_list(&svc, Some(BOB_WRITE), scope(Repo, "acme/app")).await,
        Err(NotFound)
    );
    assert_eq!(
        try_list(&svc, None, scope(Repo, "acme/app")).await,
        Err(Unauthenticated)
    );
    assert_eq!(
        try_list(&svc, Some("wrong-token"), scope(Repo, "acme/app")).await,
        Err(Unauthenticated)
    );

    // A user scope: the person, a site admin; not another user.
    store::record(
        &p,
        &NewEvent::new("auth.token.created", Actor::new("user", "alice"))
            .owner("alice")
            .detail("token", "t"),
    )
    .await
    .unwrap();
    assert_eq!(
        try_list(&svc, Some(ALICE_ADMIN), scope(User, "alice")).await,
        Ok(1)
    );
    assert_eq!(
        try_list(&svc, Some(BOB_WRITE), scope(User, "alice")).await,
        Err(NotFound)
    );
    assert_eq!(
        try_list(&svc, Some(CAROL_SITE), scope(User, "alice")).await,
        Ok(1)
    );

    // An organisation: admin of every repository of it.
    assert_eq!(
        try_list(&svc, Some(ALICE_ADMIN), scope(Org, "acme")).await,
        Ok(1)
    );
    assert_eq!(
        try_list(&svc, Some(BOB_WRITE), scope(Org, "acme")).await,
        Err(NotFound)
    );
    assert_eq!(
        try_list(&svc, Some(ALICE_ADMIN), scope(Org, "ghost-org")).await,
        Err(NotFound)
    );

    // The instance: site administrators only.
    assert_eq!(
        try_list(&svc, Some(CAROL_SITE), scope(Instance, "")).await,
        Ok(3)
    );
    assert_eq!(try_list(&svc, Some(HOST), scope(Instance, "")).await, Ok(3));
    assert_eq!(
        try_list(&svc, Some(ALICE_ADMIN), scope(Instance, "")).await,
        Err(NotFound)
    );
    assert_eq!(
        try_list(&svc, Some(BOB_WRITE), scope(Instance, "")).await,
        Err(NotFound)
    );

    // Malformed scopes.
    assert_eq!(
        try_list(&svc, Some(CAROL_SITE), None).await,
        Err(InvalidArgument)
    );
    assert_eq!(
        try_list(
            &svc,
            Some(CAROL_SITE),
            scope(wire::OwnerKind::Unspecified, "x")
        )
        .await,
        Err(InvalidArgument)
    );
    assert_eq!(
        try_list(&svc, Some(CAROL_SITE), scope(Instance, "name")).await,
        Err(InvalidArgument)
    );
    assert_eq!(
        try_list(&svc, Some(CAROL_SITE), scope(Repo, "not a repo")).await,
        Err(InvalidArgument)
    );
    assert_eq!(
        try_list(&svc, Some(CAROL_SITE), scope(User, "bad name!")).await,
        Err(InvalidArgument)
    );
}

#[tokio::test]
async fn export_verify_and_actions_check_the_same_access() {
    use tonic::Code::{NotFound, Unauthenticated};
    let (svc, p) = service().await;
    store::record(&p, &push("acme/app", 1)).await.unwrap();
    let repo = scope(wire::OwnerKind::Repo, "acme/app");

    let verify = svc
        .verify_chain(req(
            wire::VerifyAuditChainRequest {
                scope: repo.clone(),
                ..Default::default()
            },
            Some(ALICE_ADMIN),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(verify.ok);
    assert_eq!(verify.checked, 1);
    assert_eq!(
        svc.verify_chain(req(
            wire::VerifyAuditChainRequest {
                scope: repo.clone(),
                ..Default::default()
            },
            Some(BOB_WRITE)
        ))
        .await
        .unwrap_err()
        .code(),
        NotFound
    );
    assert_eq!(
        svc.verify_chain(req(
            wire::VerifyAuditChainRequest {
                scope: repo.clone(),
                from_seq: 5,
                to_seq: 2
            },
            Some(ALICE_ADMIN)
        ))
        .await
        .unwrap_err()
        .code(),
        tonic::Code::InvalidArgument
    );

    let denied = svc
        .export(req(
            wire::ExportAuditRequest {
                scope: repo.clone(),
                ..Default::default()
            },
            Some(BOB_WRITE),
        ))
        .await;
    assert_eq!(denied.err().map(|e| e.code()), Some(NotFound));
    let anon = svc
        .export(req(
            wire::ExportAuditRequest {
                scope: repo.clone(),
                ..Default::default()
            },
            None,
        ))
        .await;
    assert_eq!(anon.err().map(|e| e.code()), Some(Unauthenticated));

    // An allowed export streams the events, ends with the digest, and is itself in the log.
    use futures::StreamExt;
    let mut stream = svc
        .export(req(
            wire::ExportAuditRequest {
                scope: repo.clone(),
                ..Default::default()
            },
            Some(ALICE_ADMIN),
        ))
        .await
        .unwrap()
        .into_inner();
    let mut data = Vec::new();
    let mut last = None;
    while let Some(m) = stream.next().await {
        let m = m.unwrap();
        data.extend_from_slice(&m.data);
        if m.last {
            last = Some(m);
        }
    }
    let last = last.expect("a last message");
    assert_eq!(last.digest.as_ref(), blake3::hash(&data).as_bytes());
    assert_eq!(last.events, 2, "the push and the export event itself");
    assert!(!last.truncated);
    let text = String::from_utf8(data).unwrap();
    assert!(text.contains("\"audit.exported\""));
    assert!(text.contains("\"repo.ref.update\""));

    let actions = svc
        .list_actions(req(wire::ListAuditActionsRequest {}, Some(BOB_WRITE)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(actions.actions.len(), catalogue::CATALOGUE.len());
    assert!(
        actions
            .actions
            .iter()
            .any(|a| a.action == "policy.proposed")
    );
    assert_eq!(
        svc.list_actions(req(wire::ListAuditActionsRequest {}, None))
            .await
            .unwrap_err()
            .code(),
        Unauthenticated
    );
}

#[tokio::test]
async fn list_filters_and_tokens_arrive_through_the_service() {
    let (svc, p) = service().await;
    for n in 0..5 {
        store::record(&p, &push("acme/app", n)).await.unwrap();
    }
    let repo = scope(wire::OwnerKind::Repo, "acme/app");
    let first = svc
        .list(req(
            wire::ListAuditRequest {
                scope: repo.clone(),
                limit: 2,
                filter: Some(wire::AuditFilter {
                    actions: vec!["repo.*".into()],
                    ..Default::default()
                }),
                ..Default::default()
            },
            Some(ALICE_ADMIN),
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.events.len(), 2);
    assert_eq!(first.events[0].seq, 5);
    assert!(!first.next_page_token.is_empty());
    let e = &first.events[0];
    assert_eq!(e.actor.as_ref().unwrap().name, "alice");
    assert_eq!(e.action, "repo.ref.update");
    assert_eq!(e.hash.len(), 32);
    let rest = svc
        .list(req(
            wire::ListAuditRequest {
                scope: repo.clone(),
                page_token: first.next_page_token,
                limit: 10,
                ..Default::default()
            },
            Some(ALICE_ADMIN),
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        rest.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
    assert!(rest.next_page_token.is_empty());
    let bad = svc
        .list(req(
            wire::ListAuditRequest {
                scope: repo,
                page_token: "../etc".into(),
                ..Default::default()
            },
            Some(ALICE_ADMIN),
        ))
        .await
        .unwrap_err();
    assert_eq!(bad.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn the_hubs_claim_decides_the_scope_for_the_person_and_nothing_wider() {
    use asc_rpc::forwarded::{ForwardedVerifier, Role};
    use tonic::Code::NotFound;
    use wire::OwnerKind::{Instance, Org, Repo, User};
    let (_plain, p) = service().await;
    let registry = Arc::new(Registry::new(p.clone(), "memory:///").unwrap());
    let audit = Arc::new(Audit::new(registry, AuditConfig::default(), Arc::default()));
    let auth: Arc<dyn asc_rpc::auth::Authorizer> =
        Arc::new(crate::authz::forwarded::ForwardedAuthorizer::new(
            Arc::new(StaticTokens::new().with_token(HOST, "host-service", Access::Admin)),
            ForwardedVerifier::new(vec![6u8; 32]).unwrap(),
        ));
    let svc = AuditService::new(audit, auth, ServerInfo::default());
    store::record(&p, &push("acme/app", 1)).await.unwrap();
    let v = ForwardedVerifier::new(vec![6u8; 32]).unwrap();
    let now = crate::ai::clock::now();
    let list = |s: Option<wire::OwnerRef>, subject: &str, user: &str, role: Role, at: i64| {
        let mut r = req(
            wire::ListAuditRequest {
                scope: s,
                ..Default::default()
            },
            Some(HOST),
        );
        for (k, val) in v.sign(user, role, subject, at) {
            r.metadata_mut().append(k, val.parse().unwrap());
        }
        svc.list(r)
    };
    let count = |r: Result<tonic::Response<wire::ListAuditResponse>, tonic::Status>| {
        r.map(|x| x.into_inner().events.len()).map_err(|e| e.code())
    };
    // The repository's administrator reads its log; a writer does not.
    assert_eq!(
        count(
            list(
                scope(Repo, "acme/app"),
                "acme/app",
                "7:alice",
                Role::Admin,
                now
            )
            .await
        ),
        Ok(1)
    );
    assert_eq!(
        count(
            list(
                scope(Repo, "acme/app"),
                "acme/app",
                "8:bob",
                Role::Write,
                now
            )
            .await
        ),
        Err(NotFound)
    );
    // A claim for one repository is no claim for another, nor for the instance.
    assert_eq!(
        count(
            list(
                scope(Repo, "zed/app"),
                "acme/app",
                "7:alice",
                Role::Admin,
                now
            )
            .await
        ),
        Err(NotFound)
    );
    assert_eq!(
        count(list(scope(Instance, ""), "acme/app", "7:alice", Role::Admin, now).await),
        Err(NotFound)
    );
    // The organisation's owner reads the organisation log whatever its size; the site
    // administrator reads the instance log.
    assert_eq!(
        count(list(scope(Org, "acme"), "org:acme", "7:alice", Role::Admin, now).await),
        Ok(1)
    );
    assert_eq!(
        count(list(scope(Org, "acme"), "org:acme", "7:alice", Role::Read, now).await),
        Err(NotFound)
    );
    assert!(count(list(scope(Instance, ""), "instance", "1:root", Role::Admin, now).await).is_ok());
    // A person's own scope, by the name in the claim.
    assert!(
        count(
            list(
                scope(User, "alice"),
                "user:alice",
                "7:alice",
                Role::Write,
                now
            )
            .await
        )
        .is_ok()
    );
    assert_eq!(
        count(
            list(
                scope(User, "alice"),
                "user:alice",
                "8:bob",
                Role::Write,
                now
            )
            .await
        ),
        Err(NotFound)
    );
    // A stale claim is refused, not treated as the service's own breadth.
    assert_eq!(
        count(
            list(
                scope(Instance, ""),
                "instance",
                "1:root",
                Role::Admin,
                now - 600
            )
            .await
        ),
        Err(NotFound)
    );
    // The service credential alone (system work) still reads.
    assert!(
        try_list(&svc, Some(HOST), scope(Instance, ""))
            .await
            .is_ok()
    );
}
