//! `WatchRefs`, the ref-update stream (SPEC §21.3.8, PLAN U76): request validation, the resume
//! cursor, the pattern matcher, the periodic re-authorisation of a long-lived stream, and the
//! client side.
//!
//! The server half of the stream (the `ref_log` reader and the ACL filter) is in `asc-serve`; it
//! parses the request with [`parse_request`] so the service layer and the handler agree on every
//! limit. The cursor is opaque to clients: [`Cursor`] is a version tag and the `ref_log` sequence
//! number, and the server treats a presented cursor as untrusted input.

use std::future::Future;
use std::time::Duration;

use asc_core::id::RefTarget;
use futures::StreamExt;
use futures::stream;
use tokio::time::{self, MissedTickBehavior};
use tonic::{Status, Streaming};

use crate::client::{CallError, RpcClient};
use crate::pb;
use crate::server::RpcStream;

/// Most patterns in one request.
pub const MAX_PATTERNS: usize = 8;
/// Longest pattern, in bytes.
pub const MAX_PATTERN_LEN: usize = 128;
/// The patterns used when a request names none.
pub const DEFAULT_PATTERNS: [&str; 2] = ["master", "release/*"];
/// Longest ref name an event may carry, in bytes.
pub const MAX_EVENT_NAME: usize = 4096;

const TAG: &[u8; 2] = b"w1";

/// A position in a repository's ref-update log. Events are delivered in strictly increasing
/// cursor order; resuming at a cursor delivers exactly the events after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cursor(pub u64);

impl Cursor {
    /// The wire form: a two byte version tag and the position, big endian.
    pub fn to_bytes(self) -> Vec<u8> {
        let mut v = TAG.to_vec();
        v.extend_from_slice(&self.0.to_be_bytes());
        v
    }

    /// Decodes a cursor; `None` for any other length or version.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let rest = b.strip_prefix(TAG.as_slice())?;
        let n: [u8; 8] = rest.try_into().ok()?;
        Some(Self(u64::from_be_bytes(n)))
    }
}

/// `*` matches any run of bytes, slashes included; everything else is literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// A validated pattern list. Each pattern is a full ref name glob: a name without the `refs/`
/// prefix means `refs/heads/<name>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patterns(Vec<String>);

impl Patterns {
    /// Validates `raw`: at most [`MAX_PATTERNS`] entries, each 1 to [`MAX_PATTERN_LEN`] bytes with
    /// no control character. An empty list means [`DEFAULT_PATTERNS`].
    pub fn parse(raw: &[String]) -> Result<Self, Status> {
        if raw.len() > MAX_PATTERNS {
            return Err(Status::invalid_argument("too many patterns"));
        }
        let mut out = Vec::with_capacity(raw.len().max(DEFAULT_PATTERNS.len()));
        let given: Vec<&str> = if raw.is_empty() {
            DEFAULT_PATTERNS.to_vec()
        } else {
            raw.iter().map(String::as_str).collect()
        };
        for p in given {
            if p.is_empty() || p.len() > MAX_PATTERN_LEN || p.chars().any(char::is_control) {
                return Err(Status::invalid_argument("invalid pattern"));
            }
            out.push(if p.starts_with("refs/") {
                p.to_owned()
            } else {
                format!("refs/heads/{p}")
            });
        }
        Ok(Self(out))
    }

    /// Whether any pattern matches the full ref name `name`.
    pub fn matches(&self, name: &str) -> bool {
        self.0.iter().any(|p| glob_match(p, name))
    }

    /// The normalised patterns.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

/// A validated `WatchRefsRequest`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watch {
    pub patterns: Patterns,
    /// `None` starts at the newest update.
    pub resume: Option<Cursor>,
}

