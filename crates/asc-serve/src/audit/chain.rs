//! The audit event and its hash chain (docs/BUILDOUT.md section 3).
//!
//! Events of one scope form a chain: `hash = BLAKE3(prev_hash || canonical encoding)`, where the
//! encoding names the scope, the sequence number and every field of the event. An edited field,
//! a removed event, an event moved to another scope or position, and a removed newest event all
//! break the chain at a definite sequence number ([`verify`]).

use std::collections::BTreeMap;

use super::{Actor, Outcome, Scope};

/// The `prev_hash` of the first event of a scope.
pub const GENESIS: [u8; 32] = [0; 32];

/// Key-derivation context of the chain hash. Changing it breaks every stored chain.
const DOMAIN: &str = "asc audit chain v1";

/// One stored audit event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub seq: u64,
    /// A UUID; the same in every scope the event was written to.
    pub id: String,
    pub time_ms: i64,
    pub actor: Actor,
    pub on_behalf_of: Option<Actor>,
    pub action: String,
    pub repo: String,
    pub target: String,
    pub outcome: Outcome,
    pub request_id: String,
    pub remote_addr: String,
    pub user_agent: String,
    pub details: BTreeMap<String, String>,
    pub prev_hash: [u8; 32],
    pub hash: [u8; 32],
}

