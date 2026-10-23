//! Git over SSH: the listener and public-key authentication (SPEC §25.11; PLAN U86).
//!
//! `asc serve` gains an optional SSH listener for plain `git clone`, `fetch` and `push`. It is a
//! `russh` server (SPEC §25.11.2) that accepts only public keys, resolved by the one
//! [`Authorizer`](asc_rpc::auth::Authorizer), and only the two pack commands, handed to a
//! [`PackExec`] (U87). Everything is behind the Cargo feature `git-ssh` and the runtime switch
//! `[ssh] enabled`.
//!
//! * [`auth`]: the login: user name, key policy, `identify_ssh_key` at the offer.
//! * [`policy`]: the command parser and the request table; no shell, nothing but two commands.
//! * [`host_key`]: host keys with the M30 key-file protections, `show` and `rotate`.
//! * [`limits`]: admission before authentication and the named caps of `[limits.ssh]`.
//! * [`audit`]: the login, command and refused-request records and the `asc_ssh_*` metrics.
//! * [`server`]: the accept loop and the per-connection handler.

pub mod audit;
pub mod auth;
pub mod host_key;
pub mod limits;
pub mod policy;
pub mod server;

use std::sync::Arc;

use asc_rpc::auth::Authorizer;

pub use audit::{Audit, AuditSink, SshMetrics, TracingAudit};
pub use server::{
    Out, PackExec, PackIo, PackOut, PackRequest, SshDeps, SshError, SshServer, Unavailable,
};

use crate::config::{AuthProvider, ServeConfig};
use crate::http::Metrics;
use crate::limits::Admission;

/// The `Version.features` entry present only while the listener is bound (SPEC §25.11.10).
pub const FEATURE: &str = "git.ssh";

/// Whether the listener runs, and if not, the reason to log once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// `[ssh] enabled = false`: nothing to say.
    Off,
    /// Enabled, but not bound for this reason.
    Skip(&'static str),
    Run,
}

/// The start check (SPEC §25.11.10, PLAN U86): the listener is bound only when `[ssh] enabled`
/// and `[git] enabled` are true, the git layer is actually serving (`git_live`), and the
/// authorizer can hold SSH keys: the token store, or Forgejo with the internal endpoint (K19).
/// Stage 1 Forgejo has only the public API, which cannot map a key to a user.
pub fn gate(cfg: &ServeConfig, git_live: bool) -> Gate {
    if !cfg.ssh.enabled {
        return Gate::Off;
    }
    if !cfg.git.enabled {
        return Gate::Skip("git protocol is disabled; ssh listener not started");
    }
    if !git_live {
        return Gate::Skip("git protocol is not available; ssh listener not started");
    }
    match cfg.auth_provider() {
        Some(AuthProvider::TokenStore) => Gate::Run,
        Some(AuthProvider::Forge) => {
            if cfg
                .forgejo
                .as_ref()
                .is_some_and(|f| f.internal_token_file.is_some())
            {
                Gate::Run
            } else {
                Gate::Skip(
                    "ssh with provider=forge needs the internal authz endpoint (K19); \
                     ssh listener not started",
                )
            }
        }
        None => Gate::Skip(
            "ssh needs an authorizer that holds SSH keys ([auth] provider); ssh listener not started",
        ),
    }
}

/// What [`start`] needs from the running server.
pub struct StartDeps {
    pub authorizer: Arc<dyn Authorizer>,
    pub admission: Arc<Admission>,
    pub metrics: Arc<Metrics>,
    pub exec: Arc<dyn PackExec>,
}

