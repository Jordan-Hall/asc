//! Notification events: cleaning to the hub's limits, the queue, the retry and the hub call.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use sqlx::{AnyPool, Row};

use super::worker::{DispatchConfig, Dispatcher, Failure, GIVE_UP_SECS, KEEP_SECS, Outgoing, Sink};
use super::*;
use crate::migrate;
use crate::outbox::backoff_secs;

async fn pool_with(repos: &[(&str, u8)]) -> AnyPool {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    for (name, ns) in repos {
        sqlx::query("INSERT INTO repos (name, ns, created) VALUES ($1, $2, 1)")
            .bind(*name)
            .bind(vec![*ns; 4])
            .execute(&pool)
            .await
            .unwrap();
    }
    pool
}

/// A hub that records what it is sent and answers from a script (then `Ok(told)`).
#[derive(Default)]
struct Recorder {
    sent: Mutex<Vec<(String, Outgoing)>>,
    script: Mutex<VecDeque<Result<u64, Failure>>>,
}

impl Recorder {
    fn new(script: Vec<Result<u64, Failure>>) -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::default(),
            script: Mutex::new(script.into()),
        })
    }

    fn sent(&self) -> Vec<(String, Outgoing)> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Sink for Recorder {
    async fn deliver(&self, id: &str, event: &Outgoing) -> Result<u64, Failure> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id.to_owned(), event.clone()));
        self.script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| Ok(event.users.len() as u64))
    }
}

fn retry(m: &str) -> Failure {
    Failure {
        permanent: false,
        message: m.into(),
    }
}

fn dispatcher(pool: &AnyPool, sink: &Arc<Recorder>) -> Dispatcher {
    Dispatcher::new(
        pool.clone(),
        Some(sink.clone() as Arc<dyn Sink>),
        DispatchConfig::default(),
    )
}