/// Validates the limits of `req`; `INVALID_ARGUMENT` for an over-limit pattern list, a bad pattern
/// or a cursor that is not ours.
pub fn parse_request(req: &pb::WatchRefsRequest) -> Result<Watch, Status> {
    let patterns = Patterns::parse(&req.patterns)?;
    let resume = if req.resume.is_empty() {
        None
    } else {
        Some(
            Cursor::from_bytes(&req.resume)
                .ok_or_else(|| Status::invalid_argument("bad resume cursor"))?,
        )
    };
    Ok(Watch { patterns, resume })
}

/// Ends `inner` with an error as soon as `check` fails, which it runs every `every`. The service
/// layer uses it so a stream that outlives its token (or the caller's read access) closes.
pub(crate) fn rechecked<T, F, Fut>(inner: RpcStream<T>, every: Duration, check: F) -> RpcStream<T>
where
    T: Send + 'static,
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), Status>> + Send + 'static,
{
    let mut tick = time::interval_at(time::Instant::now() + every, every);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    stream::unfold(Some((inner, tick, check)), |state| async move {
        let (mut inner, mut tick, check) = state?;
        loop {
            tokio::select! {
                item = inner.next() => {
                    return item.map(|i| (i, Some((inner, tick, check))));
                }
                _ = tick.tick() => {
                    if let Err(e) = check().await {
                        return Some((Err(e), None));
                    }
                }
            }
        }
    })
    .boxed()
}

/// One ref update received from the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdated {
    pub name: String,
    /// `None`: the ref was created.
    pub old: Option<RefTarget>,
    /// `None`: the ref was deleted.
    pub new: Option<RefTarget>,
    /// Pass to [`RpcClient::watch_refs`] to continue after this event.
    pub resume: Cursor,
}

/// An open `WatchRefs` stream. It remembers the cursor of the last event, so a caller that lost
/// the stream resubscribes with [`RefWatch::resume`].
pub struct RefWatch {
    stream: Streaming<pb::RefEvent>,
    last: Option<Cursor>,
}

impl RefWatch {
    /// The cursor of the last event received, or the one the stream started from.
    pub fn resume(&self) -> Option<Cursor> {
        self.last
    }

    /// The next event; `Ok(None)` when the server closed the stream. A server that breaks the
    /// protocol (a bad name, a cursor that does not advance) is an error.
    pub async fn next(&mut self) -> Result<Option<RefUpdated>, CallError> {
        let Some(ev) = self.stream.message().await? else {
            return Ok(None);
        };
        let bad = |m: &str| CallError::Protocol(m.to_owned());
        if ev.name.is_empty()
            || ev.name.len() > MAX_EVENT_NAME
            || ev.name.chars().any(char::is_control)
        {
            return Err(bad("invalid ref name in an event"));
        }
        let resume = Cursor::from_bytes(&ev.resume).ok_or_else(|| bad("invalid event cursor"))?;
        if self.last.is_some_and(|l| resume <= l) {
            return Err(bad("event cursor does not advance"));
        }
        let target = |t: &Option<pb::RefTarget>| t.as_ref().map(RefTarget::try_from).transpose();
        let (old, new) = (target(&ev.old)?, target(&ev.new)?);
        self.last = Some(resume);
        Ok(Some(RefUpdated {
            name: ev.name,
            old,
            new,
            resume,
        }))
    }
}

