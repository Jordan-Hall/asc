//! Audit records and metrics of the SSH listener (SPEC §25.11.8).
//!
//! Every login attempt, every command and every refused channel request or open writes one
//! record. Records hold fingerprints (`SHA256:…`) and never a key blob or a private key
//! (SPEC §15.6.4 item 9): their types have no field that could hold one. The user name is the
//! client's own text, so it is cut and stripped of control characters before it is kept.
//!
//! The records go to an [`AuditSink`]. The server's sink writes them as `tracing` events with
//! the target `asc_ssh_audit`; tests install a sink that keeps them.

use std::net::IpAddr;
use std::sync::Arc;

use asc_rpc::auth::SshFingerprint;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;

use crate::http::{Labels, Metrics};

/// Longest user-name hint kept in a record.
pub const MAX_USER_HINT: usize = 64;

/// The outcome of a login attempt, as the audit record and the metric name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginOutcome {
    Ok,
    UnknownKey,
    User,
    KeyType,
    RsaBits,
    BadSignature,
    Method,
    /// Over a limit; the name says which (`connections`, `unauthenticated`, `auth_timeout`, …).
    Limit(&'static str),
}

impl LoginOutcome {
    /// `ok`, `rejected:<why>` or `limit:<which>`.
    pub fn label(&self) -> String {
        match self {
            Self::Ok => "ok".into(),
            Self::UnknownKey => "rejected:unknown_key".into(),
            Self::User => "rejected:user".into(),
            Self::KeyType => "rejected:key_type".into(),
            Self::RsaBits => "rejected:rsa_bits".into(),
            Self::BadSignature => "rejected:bad_signature".into(),
            Self::Method => "rejected:method".into(),
            Self::Limit(which) => format!("limit:{which}"),
        }
    }

    /// Whether this attempt failed (and so counts against the peer).
    pub fn failed(&self) -> bool {
        !matches!(self, Self::Ok)
    }
}

/// One login attempt.
#[derive(Clone, Debug)]
pub struct LoginRecord {
    pub peer: Option<IpAddr>,
    /// The SSH user name the client sent: a hint, never an identity.
    pub user_hint: String,
    /// The offered key's type name (`ssh-ed25519`, …); empty when no key was offered.
    pub key_type: String,
    pub fingerprint: Option<SshFingerprint>,
    pub outcome: LoginOutcome,
    /// For a success: who the key acts as, its kind (`user` or `deploy`) and its id.
    pub principal: Option<String>,
    pub key_kind: Option<&'static str>,
    pub key_id: Option<String>,
}

impl LoginRecord {
    pub fn new(peer: Option<IpAddr>, user: &str, outcome: LoginOutcome) -> Self {
        Self {
            peer,
            user_hint: sanitize(user, MAX_USER_HINT),
            key_type: String::new(),
            fingerprint: None,
            outcome,
            principal: None,
            key_kind: None,
            key_id: None,
        }
    }
}

/// A channel request or channel open the listener refused.
#[derive(Clone, Debug)]
pub struct RefusedRecord {
    pub peer: Option<IpAddr>,
    pub fingerprint: SshFingerprint,
    pub principal: String,
    /// A name from [`super::policy::Request::name`]: a fixed set.
    pub request: &'static str,
}

/// One command (the record of SPEC §25.8 item 9, with `transport = ssh`).
#[derive(Clone, Debug)]
pub struct CommandRecord {
    pub peer: Option<IpAddr>,
    pub fingerprint: SshFingerprint,
    pub principal: String,
    pub verb: &'static str,
    /// The repository the client named, as parsed (`owner/name`).
    pub repo: String,
    /// `ok`, `rejected:<why>`, `limit:<which>` or `killed`.
    pub outcome: String,
    pub exit_status: Option<u32>,
    pub millis: u64,
}

/// Receives the records.
pub trait AuditSink: Send + Sync {
    fn login(&self, record: &LoginRecord);
    fn refused(&self, record: &RefusedRecord);
    fn command(&self, record: &CommandRecord);
}

/// The production sink: `tracing` events with the target `asc_ssh_audit`.
pub struct TracingAudit;

impl AuditSink for TracingAudit {
    fn login(&self, r: &LoginRecord) {
        tracing::info!(
            target: "asc_ssh_audit",
            kind = "login",
            outcome = %r.outcome.label(),
            peer = %peer(r.peer),
            user = %r.user_hint,
            key_type = %r.key_type,
            fingerprint = %fingerprint(r.fingerprint),
            principal = r.principal.as_deref().unwrap_or("-"),
            key_kind = r.key_kind.unwrap_or("-"),
            key_id = r.key_id.as_deref().unwrap_or("-"),
            "ssh login"
        );
    }

    fn refused(&self, r: &RefusedRecord) {
        tracing::info!(
            target: "asc_ssh_audit",
            kind = "refused",
            request = r.request,
            peer = %peer(r.peer),
            fingerprint = %r.fingerprint,
            principal = %r.principal,
            "ssh request refused"
        );
    }

    fn command(&self, r: &CommandRecord) {
        tracing::info!(
            target: "asc_ssh_audit",
            kind = "command",
            transport = "ssh",
            verb = r.verb,
            repo = %r.repo,
            outcome = %r.outcome,
            exit_status = r.exit_status.map_or(-1, i64::from),
            millis = r.millis,
            peer = %peer(r.peer),
            fingerprint = %r.fingerprint,
            principal = %r.principal,
            "ssh command"
        );
    }
}