async fn row(pool: &AnyPool, id: i64) -> (i64, i64, i64, String) {
    let r = sqlx::query(
        "SELECT CAST(state AS BIGINT) AS s, CAST(attempts AS BIGINT) AS a, CAST(next_at AS BIGINT) AS n, \
                last_error AS e FROM notify_events WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    (r.get("s"), r.get("a"), r.get("n"), r.get::<String, _>("e"))
}

fn ev(kind: Kind) -> Event {
    Event::new(kind, "Agent fixer wants to run cargo test")
        .users(["alice", "bob"])
        .detail("It asks before it runs a command.")
        .path("/acme/api/agents/runs/7")
}

// ------------------------------------------------------------------------------ cleaning

#[test]
fn text_is_one_line_within_the_hubs_limits() {
    let c = clean(
        Event::new(Kind::ChecksFailed, "  line one\r\nline two\tend  ")
            .users(["alice"])
            .detail("a\nb\u{2028}c\u{0}d"),
    )
    .unwrap();
    assert_eq!(c.title, "line one line two end");
    assert_eq!(c.detail, "a b c d");
    // Bytes, not characters: the hub measures `len()`; a cut never splits a character.
    let long = "é".repeat(400);
    let c = clean(
        Event::new(Kind::MergeQueue, long.clone())
            .users(["alice"])
            .detail(long),
    )
    .unwrap();
    assert!(c.title.len() <= MAX_TITLE && c.title.chars().all(|ch| ch == 'é'));
    assert_eq!(c.title.len(), 200);
    assert!(c.detail.len() <= MAX_DETAIL && c.detail.len() >= MAX_DETAIL - 1);
    assert!(!c.title.contains('\n') && !c.detail.contains('\n'));
}

#[test]
fn a_path_is_local_to_the_hub_or_left_out() {
    let path = |p: &str| {
        clean(Event::new(Kind::Mentioned, "t").users(["a"]).path(p))
            .unwrap()
            .path
    };
    assert_eq!(path("/acme/api/pulls/3"), "/acme/api/pulls/3");
    for bad in [
        "https://evil.example/x",
        "//evil.example/x",
        "acme/api",
        "/a b",
        "/a\\b",
        "/a\nb",
        "",
    ] {
        assert_eq!(path(bad), "", "{bad:?}");
    }
    assert_eq!(path(&format!("/{}", "x".repeat(MAX_PATH))), "");
}

#[test]
fn people_are_valid_logins_without_repeats_and_an_event_for_nobody_is_dropped() {
    let c = clean(
        Event::new(Kind::Mentioned, "t").users(["alice", "bad name", "", "alice", "a/b", "bob"]),
    )
    .unwrap();
    assert_eq!(c.users, ["alice", "bob"]);
    assert!(clean(Event::new(Kind::Mentioned, "t").users(["no way", "x/y"])).is_none());
    assert!(clean(Event::new(Kind::Mentioned, "t")).is_none());
    assert!(
        clean(Event::new(Kind::Mentioned, " \n ").users(["alice"])).is_none(),
        "no title"
    );
    let e = Event::new(Kind::Mentioned, "t")
        .users(["alice", "bob"])
        .except("alice");
    assert_eq!(e.users, ["bob"]);
}

#[test]
fn every_event_name_round_trips_and_is_the_hubs() {
    let names: Vec<&str> = Kind::ALL.iter().map(|k| k.name()).collect();
    assert_eq!(
        names,
        [
            "agent_question",
            "agent_finished",
            "checks_failed",
            "restack_conflict",
            "merge_queue",
            "policy_changed",
            "review_requested",
            "mentioned"
        ]
    );
    for k in Kind::ALL {
        assert_eq!(Kind::parse(k.name()), Some(k));
    }
    assert_eq!(Kind::parse("nope"), None);
}

#[test]
fn mentions_are_whole_words_after_an_at() {
    assert_eq!(mentions("@alice please look, cc @bob."), ["alice", "bob"]);
    assert_eq!(mentions("mail me at a@b.com or @carol_d"), ["carol_d"]);
    assert_eq!(mentions("@alice @alice (@dan)"), ["alice", "dan"]);
    assert!(mentions("@@x @ y @- @.").is_empty());
    assert_eq!(mentions("`@alice`"), ["alice"]);
    assert!(mentions("no one here").is_empty());
}

// ------------------------------------------------------------------------------ queue

#[tokio::test]
async fn a_long_list_of_people_is_split_into_calls_of_at_most_fifty() {
    let pool = pool_with(&[("acme/api", 1)]).await;
    let users: Vec<String> = (0..120).map(|i| format!("user{i}")).collect();
    let n = record(
        &pool,
        "acme/api",
        Event::new(Kind::PolicyChanged, "Policy changed").users(&users),
    )
    .await
    .unwrap();
    assert_eq!(n, 3);
    let sink = Recorder::new(vec![]);
    dispatcher(&pool, &sink).deliver_due(now()).await.unwrap();
    let sizes: Vec<usize> = sink.sent().iter().map(|(_, o)| o.users.len()).collect();
    assert_eq!(sizes, [50, 50, 20]);
    assert!(
        sink.sent()
            .iter()
            .all(|(_, o)| o.repo == "acme/api" && o.event == "policy_changed")
    );
}

#[tokio::test]
async fn only_a_known_owner_name_repository_gets_rows() {
    let pool = pool_with(&[("acme/api", 1), ("lonely", 2)]).await;
    for repo in ["unknown/repo", "lonely", "a/b/c", "/x", "x/"] {
        assert_eq!(
            record(&pool, repo, ev(Kind::AgentQuestion)).await.unwrap(),
            0,
            "{repo}"
        );
    }
    assert_eq!(counts(&pool).await.unwrap(), (0, 0, 0));
    assert_eq!(
        record(&pool, "acme/api", ev(Kind::AgentQuestion))
            .await
            .unwrap(),
        1
    );
    // `emit` swallows errors: a closed pool is logged, not raised.
    pool.close().await;
    emit(&pool, "acme/api", ev(Kind::AgentQuestion)).await;
}

#[tokio::test]
async fn one_cause_is_told_once() {
    let pool = pool_with(&[("acme/api", 1), ("acme/web", 2)]).await;
    let e = || ev(Kind::AgentFinished).dedup("run:7:finished");
    assert_eq!(record(&pool, "acme/api", e()).await.unwrap(), 1);
    assert_eq!(record(&pool, "acme/api", e()).await.unwrap(), 0);
    // The key is per repository; no key always writes.
    assert_eq!(record(&pool, "acme/web", e()).await.unwrap(), 1);
    assert_eq!(
        record(&pool, "acme/api", ev(Kind::AgentFinished))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        record(&pool, "acme/api", ev(Kind::AgentFinished))
            .await
            .unwrap(),
        1
    );
    assert_eq!(counts(&pool).await.unwrap().0, 4);
}

#[tokio::test]
async fn a_repository_holds_a_bounded_number_of_undelivered_rows() {
    let pool = pool_with(&[("acme/api", 1), ("acme/web", 2)]).await;
    sqlx::query(
        "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < $2) \
         INSERT INTO notify_events (ns, id, event, users_json, title, created_at, next_at) \
         SELECT $1, n, 'mentioned', '[]', 't', 1, 1 FROM s",
    )
    .bind(vec![1_u8; 4])
    .bind(MAX_PENDING)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        record(&pool, "acme/api", ev(Kind::Mentioned))
            .await
            .unwrap(),
        0,
        "full"
    );
    assert_eq!(
        record(&pool, "acme/web", ev(Kind::Mentioned))
            .await
            .unwrap(),
        1,
        "other repository"
    );
}