impl RpcClient {
    /// Opens the ref-update stream for `patterns` (empty: `master` and `release/*`), starting after
    /// `resume`, or at the newest update when it is `None`. The limits are checked before the call.
    pub async fn watch_refs(
        &self,
        patterns: &[String],
        resume: Option<Cursor>,
    ) -> Result<RefWatch, CallError> {
        Patterns::parse(patterns).map_err(|e| CallError::Invalid(e.message().to_owned()))?;
        let req = pb::WatchRefsRequest {
            repo: self.repo_ref(),
            patterns: patterns.to_vec(),
            resume: resume.map_or_else(Vec::new, Cursor::to_bytes).into(),
        };
        let stream = self
            .primary()
            .watch_refs(self.req(req, None))
            .await?
            .into_inner();
        Ok(RefWatch {
            stream,
            last: resume,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use bytes::Bytes;
    use asc_core::id::ChangesetId;
    use tokio::sync::oneshot;
    use tokio_stream::wrappers::TcpListenerStream;

    use super::*;
    use crate::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
    use crate::server::{CallCtx, RepoHandlers, RepoService, ServerInfo, WatchHandler};

    fn cs(n: u8) -> RefTarget {
        RefTarget::Changeset(ChangesetId::from_bytes([n; 32]))
    }

    #[test]
    fn cursors_round_trip_and_reject_foreign_bytes() {
        let c = Cursor(0x0102_0304_0506_0708);
        assert_eq!(Cursor::from_bytes(&c.to_bytes()), Some(c));
        for bad in [
            &b""[..],
            b"w1",
            b"w2\0\0\0\0\0\0\0\0",
            b"w1\0\0\0\0\0\0\0\0\0",
        ] {
            assert_eq!(Cursor::from_bytes(bad), None);
        }
    }

    #[test]
    fn patterns_default_normalise_and_match() {
        let d = Patterns::parse(&[]).unwrap();
        assert!(d.matches("refs/heads/master"));
        assert!(d.matches("refs/heads/release/1.0"));
        assert!(!d.matches("refs/heads/feature"));
        assert!(!d.matches("refs/heads/masterful"));
        let p = Patterns::parse(&["refs/tags/v*".into(), "dev".into()]).unwrap();
        assert!(p.matches("refs/tags/v1"));
        assert!(p.matches("refs/heads/dev"));
        assert!(!p.matches("refs/heads/devx"));
    }

    #[test]
    fn over_limit_pattern_lists_are_invalid() {
        let many: Vec<String> = (0..=MAX_PATTERNS).map(|i| format!("b{i}")).collect();
        assert_eq!(
            Patterns::parse(&many).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let eight: Vec<String> = (0..MAX_PATTERNS).map(|i| format!("b{i}")).collect();
        assert!(Patterns::parse(&eight).is_ok());
        let long = vec!["a".repeat(MAX_PATTERN_LEN + 1)];
        assert!(Patterns::parse(&long).is_err());
        assert!(Patterns::parse(&["a".repeat(MAX_PATTERN_LEN)]).is_ok());
        assert!(Patterns::parse(&[String::new()]).is_err());
        assert!(Patterns::parse(&["a\nb".into()]).is_err());
        let bad_resume = pb::WatchRefsRequest {
            resume: Bytes::from_static(b"junk"),
            ..Default::default()
        };
        assert!(parse_request(&bad_resume).is_err());
    }

    /// Reads tokens `ok` (any number of checks) and `short` (two checks, then expired).
    struct Auth {
        short_checks: AtomicUsize,
    }

    #[async_trait]
    impl Authorizer for Auth {
        async fn authorize(
            &self,
            token: Option<&BearerToken>,
            _repo: &str,
            _access: Access,
        ) -> Result<Principal, AuthError> {
            match token.map(BearerToken::expose) {
                Some("ok") => Ok(Principal { id: "alice".into() }),
                Some("short") if self.short_checks.fetch_add(1, Ordering::SeqCst) < 2 => {
                    Ok(Principal { id: "alice".into() })
                }
                _ => Err(AuthError::Unauthenticated),
            }
        }
    }

    /// Replays a fixed log after the request cursor, then stays open.
    struct Log(Vec<pb::RefEvent>);

    #[async_trait]
    impl WatchHandler for Log {
        async fn watch_refs(
            &self,
            _ctx: &CallCtx,
            req: pb::WatchRefsRequest,
        ) -> Result<RpcStream<pb::RefEvent>, Status> {
            let w = parse_request(&req)?;
            let after = w.resume.unwrap_or(Cursor(0));
            let rows: Vec<_> = self
                .0
                .iter()
                .filter(|e| Cursor::from_bytes(&e.resume).unwrap() > after)
                .cloned()
                .map(Ok)
                .collect();
            Ok(stream::iter(rows).chain(stream::pending()).boxed())
        }
    }

    fn event(seq: u64) -> pb::RefEvent {
        pb::RefEvent {
            name: "refs/heads/master".into(),
            old: (seq > 1).then(|| (&cs(seq as u8 - 1)).into()),
            new: Some((&cs(seq as u8)).into()),
            resume: Bytes::from(Cursor(seq).to_bytes()),
        }
    }

    async fn serve(recheck: Duration) -> (RpcClient, RpcClient, oneshot::Sender<()>) {
        asc_core::transport_policy::set_config_allow_loopback_http(true);
        let handlers = Arc::new(RepoHandlers::new());
        handlers
            .set_watch(Arc::new(Log((1..=5).map(event).collect())))
            .unwrap();
        let auth = Auth {
            short_checks: AtomicUsize::new(0),
        };
        let svc = RepoService::new(handlers, Arc::new(auth), ServerInfo::default())
            .with_watch_recheck(recheck)
            .into_service();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(svc)
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    let _ = rx.await;
                })
                .await;
        });
        let url = format!("http://{addr}");
        let client =
            |t: &str| RpcClient::connect(&url, "o/r", Some(BearerToken::new(t).unwrap())).unwrap();
        (client("ok"), client("short"), tx)
    }