/// Binds the listener when the gate allows it. A host key that fails its checks, or an address
/// that cannot be bound, is an error: the operator asked for SSH and must not silently lose it.
/// Any other reason not to run is logged once and gives `Ok(None)`.
///
/// # Errors
/// [`SshError`] from the host keys or the bind.
pub async fn start(
    cfg: &ServeConfig,
    git_live: bool,
    deps: StartDeps,
) -> Result<Option<SshServer>, SshError> {
    match gate(cfg, git_live) {
        Gate::Off => return Ok(None),
        Gate::Skip(reason) => {
            tracing::warn!("{reason}");
            return Ok(None);
        }
        Gate::Run => {}
    }
    let audit = Audit::new(Arc::new(TracingAudit), SshMetrics::register(&deps.metrics));
    let server = SshServer::bind(
        &cfg.ssh,
        &cfg.limits.ssh,
        SshDeps {
            authorizer: deps.authorizer,
            admission: Some(deps.admission),
            audit,
            exec: deps.exec,
        },
    )
    .await?;
    for key in server.host_keys().keys() {
        tracing::info!(
            fingerprint = %key.fingerprint(),
            algorithm = %key.algorithm(),
            "ssh host key"
        );
    }
    if let Ok(addr) = server.local_addr() {
        tracing::info!(listen = %addr, "asc serve ssh listening");
    }
    Ok(Some(server))
}

/// `asc serve ssh host-key show`: each configured key's fingerprint and a `known_hosts` line.
///
/// # Errors
/// A missing or unusable host key file.
pub fn host_key_show(cfg: &ServeConfig) -> Result<String, host_key::HostKeyError> {
    let keys = host_key::HostKeys::load_existing(&cfg.ssh.host_key_files)?;
    let host = cfg
        .public_url
        .as_deref()
        .and_then(|u| url::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<host>".to_owned());
    Ok(keys.show_text(&host, cfg.ssh.listen.port()))
}

/// `asc serve ssh host-key rotate`: a new first host key. The text says what clients will see.
///
/// # Errors
/// An RSA first key, or a file that cannot be written.
pub fn host_key_rotate(cfg: &ServeConfig) -> Result<String, host_key::HostKeyError> {
    host_key::rotate(&cfg.ssh.host_key_files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(extra: &str) -> ServeConfig {
        ServeConfig::parse(&format!(
            "store = \"memory:///\"\ndb = \"sqlite::memory:\"\n{extra}"
        ))
        .unwrap()
    }

    const TOKEN_STORE: &str = "[auth]\nprovider = \"token_store\"\ntoken_key_file = \"k\"\n";

    #[test]
    fn the_gate_needs_every_condition() {
        let on = format!("{TOKEN_STORE}[ssh]\nenabled = true\n");
        assert_eq!(gate(&cfg(&on), true), Gate::Run);
        // Off: silent.
        assert_eq!(gate(&cfg(TOKEN_STORE), true), Gate::Off);
        // git disabled: not bound.
        let no_git = format!("{on}[git]\nenabled = false\n");
        assert!(
            matches!(gate(&cfg(&no_git), true), Gate::Skip(r) if r.contains("git protocol is disabled"))
        );
        // git layer not serving.
        assert!(matches!(gate(&cfg(&on), false), Gate::Skip(r) if r.contains("not available")));
        // No authorizer that holds keys.
        let none = "[ssh]\nenabled = true\n";
        assert!(matches!(gate(&cfg(none), true), Gate::Skip(r) if r.contains("[auth] provider")));
    }

    #[test]
    fn forge_needs_the_internal_endpoint() {
        let forge = |internal: &str| {
            format!(
                "[auth]\nprovider = \"forge\"\n[forgejo]\nurl = \"http://forge.example\"\n{internal}[ssh]\nenabled = true\n"
            )
        };
        let stage1 = forge("");
        assert!(
            matches!(gate(&cfg(&stage1), true), Gate::Skip(r) if r.contains("internal authz endpoint (K19)"))
        );
        // The [ssh] table must follow [forgejo]; the internal token file is a [forgejo] key.
        let stage2 = "[auth]\nprovider = \"forge\"\n[forgejo]\nurl = \"http://forge.example\"\ninternal_token_file = \"t\"\n[ssh]\nenabled = true\n";
        assert_eq!(gate(&cfg(stage2), true), Gate::Run);
    }
}