// ------------------------------------------------------------------------------ delivery

#[tokio::test]
async fn a_row_is_posted_as_the_hub_names_its_fields_and_marked_delivered() {
    let pool = pool_with(&[("acme/api", 1)]).await;
    record(&pool, "acme/api", ev(Kind::AgentQuestion))
        .await
        .unwrap();
    let sink = Recorder::new(vec![Ok(2)]);
    let d = dispatcher(&pool, &sink);
    assert_eq!(d.deliver_due(now()).await.unwrap(), 1);
    let sent = sink.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "01010101-1");
    let json = serde_json::to_value(&sent[0].1).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "event": "agent_question",
            "repo": "acme/api",
            "users": ["alice", "bob"],
            "title": "Agent fixer wants to run cargo test",
            "detail": "It asks before it runs a command.",
            "path": "/acme/api/agents/runs/7",
        })
    );
    assert_eq!(counts(&pool).await.unwrap(), (0, 1, 0));
    let told: i64 = sqlx::query_scalar("SELECT CAST(notified AS BIGINT) FROM notify_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(told, 2);
    // Delivered rows are not posted again.
    assert_eq!(d.deliver_due(now() + 10).await.unwrap(), 0);
    assert_eq!(sink.sent().len(), 1);
}

#[tokio::test]
async fn a_failed_post_backs_off_and_is_retried_until_the_hub_answers() {
    let pool = pool_with(&[("acme/api", 1)]).await;
    record(&pool, "acme/api", ev(Kind::ChecksFailed))
        .await
        .unwrap();
    let sink = Recorder::new(vec![Err(retry("hub is down")), Err(retry("still down"))]);
    let d = dispatcher(&pool, &sink);
    let t0 = now();
    assert_eq!(d.deliver_due(t0).await.unwrap(), 1);
    let (state, attempts, next, err) = row(&pool, 1).await;
    assert_eq!(
        (state, attempts, next, err.as_str()),
        (0, 1, t0 + backoff_secs(1), "hub is down")
    );
    // Not due yet: nothing is posted.
    assert_eq!(d.deliver_due(t0 + backoff_secs(1) - 1).await.unwrap(), 0);
    assert_eq!(sink.sent().len(), 1);
    let t1 = t0 + backoff_secs(1);
    assert_eq!(d.deliver_due(t1).await.unwrap(), 1);
    let (_, attempts, next, err) = row(&pool, 1).await;
    assert_eq!(
        (attempts, next, err.as_str()),
        (2, t1 + backoff_secs(2), "still down")
    );
    let t2 = t1 + backoff_secs(2);
    assert_eq!(d.deliver_due(t2).await.unwrap(), 1);
    assert_eq!(counts(&pool).await.unwrap(), (0, 1, 0));
    assert_eq!(sink.sent().len(), 3);
    assert!(
        sink.sent().windows(2).all(|w| w[0].0 == w[1].0),
        "same id on every try"
    );
}