fn put(h: &mut blake3::Hasher, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

/// The hash of `event` in `scope` on top of `prev`. `event.prev_hash` and `event.hash` are not
/// read: the caller passes `prev` itself.
pub fn event_hash(prev: &[u8; 32], scope: &Scope, event: &Event) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key(DOMAIN);
    h.update(prev);
    put(&mut h, scope.kind.as_str().as_bytes());
    put(&mut h, scope.name.as_bytes());
    h.update(&event.seq.to_le_bytes());
    put(&mut h, event.id.as_bytes());
    h.update(&event.time_ms.to_le_bytes());
    put(&mut h, event.actor.kind.as_bytes());
    put(&mut h, event.actor.name.as_bytes());
    match &event.on_behalf_of {
        Some(a) => {
            h.update(&[1]);
            put(&mut h, a.kind.as_bytes());
            put(&mut h, a.name.as_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
    put(&mut h, event.action.as_bytes());
    put(&mut h, event.repo.as_bytes());
    put(&mut h, event.target.as_bytes());
    h.update(&[event.outcome.code()]);
    put(&mut h, event.request_id.as_bytes());
    put(&mut h, event.remote_addr.as_bytes());
    put(&mut h, event.user_agent.as_bytes());
    h.update(&(event.details.len() as u64).to_le_bytes());
    for (k, v) in &event.details {
        put(&mut h, k.as_bytes());
        put(&mut h, v.as_bytes());
    }
    *h.finalize().as_bytes()
}

/// Lower case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What a scope's stored state says should hold before the first checked event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// The sequence number the first checked event must have.
    pub seq: u64,
    /// The hash the first checked event must name as its `prev_hash`.
    pub prev: [u8; 32],
}

/// Where a chain stopped holding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Broken {
    /// The sequence number at which it failed.
    pub seq: u64,
    pub reason: String,
    /// Events that checked out before it.
    pub checked: u64,
}

/// Checks `events` (in sequence order) against `anchor`: no gap, each `prev_hash` is the hash of
/// the event before, and each stored hash is the recomputed one. Returns the hash of the last
/// event checked, or where the chain broke.
pub fn verify<'a>(
    scope: &Scope,
    anchor: Anchor,
    events: impl IntoIterator<Item = &'a Event>,
) -> Result<([u8; 32], u64), Broken> {
    let mut prev = anchor.prev;
    let mut checked = 0;
    for (want_seq, e) in (anchor.seq..).zip(events) {
        if e.seq != want_seq {
            return Err(Broken {
                seq: want_seq,
                reason: format!("event {want_seq} is missing"),
                checked,
            });
        }
        if e.prev_hash != prev {
            return Err(Broken {
                seq: e.seq,
                reason: "the previous hash does not match the event before".into(),
                checked,
            });
        }
        if event_hash(&prev, scope, e) != e.hash {
            return Err(Broken {
                seq: e.seq,
                reason: "the event was changed after it was written".into(),
                checked,
            });
        }
        prev = e.hash;
        checked += 1;
    }
    Ok((prev, checked))
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use crate::audit::ScopeKind;

    pub fn scope() -> Scope {
        Scope {
            kind: ScopeKind::Repo,
            name: "acme/app".into(),
        }
    }

    pub fn chain(n: u64) -> (Scope, Vec<Event>) {
        let s = scope();
        let mut prev = GENESIS;
        let events = (1..=n)
            .map(|seq| {
                let mut e = Event {
                    seq,
                    id: format!("00000000-0000-0000-0000-{seq:012}"),
                    time_ms: 1_000 + seq as i64,
                    actor: Actor::new("user", "alice"),
                    on_behalf_of: None,
                    action: "repo.ref.update".into(),
                    repo: "acme/app".into(),
                    target: "refs/heads/main".into(),
                    outcome: Outcome::Success,
                    request_id: "r".into(),
                    remote_addr: "203.0.113.0/24".into(),
                    user_agent: "asc/1".into(),
                    details: BTreeMap::from([("via".to_owned(), "asc-push".to_owned())]),
                    prev_hash: prev,
                    hash: [0; 32],
                };
                e.hash = event_hash(&prev, &s, &e);
                prev = e.hash;
                e
            })
            .collect();
        (s, events)
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{chain, scope};
    use super::*;
    use crate::audit::ScopeKind;

    fn anchor() -> Anchor {
        Anchor {
            seq: 1,
            prev: GENESIS,
        }
    }

    #[test]
    fn an_intact_chain_verifies() {
        let (_, c) = chain(5);
        let (head, n) = verify(&scope(), anchor(), &c).unwrap();
        assert_eq!((head, n), (c[4].hash, 5));
        assert_eq!(verify(&scope(), anchor(), &[]).unwrap(), (GENESIS, 0));
    }

    #[test]
    fn every_field_is_covered_by_the_hash() {
        let (_, base) = chain(3);
        type Edit = Box<dyn Fn(&mut Event)>;
        let edits: Vec<Edit> = vec![
            Box::new(|e| e.id.push('x')),
            Box::new(|e| e.time_ms += 1),
            Box::new(|e| e.actor.name = "mallory".into()),
            Box::new(|e| e.actor.kind = "agent".into()),
            Box::new(|e| e.on_behalf_of = Some(Actor::new("user", "bob"))),
            Box::new(|e| e.action = "repo.ref.undo".into()),
            Box::new(|e| e.repo = "acme/other".into()),
            Box::new(|e| e.target.push('x')),
            Box::new(|e| e.outcome = Outcome::Denied),
            Box::new(|e| e.request_id.push('x')),
            Box::new(|e| e.remote_addr = "198.51.100.0/24".into()),
            Box::new(|e| e.user_agent.push('x')),
            Box::new(|e| {
                e.details.insert("k".into(), "v".into());
            }),
            Box::new(|e| {
                e.details.insert("via".into(), "other".into());
            }),
        ];
        for (i, edit) in edits.iter().enumerate() {
            let mut c = base.clone();
            edit(&mut c[1]);
            let broken = verify(&scope(), anchor(), &c).unwrap_err();
            assert_eq!(broken.seq, 2, "edit {i} is caught at the edited event");
        }
    }

    #[test]
    fn a_removed_event_is_caught_at_the_gap() {
        let (_, mut c) = chain(5);
        c.remove(2);
        let b = verify(&scope(), anchor(), &c).unwrap_err();
        assert_eq!(b.seq, 3);
        assert!(b.reason.contains("missing"));
    }

    #[test]
    fn a_swapped_prev_hash_is_caught() {
        let (_, mut c) = chain(3);
        c[2].prev_hash = GENESIS;
        let b = verify(&scope(), anchor(), &c).unwrap_err();
        assert_eq!(b.seq, 3);
    }

    #[test]
    fn an_event_moved_to_another_scope_is_caught() {
        let (_, c) = chain(2);
        let other = Scope {
            kind: ScopeKind::Owner,
            name: "acme".into(),
        };
        assert_eq!(verify(&other, anchor(), &c).unwrap_err().seq, 1);
    }

    #[test]
    fn a_chain_can_be_checked_from_a_retention_anchor() {
        let (_, c) = chain(6);
        let a = Anchor {
            seq: 4,
            prev: c[2].hash,
        };
        let (head, n) = verify(&scope(), a, &c[3..]).unwrap();
        assert_eq!((head, n), (c[5].hash, 3));
        let wrong = Anchor {
            seq: 4,
            prev: GENESIS,
        };
        assert_eq!(verify(&scope(), wrong, &c[3..]).unwrap_err().seq, 4);
    }

    #[test]
    fn hex_prints_lower_case() {
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
    }
}
