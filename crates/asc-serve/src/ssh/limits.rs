//! Connection admission for the SSH listener (SPEC §25.11.7, §16.3).
//!
//! Admission runs before authentication and before any key-exchange work: the accept loop asks
//! [`SshLimits::admit`] for a [`ConnPermit`] and closes the connection with
//! `SSH_MSG_DISCONNECT` (`too many connections`) when it is refused. The caps are named in
//! `[limits.ssh]`: connections per replica, connections that have not authenticated yet (per
//! replica and per peer), new connections per second per peer, and connections per principal
//! once authenticated. Every table is bounded (`asc_rpc::limits`): a flood of peers shares an
//! overflow bucket, and counters hold entries only while a slot is held.
//!
//! Failed logins feed the same per-peer counters as failed HTTP requests: they are charged to
//! the peer's unverified `cheap` bucket in the server's [`Admission`], the bucket HTTP callers
//! without a verified token share (SPEC §15.6.2), and admission then refuses further
//! connections from a peer that has used it up.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use asc_rpc::limits::{CounterGuard, KeyedCounter, KeyedLimiter, MAX_KEYS, OnFull};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::SshLimitsConfig;
use crate::limits::{Admission, Caller, Class, Peer};

/// Units of a peer's `cheap` bucket one failed login costs. A burst of 400 allows 50 failures
/// at once and a sustained 25 a second, far above a person's typing and far below a guesser's.
pub const FAILED_LOGIN_CHARGE: u32 = 8;

/// Which limit refused a connection (the `limit:<which>` of the audit record).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// `max_connections` on this replica.
    Connections,
    /// `max_unauthenticated` on this replica.
    Unauthenticated,
    /// `max_unauthenticated_per_peer` for this peer.
    UnauthenticatedPeer,
    /// `new_connections_per_sec` for this peer.
    ConnectionRate,
    /// The peer's shared admission bucket is empty (failed logins, HTTP failures).
    Admission,
}

impl Refused {
    pub fn which(self) -> &'static str {
        match self {
            Self::Connections => "connections",
            Self::Unauthenticated => "unauthenticated",
            Self::UnauthenticatedPeer => "unauthenticated_per_peer",
            Self::ConnectionRate => "connection_rate",
            Self::Admission => "admission",
        }
    }
}

/// What an admitted connection holds. The unauthenticated share is released at login.
pub struct ConnPermit {
    _connection: OwnedSemaphorePermit,
    unauthenticated: Option<(OwnedSemaphorePermit, CounterGuard<Peer>)>,
}

impl ConnPermit {
    /// The connection authenticated: it no longer counts against the unauthenticated caps.
    pub fn authenticated(&mut self) {
        self.unauthenticated = None;
    }

    /// Whether the unauthenticated share is still held (for tests).
    pub fn is_unauthenticated(&self) -> bool {
        self.unauthenticated.is_some()
    }
}

/// The listener's limits.
pub struct SshLimits {
    cfg: SshLimitsConfig,
    connections: Arc<Semaphore>,
    unauthenticated: Arc<Semaphore>,
    per_peer: Arc<KeyedCounter<Peer>>,
    rate: KeyedLimiter<Peer>,
    per_principal: Arc<KeyedCounter<String>>,
    admission: Option<Arc<Admission>>,
}

impl SshLimits {
    /// `admission` is the server's, shared with the HTTP surfaces; `None` skips that check
    /// (tests of the other caps).
    pub fn new(cfg: &SshLimitsConfig, admission: Option<Arc<Admission>>) -> Self {
        Self {
            cfg: cfg.clone(),
            connections: Arc::new(Semaphore::new(cfg.max_connections as usize)),
            unauthenticated: Arc::new(Semaphore::new(cfg.max_unauthenticated as usize)),
            per_peer: Arc::new(KeyedCounter::new()),
            rate: KeyedLimiter::bounded(
                u64::from(cfg.new_connections_per_sec),
                u64::from(cfg.new_connections_burst),
                MAX_KEYS,
                OnFull::Overflow,
            ),
            per_principal: Arc::new(KeyedCounter::new()),
            admission,
        }
    }

    pub fn config(&self) -> &SshLimitsConfig {
        &self.cfg
    }

    /// Admits one new connection from `peer`, or says which limit refused it. Never waits.
    pub fn admit(&self, peer: IpAddr) -> Result<ConnPermit, Refused> {
        let key = Peer::from_ip(Some(peer));
        self.rate
            .check(&key, 1)
            .map_err(|_| Refused::ConnectionRate)?;
        let connection = self
            .connections
            .clone()
            .try_acquire_owned()
            .map_err(|_| Refused::Connections)?;
        let unauth = self
            .unauthenticated
            .clone()
            .try_acquire_owned()
            .map_err(|_| Refused::Unauthenticated)?;
        let peer_slot = self
            .per_peer
            .try_acquire(&key, self.cfg.max_unauthenticated_per_peer)
            .map_err(|_| Refused::UnauthenticatedPeer)?;
        if let Some(admission) = &self.admission {
            admission
                .admit(Caller::Unverified(key), Class::Cheap, 1)
                .map_err(|_| Refused::Admission)?;
        }
        Ok(ConnPermit {
            _connection: connection,
            unauthenticated: Some((unauth, peer_slot)),
        })
    }

    /// Takes one of `principal`'s connection slots (`max_connections_per_principal`).
    pub fn principal_slot(&self, principal: &str) -> Option<CounterGuard<String>> {
        self.per_principal
            .try_acquire(
                &principal.to_owned(),
                self.cfg.max_connections_per_principal,
            )
            .ok()
    }