#[tokio::test]
async fn a_row_the_hub_refuses_is_not_retried_and_a_day_old_row_is_given_up() {
    let pool = pool_with(&[("acme/api", 1)]).await;
    record(&pool, "acme/api", ev(Kind::Mentioned))
        .await
        .unwrap();
    record(&pool, "acme/api", ev(Kind::MergeQueue))
        .await
        .unwrap();
    let sink = Recorder::new(vec![
        Err(Failure {
            permanent: true,
            message: "invalid request\nsecond line".into(),
        }),
        Err(retry("down")),
    ]);
    let d = dispatcher(&pool, &sink);
    let t0 = now();
    d.deliver_due(t0).await.unwrap();
    let (state, _, _, err) = row(&pool, 1).await;
    assert_eq!((state, err.as_str()), (2, "invalid request second line"));
    assert_eq!(row(&pool, 2).await.0, 0);
    // A day later the second row is given up on its next failure.
    let late = t0 + GIVE_UP_SECS + 1;
    sqlx::query("UPDATE notify_events SET next_at = $1 WHERE id = 2")
        .bind(late)
        .execute(&pool)
        .await
        .unwrap();
    let sink2 = Recorder::new(vec![Err(retry("down"))]);
    dispatcher(&pool, &sink2).deliver_due(late).await.unwrap();
    assert_eq!(row(&pool, 2).await.0, 2);
    assert_eq!(counts(&pool).await.unwrap(), (0, 0, 2));
    // Nothing more is posted for either.
    assert_eq!(
        dispatcher(&pool, &sink2)
            .deliver_due(late + 1000)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn a_leased_row_is_hidden_from_another_worker_until_the_lease_ends() {
    let pool = pool_with(&[("acme/api", 1)]).await;
    record(&pool, "acme/api", ev(Kind::PolicyChanged))
        .await
        .unwrap();
    // A worker that died after the lease: `next_at` is one lease ahead.
    let t = now();
    sqlx::query("UPDATE notify_events SET next_at = $1")
        .bind(t + 60)
        .execute(&pool)
        .await
        .unwrap();
    let sink = Recorder::new(vec![]);
    let d = dispatcher(&pool, &sink);
    assert_eq!(d.deliver_due(t + 30).await.unwrap(), 0);
    assert_eq!(d.deliver_due(t + 60).await.unwrap(), 1);
    assert_eq!(sink.sent().len(), 1);
}

#[tokio::test]
async fn rows_carry_the_current_name_of_their_repository_and_never_cross_repositories() {
    let pool = pool_with(&[("acme/api", 1), ("acme/web", 2)]).await;
    record(&pool, "acme/api", ev(Kind::AgentFinished).users(["carol"]))
        .await
        .unwrap();
    record(
        &pool,
        "acme/web",
        Event::new(Kind::MergeQueue, "Landed").users(["dave"]),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE repos SET name = 'acme/api2' WHERE name = 'acme/api'")
        .execute(&pool)
        .await
        .unwrap();
    let sink = Recorder::new(vec![]);
    dispatcher(&pool, &sink).deliver_due(now()).await.unwrap();
    let mut got: Vec<(String, Vec<String>)> = sink
        .sent()
        .into_iter()
        .map(|(_, o)| (o.repo, o.users))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            (
                "acme/api2".to_owned(),
                vec!["alice".into(), "bob".into(), "carol".into()]
            ),
            ("acme/web".to_owned(), vec!["dave".into()]),
        ]
    );
    // A row whose repository was deleted is not posted.
    record(
        &pool,
        "acme/web",
        Event::new(Kind::MergeQueue, "Again").users(["dave"]),
    )
    .await
    .unwrap();
    sqlx::query("DELETE FROM repos WHERE name = 'acme/web'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        dispatcher(&pool, &sink).deliver_due(now()).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn without_a_hub_nothing_is_posted_and_old_rows_are_pruned() {
    let pool = pool_with(&[("acme/api", 1)]).await;
    record(&pool, "acme/api", ev(Kind::AgentFinished))
        .await
        .unwrap();
    let d = Dispatcher::new(pool.clone(), None, DispatchConfig::default());
    let t = now();
    assert_eq!(d.deliver_due(t).await.unwrap(), 0);
    assert_eq!(counts(&pool).await.unwrap(), (1, 0, 0));
    // A day on, the unposted row is given up; a week after that it is deleted.
    d.prune(t + GIVE_UP_SECS + 5).await.unwrap();
    assert_eq!(counts(&pool).await.unwrap(), (0, 0, 1));
    d.prune(t + GIVE_UP_SECS + 5 + KEEP_SECS + 1).await.unwrap();
    assert_eq!(counts(&pool).await.unwrap(), (0, 0, 0));
}

// ------------------------------------------------------------------------------ the hub call

#[cfg(feature = "forgejo")]
mod hub {
    use asc_core::transport_policy::Policy;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::forgejo::{ForgejoClient, InternalToken};
    use crate::notify::worker::HubSink;

    /// A hub on loopback that answers each request from `answers` (status, body) and keeps what it got.
    async fn mock(answers: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for (status, body) in answers {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let want: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse().ok())
                            })
                            .unwrap_or(0);
                        if rest.len() >= want {
                            break;
                        }
                    }
                }
                log.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(String::from_utf8_lossy(&buf).into_owned());
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(reply.as_bytes()).await;
                let _ = s.shutdown().await;
            }
        });
        (url, seen)
    }

    fn sink(url: &str) -> HubSink {
        let client = ForgejoClient::new_under(
            url,
            Some(InternalToken::new("internal-secret").unwrap()),
            Policy::LOOPBACK_HTTP,
        )
        .unwrap();
        HubSink(Arc::new(client))
    }

    fn out() -> Outgoing {
        Outgoing {
            event: "checks_failed".into(),
            repo: "acme/api".into(),
            users: vec!["alice".into()],
            title: "Checks failed".into(),
            detail: String::new(),
            path: "/acme/api/changes/ab".into(),
        }
    }

    #[tokio::test]
    async fn the_event_goes_to_the_internal_endpoint_with_the_internal_token() {
        let (url, seen) = mock(vec![(200, r#"{"notified":1}"#)]).await;
        let told = sink(&url).deliver("0a0b-3", &out()).await.unwrap();
        assert_eq!(told, 1);
        let req = seen.lock().unwrap().remove(0);
        let lower = req.to_ascii_lowercase();
        assert!(
            req.starts_with("POST /api/internal/asc/event HTTP/1.1"),
            "{req}"
        );
        assert!(
            lower.contains("authorization: bearer internal-secret"),
            "{req}"
        );
        assert!(lower.contains("x-asc-event-id: 0a0b-3"), "{req}");
        assert!(lower.contains("application/json"), "{req}");
        let body: serde_json::Value =
            serde_json::from_str(req.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["event"], "checks_failed");
        assert_eq!(body["repo"], "acme/api");
        assert_eq!(body["users"], serde_json::json!(["alice"]));
        assert_eq!(body["path"], "/acme/api/changes/ab");
    }

    #[tokio::test]
    async fn the_hub_refusing_is_final_and_the_hub_failing_is_retried() {
        let (url, _) = mock(vec![
            (400, r#"{"message":"invalid request"}"#),
            (404, "{}"),
            (429, "{}"),
            (500, "{}"),
            (503, "{}"),
        ])
        .await;
        let s = sink(&url);
        let got = [
            s.deliver("x-1", &out()).await.unwrap_err(),
            s.deliver("x-1", &out()).await.unwrap_err(),
            s.deliver("x-1", &out()).await.unwrap_err(),
            s.deliver("x-1", &out()).await.unwrap_err(),
            s.deliver("x-1", &out()).await.unwrap_err(),
        ];
        let permanent: Vec<bool> = got.iter().map(|f| f.permanent).collect();
        assert_eq!(permanent, [true, true, false, false, false]);
        // Nothing is listening any more: unreachable is retried.
        let down = s.deliver("x-1", &out()).await.unwrap_err();
        assert!(!down.permanent, "{down:?}");
    }

    #[tokio::test]
    async fn the_hub_answering_nonsense_is_not_posted_again() {
        let (url, _) = mock(vec![(200, "not json")]).await;
        let e = sink(&url).deliver("x-2", &out()).await.unwrap_err();
        assert!(e.permanent, "{e:?}");
    }
}