fn peer(ip: Option<IpAddr>) -> String {
    ip.map_or_else(|| "-".to_owned(), |p| p.to_string())
}

fn fingerprint(fp: Option<SshFingerprint>) -> String {
    fp.map_or_else(|| "-".to_owned(), |f| f.to_string())
}

/// `text` with control characters removed, cut to `max` characters.
pub fn sanitize(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// The `asc_ssh_*` metrics (SPEC §25.11.8). Every label value comes from a fixed set.
pub struct SshMetrics {
    connections: Family<Labels, Counter>,
    logins: Family<Labels, Counter>,
    refused: Family<Labels, Counter>,
    sessions: Gauge,
}

impl SshMetrics {
    /// Registers the metrics on `metrics` (exposed on the admin listener).
    pub fn register(metrics: &Metrics) -> Arc<Self> {
        let m = Arc::new(Self {
            connections: Family::default(),
            logins: Family::default(),
            refused: Family::default(),
            sessions: Gauge::default(),
        });
        metrics.with_registry(|r| {
            r.register(
                "asc_ssh_connections",
                "SSH connections by result (accepted, or the limit that refused them)",
                m.connections.clone(),
            );
            r.register(
                "asc_ssh_logins",
                "SSH login attempts by result",
                m.logins.clone(),
            );
            r.register(
                "asc_ssh_refused",
                "SSH channel requests and opens refused, by request",
                m.refused.clone(),
            );
            r.register(
                "asc_ssh_sessions",
                "Authenticated SSH connections now open",
                m.sessions.clone(),
            );
        });
        m
    }

    /// Metrics that are registered nowhere (tests, or a server with no metrics sink).
    pub fn detached() -> Arc<Self> {
        Arc::new(Self {
            connections: Family::default(),
            logins: Family::default(),
            refused: Family::default(),
            sessions: Gauge::default(),
        })
    }

    fn label(name: &str, value: &str) -> Labels {
        vec![(name.to_owned(), value.to_owned())]
    }

    pub fn connection(&self, result: &str) {
        self.connections
            .get_or_create(&Self::label("result", result))
            .inc();
    }

    pub fn login(&self, result: &str) {
        self.logins
            .get_or_create(&Self::label("result", result))
            .inc();
    }

    pub fn refusal(&self, request: &str) {
        self.refused
            .get_or_create(&Self::label("request", request))
            .inc();
    }

    pub fn session_opened(&self) {
        self.sessions.inc();
    }

    pub fn session_closed(&self) {
        self.sessions.dec();
    }

    /// Sessions open now (for tests).
    pub fn sessions(&self) -> i64 {
        self.sessions.get()
    }

    /// A counter's value (for tests).
    pub fn logins_total(&self, result: &str) -> u64 {
        self.logins
            .get_or_create(&Self::label("result", result))
            .get()
    }

    pub fn refused_total(&self, request: &str) -> u64 {
        self.refused
            .get_or_create(&Self::label("request", request))
            .get()
    }

    pub fn connections_total(&self, result: &str) -> u64 {
        self.connections
            .get_or_create(&Self::label("result", result))
            .get()
    }
}

/// Where the listener sends its records and counts.
#[derive(Clone)]
pub struct Audit {
    sink: Arc<dyn AuditSink>,
    metrics: Arc<SshMetrics>,
}

impl Audit {
    pub fn new(sink: Arc<dyn AuditSink>, metrics: Arc<SshMetrics>) -> Self {
        Self { sink, metrics }
    }

    pub fn metrics(&self) -> &Arc<SshMetrics> {
        &self.metrics
    }

    pub fn login(&self, record: LoginRecord) {
        self.metrics.login(&record.outcome.label());
        self.sink.login(&record);
    }

    pub fn refused(&self, record: RefusedRecord) {
        self.metrics.refusal(record.request);
        self.sink.refused(&record);
    }

    pub fn command(&self, record: CommandRecord) {
        self.sink.command(&record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_have_the_spec_labels() {
        assert_eq!(LoginOutcome::Ok.label(), "ok");
        assert_eq!(LoginOutcome::UnknownKey.label(), "rejected:unknown_key");
        assert_eq!(LoginOutcome::User.label(), "rejected:user");
        assert_eq!(LoginOutcome::KeyType.label(), "rejected:key_type");
        assert_eq!(LoginOutcome::RsaBits.label(), "rejected:rsa_bits");
        assert_eq!(LoginOutcome::BadSignature.label(), "rejected:bad_signature");
        assert_eq!(LoginOutcome::Method.label(), "rejected:method");
        assert_eq!(
            LoginOutcome::Limit("connections").label(),
            "limit:connections"
        );
        assert!(!LoginOutcome::Ok.failed());
        assert!(LoginOutcome::User.failed());
    }

    #[test]
    fn the_user_hint_is_cut_and_cleaned() {
        let long = format!("a\u{1b}[2J\n{}", "b".repeat(500));
        let r = LoginRecord::new(None, &long, LoginOutcome::User);
        assert_eq!(r.user_hint.chars().count(), MAX_USER_HINT);
        assert!(!r.user_hint.contains('\u{1b}') && !r.user_hint.contains('\n'));
    }
}