    /// Charges a failed login to `peer`'s shared admission bucket.
    pub fn failed_login(&self, peer: IpAddr) {
        if let Some(admission) = &self.admission {
            let who = Caller::Unverified(Peer::from_ip(Some(peer)));
            for _ in 0..FAILED_LOGIN_CHARGE {
                // The permit is dropped at once; only the bucket's debit matters.
                if admission.admit(who, Class::Cheap, 1).is_err() {
                    break;
                }
            }
        }
    }

    /// Connections that hold a slot now (for tests).
    pub fn open_connections(&self) -> usize {
        self.cfg.max_connections as usize - self.connections.available_permits()
    }

    pub fn auth_timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.auth_timeout.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Limits, Secs};

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    fn cfg() -> SshLimitsConfig {
        SshLimitsConfig {
            max_connections: 6,
            max_unauthenticated: 4,
            max_unauthenticated_per_peer: 2,
            new_connections_per_sec: 1,
            new_connections_burst: 1_000,
            max_connections_per_principal: 2,
            auth_timeout: Secs(30),
            ..SshLimitsConfig::default()
        }
    }

    #[test]
    fn unauthenticated_connections_are_capped_per_peer_and_in_total() {
        let l = SshLimits::new(&cfg(), None);
        let a1 = l.admit(ip(1)).unwrap();
        let _a2 = l.admit(ip(1)).unwrap();
        assert_eq!(l.admit(ip(1)).err(), Some(Refused::UnauthenticatedPeer));
        let _b1 = l.admit(ip(2)).unwrap();
        let _b2 = l.admit(ip(2)).unwrap();
        // Four unauthenticated in total: a third peer is refused by the replica cap.
        assert_eq!(l.admit(ip(3)).err(), Some(Refused::Unauthenticated));
        // Dropping one frees its slots.
        drop(a1);
        l.admit(ip(3)).unwrap();
    }

    #[test]
    fn authenticating_releases_the_unauthenticated_share_only() {
        let l = SshLimits::new(&cfg(), None);
        let mut c1 = l.admit(ip(1)).unwrap();
        let mut c2 = l.admit(ip(1)).unwrap();
        assert!(c1.is_unauthenticated());
        c1.authenticated();
        c2.authenticated();
        assert!(!c1.is_unauthenticated());
        assert_eq!(l.open_connections(), 2);
        // The peer can open more while the first two stay authenticated.
        let _c3 = l.admit(ip(1)).unwrap();
        let _c4 = l.admit(ip(1)).unwrap();
    }

    #[test]
    fn the_replica_connection_cap_counts_authenticated_connections_too() {
        let mut limits = cfg();
        limits.max_unauthenticated = 6;
        limits.max_unauthenticated_per_peer = 6;
        let l = SshLimits::new(&limits, None);
        let mut held = Vec::new();
        for n in 0..6 {
            let mut c = l.admit(ip(n)).unwrap();
            c.authenticated();
            held.push(c);
        }
        assert_eq!(l.admit(ip(9)).err(), Some(Refused::Connections));
        held.pop();
        l.admit(ip(9)).unwrap();
    }

    #[test]
    fn new_connections_are_rate_limited_per_peer() {
        let mut limits = cfg();
        limits.new_connections_per_sec = 1;
        limits.new_connections_burst = 3;
        limits.max_unauthenticated = 60;
        limits.max_unauthenticated_per_peer = 60;
        limits.max_connections = 100;
        let l = SshLimits::new(&limits, None);
        let mut ok = 0;
        let mut refused = None;
        let mut held = Vec::new();
        for _ in 0..10 {
            match l.admit(ip(1)) {
                Ok(c) => {
                    ok += 1;
                    held.push(c);
                }
                Err(e) => refused = Some(e),
            }
        }
        assert!((3..=4).contains(&ok), "{ok}");
        assert_eq!(refused, Some(Refused::ConnectionRate));
        // Another peer has its own bucket.
        l.admit(ip(2)).unwrap();
    }

    #[test]
    fn a_principal_has_a_connection_cap() {
        let l = SshLimits::new(&cfg(), None);
        let a = l.principal_slot("pat:alice").unwrap();
        let _b = l.principal_slot("pat:alice").unwrap();
        assert!(l.principal_slot("pat:alice").is_none());
        assert!(l.principal_slot("pat:bob").is_some());
        drop(a);
        assert!(l.principal_slot("pat:alice").is_some());
    }

    #[test]
    fn failed_logins_drain_the_shared_admission_bucket() {
        let shared = Limits {
            cheap_burst: 40,
            cheap_rps: 1,
            ..Limits::default()
        };
        let admission = Arc::new(Admission::new(&shared));
        let mut limits = cfg();
        limits.new_connections_burst = 1_000;
        let l = SshLimits::new(&limits, Some(admission.clone()));
        let who = Caller::Unverified(Peer::from_ip(Some(ip(1))));
        // HTTP callers from that address share the bucket.
        assert!(admission.admit(who, Class::Cheap, 1).is_ok());
        for _ in 0..8 {
            l.failed_login(ip(1));
        }
        assert!(admission.admit(who, Class::Cheap, 1).is_err());
        assert_eq!(l.admit(ip(1)).err(), Some(Refused::Admission));
        // Another address is unaffected.
        l.admit(ip(2)).unwrap();
    }
}
