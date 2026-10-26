//! The repository gates of `ServeBackend` on the authenticator's decisions (SPEC 22.7.3; PLAN M29).
//!
//! `/mcp` holds no credential after [`super::auth`] has used it, and the backend only knows the
//! principal. The authenticator authorises every repository-bearing `tools/call` with exactly one
//! `Authorizer::authorize_credential(credential, repo, read|write)` and records the decision in a
//! [`Ledger`] as a [`Vouch`] that lives as long as the call runs. [`LedgerGate`] answers the
//! backend's [`RepoGate`] and [`WriteGate`] questions from the ledger, so:
//!
//! * the backend runs only for a `(principal, repository)` pair the authorizer allowed for this
//!   very call, and a write tool only for a pair it allowed to write;
//! * anything that reaches the backend without passing the authenticator finds no vouch and is
//!   answered `NOT_FOUND`, the same text as a missing repository (fail closed);
//! * a vouch ends with the call, so it can never be reused by a later or concurrent request that
//!   was not authorised itself.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;

use super::audit::Mode;
use super::backend::{RepoGate, WriteGate};

/// Vouches held for one `(principal, repository)` pair.
#[derive(Debug, Clone, Copy, Default)]
struct Held {
    read: u32,
    write: u32,
}

/// The authorised calls in flight on this instance.
#[derive(Debug, Default)]
pub struct Ledger {
    held: Mutex<HashMap<(String, String), Held>>,
}

impl Ledger {
    /// A new, empty ledger.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Records that `principal` was authorised for `repo` with `mode` for one call. A write
    /// authorisation implies read. The vouch ends when the returned value is dropped.
    #[must_use]
    pub fn vouch(self: &Arc<Self>, principal: &str, repo: &str, mode: Mode) -> Vouch {
        let mut map = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let h = map
            .entry((principal.to_owned(), repo.to_owned()))
            .or_default();
        h.read = h.read.saturating_add(1);
        if mode == Mode::Write {
            h.write = h.write.saturating_add(1);
        }
        Vouch {
            ledger: Arc::clone(self),
            principal: principal.to_owned(),
            repo: repo.to_owned(),
            mode,
        }
    }

    fn held(&self, principal: &str, repo: &str) -> Held {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(principal.to_owned(), repo.to_owned()))
            .copied()
            .unwrap_or_default()
    }

    /// Pairs with a call in flight (diagnostics and tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether no authorised call is in flight.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One authorised call. Dropping it ends the authorisation.
#[derive(Debug)]
pub struct Vouch {
    ledger: Arc<Ledger>,
    principal: String,
    repo: String,
    mode: Mode,
}

impl Drop for Vouch {
    fn drop(&mut self) {
        let mut map = self
            .ledger
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let key = (
            std::mem::take(&mut self.principal),
            std::mem::take(&mut self.repo),
        );
        if let Some(h) = map.get_mut(&key) {
            h.read = h.read.saturating_sub(1);
            if self.mode == Mode::Write {
                h.write = h.write.saturating_sub(1);
            }
            if h.read == 0 && h.write == 0 {
                map.remove(&key);
            }
        }
    }
}

/// [`RepoGate`] and [`WriteGate`] over a [`Ledger`].
#[derive(Debug, Clone)]
pub struct LedgerGate(Arc<Ledger>);

impl LedgerGate {
    /// A gate that answers from `ledger`.
    #[must_use]
    pub fn new(ledger: Arc<Ledger>) -> Self {
        Self(ledger)
    }
}

#[async_trait]
impl RepoGate for LedgerGate {
    async fn can_read(&self, principal: &str, repo: &str) -> bool {
        self.0.held(principal, repo).read > 0
    }
}

#[async_trait]
impl WriteGate for LedgerGate {
    async fn can_write(&self, principal: &str, repo: &str) -> bool {
        self.0.held(principal, repo).write > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_vouch_opens_the_gate_for_its_pair_and_mode_until_it_drops() {
        let l = Ledger::new();
        let g = LedgerGate::new(l.clone());
        assert!(!g.can_read("alice", "acme/app").await);
        let read = l.vouch("alice", "acme/app", Mode::Read);
        assert!(g.can_read("alice", "acme/app").await);
        assert!(!g.can_write("alice", "acme/app").await, "read is not write");
        assert!(!g.can_read("alice", "acme/other").await);
        assert!(!g.can_read("bob", "acme/app").await);
        let write = l.vouch("alice", "acme/app", Mode::Write);
        assert!(g.can_write("alice", "acme/app").await);
        drop(write);
        assert!(!g.can_write("alice", "acme/app").await);
        assert!(
            g.can_read("alice", "acme/app").await,
            "the read vouch remains"
        );
        drop(read);
        assert!(!g.can_read("alice", "acme/app").await);
        assert!(l.is_empty(), "no entry outlives its calls");
    }

    #[tokio::test]
    async fn a_write_vouch_implies_read() {
        let l = Ledger::new();
        let g = LedgerGate::new(l.clone());
        let _v = l.vouch("alice", "acme/app", Mode::Write);
        assert!(g.can_read("alice", "acme/app").await);
        assert!(g.can_write("alice", "acme/app").await);
    }

    #[tokio::test]
    async fn concurrent_vouches_are_counted() {
        let l = Ledger::new();
        let g = LedgerGate::new(l.clone());
        let a = l.vouch("p", "o/r", Mode::Read);
        let b = l.vouch("p", "o/r", Mode::Read);
        drop(a);
        assert!(
            g.can_read("p", "o/r").await,
            "the second call is still vouched"
        );
        drop(b);
        assert!(!g.can_read("p", "o/r").await);
    }
}