    #[tokio::test]
    async fn events_arrive_in_order_and_resume_has_no_gap_or_duplicate() {
        let (ok, _, _guard) = serve(Duration::from_secs(60)).await;
        let mut w = ok.watch_refs(&[], Some(Cursor(0))).await.unwrap();
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(w.next().await.unwrap().unwrap().resume);
        }
        assert_eq!(seen, [Cursor(1), Cursor(2), Cursor(3)]);
        assert_eq!(w.resume(), Some(Cursor(3)));
        drop(w);
        let mut again = ok.watch_refs(&[], w_resume(&seen)).await.unwrap();
        let first = again.next().await.unwrap().unwrap();
        assert_eq!(first.resume, Cursor(4));
        assert_eq!(first.old, Some(cs(3)));
        assert_eq!(again.next().await.unwrap().unwrap().resume, Cursor(5));
    }

    fn w_resume(seen: &[Cursor]) -> Option<Cursor> {
        seen.last().copied()
    }

    #[tokio::test]
    async fn an_over_limit_pattern_list_is_invalid_argument_on_the_server() {
        let (ok, _, _guard) = serve(Duration::from_secs(60)).await;
        let req = pb::WatchRefsRequest {
            repo: ok.repo_ref(),
            patterns: (0..=MAX_PATTERNS).map(|i| format!("b{i}")).collect(),
            ..Default::default()
        };
        let err = ok
            .primary()
            .watch_refs(ok.req(req, None))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        // The client refuses it before sending.
        let many: Vec<String> = (0..=MAX_PATTERNS).map(|i| format!("b{i}")).collect();
        assert!(matches!(
            ok.watch_refs(&many, None).await,
            Err(CallError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn the_stream_closes_when_the_token_expires() {
        let (_, short, _guard) = serve(Duration::from_millis(30)).await;
        let mut w = short.watch_refs(&[], Some(Cursor(0))).await.unwrap();
        for _ in 0..5 {
            w.next().await.unwrap().unwrap();
        }
        // The open call and the first re-check pass; the next re-checks fail.
        let end = tokio::time::timeout(Duration::from_secs(5), w.next())
            .await
            .expect("the stream closes");
        match end {
            Err(CallError::Status { code, .. }) => assert_eq!(code, tonic::Code::Unauthenticated),
            other => panic!("expected the stream to fail unauthenticated, got {other:?}"),
        }
        assert_eq!(w.resume(), Some(Cursor(5)));
    }
}
