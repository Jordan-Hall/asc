//! `Maintenance.CheckHealth`: the probes of the hub's health page that only this server can make.
//! The object store takes a random probe object, gives it back and deletes it; the server's
//! signing key signs a fixed message and the signature verifies. The probes run at most once every
//! [`MIN_INTERVAL`]; a call in between gets the last result, so a page reloaded many times does not
//! write many objects.

use std::sync::Arc;
use std::time::{Duration, Instant};

use asc_core::sshsig;
use asc_rpc::server::wire;
use bytes::Bytes;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use tokio::sync::Mutex;

use crate::signing::ServerSigner;
use crate::storage::live::sanitize;
use crate::storage::model::PROBE_DIR;

/// What `Version.features` says when the server answers `CheckHealth`.
pub const FEATURE: &str = "health.v1";

/// The shortest time between two runs of the probes.
pub const MIN_INTERVAL: Duration = Duration::from_secs(5);

/// The longest a probe may take before it counts as failed.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The message the server key signs in its probe. It is not a changeset id, so the signature
/// cannot be passed off as one.
const KEY_PROBE_MESSAGE: &[u8] = b"asc health probe";

/// The result of one probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub state: wire::HealthState,
    pub millis: u32,
    pub detail: String,
}

impl Probe {
    fn ok(started: Instant) -> Self {
        Self {
            state: wire::HealthState::Ok,
            millis: elapsed_ms(started),
            detail: String::new(),
        }
    }

    fn failed(started: Instant, detail: impl Into<String>) -> Self {
        Self {
            state: wire::HealthState::Failed,
            millis: elapsed_ms(started),
            detail: detail.into(),
        }
    }

    fn not_configured() -> Self {
        Self {
            state: wire::HealthState::NotConfigured,
            millis: 0,
            detail: String::new(),
        }
    }

    pub fn to_wire(&self) -> wire::HealthProbe {
        wire::HealthProbe {
            state: self.state as i32,
            millis: self.millis,
            detail: self.detail.clone(),
        }
    }
}

fn elapsed_ms(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)
}

/// What the probes found, and when.
#[derive(Clone, Debug)]
pub struct Health {
    pub object_store: Probe,
    pub server_key: Probe,
    pub fingerprint: String,
    pub checked_at_ms: i64,
}

/// The things the probes check.
pub struct HealthProbes {
    store: Arc<dyn ObjectStore>,
    root: Path,
    signer: Option<Arc<ServerSigner>>,
    last: Mutex<Option<(Instant, Health)>>,
}

impl HealthProbes {
    /// The probes of the instance bucket `store` under `root`, and of the server key `signer`
    /// when one is set.
    pub fn new(store: Arc<dyn ObjectStore>, root: Path, signer: Option<Arc<ServerSigner>>) -> Self {
        Self {
            store,
            root,
            signer,
            last: Mutex::new(None),
        }
    }

    /// The result of the probes, made now unless they ran less than [`MIN_INTERVAL`] ago. Calls at
    /// the same time wait for one run.
    pub async fn check(&self, now_ms: i64) -> Health {
        let mut last = self.last.lock().await;
        if let Some((at, h)) = last.as_ref()
            && at.elapsed() < MIN_INTERVAL
        {
            return h.clone();
        }
        let h = Health {
            object_store: self.probe_store().await,
            server_key: self.probe_key(),
            fingerprint: self
                .signer
                .as_ref()
                .map(|s| s.public_key().fingerprint().to_owned())
                .unwrap_or_default(),
            checked_at_ms: now_ms,
        };
        *last = Some((Instant::now(), h.clone()));
        h
    }

    /// Writes a random object under `<root>/.asc-probe/`, reads it back, compares, and deletes it.
    /// A written object is deleted even when the read fails.
    async fn probe_store(&self) -> Probe {
        let started = Instant::now();
        let mut rnd = [0u8; 24];
        if getrandom::fill(&mut rnd).is_err() {
            return Probe::failed(started, "no randomness");
        }
        let name: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
        let location = self.root.clone().join(PROBE_DIR).join(name);
        let body = Bytes::from(rnd.to_vec());
        let round = async {
            self.store
                .put(&location, PutPayload::from_bytes(body.clone()))
                .await
                .map_err(|e| format!("write: {}", sanitize(&e)))?;
            let got = async {
                self.store
                    .get(&location)
                    .await
                    .map_err(|e| format!("read: {}", sanitize(&e)))?
                    .bytes()
                    .await
                    .map_err(|e| format!("read: {}", sanitize(&e)))
            }
            .await;
            let deleted = self
                .store
                .delete(&location)
                .await
                .map_err(|e| format!("delete: {}", sanitize(&e)));
            if got? != body {
                return Err("the object read back differs from the one written".to_owned());
            }
            deleted
        };
        match tokio::time::timeout(PROBE_TIMEOUT, round).await {
            Ok(Ok(())) => Probe::ok(started),
            Ok(Err(e)) => Probe::failed(started, e),
            Err(_) => Probe::failed(started, "the object store did not answer in 10 seconds"),
        }
    }

    /// Signs a fixed message with the server key and verifies the signature with the key's public
    /// half, as a reader of a server-made commit would.
    fn probe_key(&self) -> Probe {
        let Some(signer) = &self.signer else {
            return Probe::not_configured();
        };
        let started = Instant::now();
        let check = || -> Result<(), &'static str> {
            let armored = signer
                .sign_bytes(KEY_PROBE_MESSAGE)
                .map_err(|_| "the key does not sign")?;
            let sig = sshsig::Signature::parse(&armored)
                .map_err(|_| "the signature cannot be read back")?;
            if sig.key() != signer.public_key() {
                return Err("the signature names another key");
            }
            sig.verify(KEY_PROBE_MESSAGE)
                .map_err(|_| "the signature does not verify")
        };
        match check() {
            Ok(()) => Probe::ok(started),
            Err(e) => Probe::failed(started, e),
        }
    }
}

impl Health {
    pub fn to_wire(&self, now_ms: i64) -> wire::CheckHealthResponse {
        wire::CheckHealthResponse {
            object_store: Some(self.object_store.to_wire()),
            server_key: Some(self.server_key.to_wire()),
            server_key_fingerprint: self.fingerprint.clone(),
            checked_at_ms: self.checked_at_ms,
            now_ms,
        }
    }
}
