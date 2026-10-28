//! Dependency scanning and update proposals (`deps.proto`, docs/DEPENDENCIES.md, docs/BUILDOUT.md
//! section 15).
//!
//! The server reads the manifests and lockfiles of a revision ([`parse`]), matches the packages against
//! an advisory source ([`osv`]: the OSV API, a mirror, or an advisory file) behind the network guard and
//! an egress allowlist ([`egress`]), asks the public registries for newer versions ([`latest`]), and
//! records updates as changes on `agent/deps/...` refs ([`updates`], [`apply`], [`rewrite`]). Rules, in
//! the order they matter:
//!
//! * **Nothing of the repository runs.** Files are read by pure parsers with size and depth bounds
//!   (`parse`, `yaml`); no resolver, package manager or script is started.
//! * **A failed lookup is not a clean one.** A package whose advisories could not be had is counted as
//!   unknown, and the landing gate refuses on it when the policy asks for blocking.
//! * **Trunk decides.** `.asc/deps.toml` is read from the trunk tip, never from the revision under
//!   review; the endpoints the server calls are the operator's `[deps]` section, never a repository file.
//! * **Never trunk.** A proposal is a changeset on a draft ref under `agent/deps/`; landing it is a
//!   person's decision, through the usual review and gate.
//! * **Registry text is data.** A version from a registry is written into a file only if it is a plain
//!   literal, and only after the file parsers read the result back.

pub mod advisory;
pub mod apply;
pub mod config;
pub mod cvss;
pub mod egress;
pub mod facts;
pub mod gate;
pub mod imports;
pub mod hub;
pub mod latest;
pub mod latest_facts;
pub mod listing;
pub mod lock;
pub mod model;
pub mod names;
pub mod ordered;
pub mod osv;
pub mod parse;
pub mod plan;
pub mod ranges;
pub mod repo_config;
pub mod requirement;
pub mod rewrite;
pub mod rpc;
pub mod safe;
pub mod scan;
pub mod settings;
pub mod store;
pub mod sumdb;
pub mod updates;
pub mod version;
pub mod workflow;
pub mod yaml;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_facts;
#[cfg(test)]
mod tests_hub;

use std::sync::{Arc, OnceLock};

pub use config::Config;

use crate::mutations::ServerMutations;
use crate::netguard::{Guard, NetGuardConfig};
use crate::registry::{Extensions, Registry};

/// What `Version.features` adds.
pub const FEATURE: &str = "deps.v1";
/// The refs proposals are recorded on.
pub const REF_PREFIX: &str = "refs/heads/agent/deps/";

/// The place the server's single [`hub::DepsHub`] is registered, once.
#[derive(Default)]
pub struct DepsSlot(OnceLock<Arc<hub::DepsHub>>);

impl DepsSlot {
    pub fn set(&self, hub: Arc<hub::DepsHub>) -> bool {
        self.0.set(hub).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<hub::DepsHub>> {
        self.0.get()
    }
}

/// The unit's one line in `register_all`. Off (`[deps] enabled = false`) it registers nothing, and a
/// policy that blocks on advisories then refuses what touches a dependency file.
pub fn register(
    ext: &Extensions,
    registry: &Arc<Registry>,
    mutations: &Arc<ServerMutations>,
    net: &NetGuardConfig,
    cfg: &Config,
    public_url: Option<&str>,
) -> Result<(), String> {
    if !cfg.enabled {
        return Ok(());
    }
    let cfg = &cfg.clone().resolved();
    cfg.validate(net)?;
    let egress = Arc::new(egress::Egress::new(
        Guard::new(net.clone()),
        cfg.allowed_hosts(),
    ));
    let hub = hub::DepsHub::new(
        registry.clone(),
        mutations.clone(),
        egress,
        cfg.clone(),
        public_url.map(str::to_owned),
        osv::system_clock(),
    );
    if let Some(s) = ext.security.get() {
        let check: Arc<dyn gate::DepsCheck> = hub.clone();
        if !s.deps_gate.set(check) {
            return Err("the dependency check was already registered".into());
        }
    }
    if ext.deps.set(hub) {
        Ok(())
    } else {
        Err("the dependency service was already registered".into())
    }
}

/// What `Version.features` adds when the service is mounted.
pub fn features(ext: &Extensions) -> Vec<String> {
    if ext.deps.get().is_some() {
        vec![FEATURE.to_owned()]
    } else {
        Vec::new()
    }
}
