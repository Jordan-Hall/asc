//! Secret scanning (docs/BUILDOUT.md section 4).
//!
//! Every update that lands changes on a protected branch is scanned before the ref moves
//! (`policy::gate`), and a repository admin can scan any revision, change or range on demand
//! (`SecretScan.Scan`). The detectors are the pure engine in [`asc_core::secrets`]; this module
//! reads the files, bounds the work, keys the fingerprints and keeps the findings.
//!
//! A finding never carries the secret: [`store::fingerprint`] is a keyed hash, the preview shows at
//! most two characters, and the refusal of a push names the path, the line and the rule only. The
//! policy decides what a hit means (`[secrets] enabled`, `block_on_push`, `allow_paths`, custom
//! rules), always read from the trunk tip before the update, so a change cannot lift the scan that
//! binds it.
//!
//! * [`scan`]: the bounded loop over files.
//! * [`store`]: findings and the per-repository fingerprint key.
//! * [`rpc`]: the `SecretScan` service.

pub mod rpc;
pub mod scan;
pub mod store;

use std::sync::Arc;

use asc_core::id::ChangesetId;
use asc_core::object::{Change, Kind};
use asc_core::policy::SecretsPolicy;
use asc_core::secrets::RuleSet;
use serde::Deserialize;
use tonic::Status;

use crate::config::ByteSize;
use crate::idmode::Eng;
use crate::registry::Registry;
use scan::{FileRef, Report};

/// What `Version.features` adds for the `SecretScan` service.
pub const FEATURE: &str = "secretscan.v1";
/// Longest note on a finding, in bytes.
pub const MAX_NOTE: usize = 1024;
/// Findings in one `ListFindings` page when the request names no limit.
pub const DEFAULT_PAGE: usize = 50;
/// Findings in one `ListFindings` page, at most.
pub const MAX_PAGE: usize = 200;
/// Most changesets one scan reads the files of; a larger update is scanned over its newest ones.
pub const MAX_CHANGESETS: usize = 20_000;

/// `[secrets]` of the server config.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Files over this size are skipped (and counted).
    pub max_file_bytes: ByteSize,
    /// A scan stops after reading this many bytes.
    pub max_scan_bytes: ByteSize,
    /// A scan stops after reading this many files.
    pub max_files: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_file_bytes: ByteSize(ByteSize::MIB),
            max_scan_bytes: ByteSize(16 * ByteSize::MIB),
            max_files: 10_000,
        }
    }
}

impl Config {
    /// Checks the bounds. The error names the key.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_file_bytes.0 < ByteSize::KIB || self.max_file_bytes.0 > 8 * ByteSize::MIB {
            return Err("secrets.max_file_bytes must be 1KiB to 8MiB".into());
        }
        if self.max_scan_bytes.0 < self.max_file_bytes.0
            || self.max_scan_bytes.0 > 4 * ByteSize::GIB
        {
            return Err(
                "secrets.max_scan_bytes must be at least max_file_bytes and at most 4GiB".into(),
            );
        }
        if self.max_files == 0 || self.max_files > 1_000_000 {
            return Err("secrets.max_files must be 1 to 1000000".into());
        }
        Ok(())
    }
}

/// The scanner of a server.
pub struct Scanner {
    pub(crate) registry: Arc<Registry>,
    pub(crate) cfg: Config,
}

/// The place the server's single [`Scanner`] is registered, once.
#[derive(Default)]
pub struct ScannerSlot(std::sync::OnceLock<Arc<Scanner>>);

impl ScannerSlot {
    pub fn set(&self, s: Arc<Scanner>) -> bool {
        self.0.set(s).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<Scanner>> {
        self.0.get()
    }
}

impl Scanner {
    pub fn new(registry: Arc<Registry>, cfg: Config) -> Self {
        Self { registry, cfg }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The rules of `policy`: the built-in ones and its custom ones. The policy was validated when
    /// it was parsed, so a failure here is an internal error.
    pub fn rules(policy: &SecretsPolicy) -> Result<RuleSet, Status> {
        RuleSet::with_custom(&policy.rules).map_err(|e| crate::review::internal("secret rules", &e))
    }

    /// Scans the files that `changesets` add (`Write` of a regular file or an executable), newest
    /// changeset first, under `policy`.
    pub async fn scan_changesets(
        &self,
        engine: &Eng,
        repo_id: i64,
        policy: &SecretsPolicy,
        changesets: &[ChangesetId],
    ) -> Result<Report, Status> {
        let rules = Self::rules(policy)?;
        let key = store::repo_key(self.registry.pool(), repo_id, crate::review::now())
            .await
            .map_err(crate::review::sql)?;
        let added = added_files(engine, changesets).await?;
        let mut report =
            scan::scan_files(engine, &rules, policy, &self.cfg, &key, added.files).await?;
        // Changesets left out of the listing were never read: the scan did not cover the update.
        report.truncated |= added.truncated;
        Ok(report)
    }

    /// Scans `files` under `policy`.
    pub async fn scan_refs(
        &self,
        engine: &Eng,
        repo_id: i64,
        policy: &SecretsPolicy,
        files: Vec<FileRef>,
    ) -> Result<Report, Status> {
        let rules = Self::rules(policy)?;
        let key = store::repo_key(self.registry.pool(), repo_id, crate::review::now())
            .await
            .map_err(crate::review::sql)?;
        scan::scan_files(engine, &rules, policy, &self.cfg, &key, files).await
    }
}

/// The files `changesets` add, and whether the listing left changesets out.
#[derive(Clone, Debug, Default)]
pub struct AddedFiles {
    pub files: Vec<FileRef>,
    /// More than [`MAX_CHANGESETS`] changesets were named; only the first ones are listed. A scan
    /// over this listing did not see every file and must say so.
    pub truncated: bool,
}

/// The regular files `changesets` write, in the order of `changesets`. Reads at most
/// [`MAX_CHANGESETS`] changesets and reports when it left some out.
pub async fn added_files(engine: &Eng, changesets: &[ChangesetId]) -> Result<AddedFiles, Status> {
    let mut out = Vec::new();
    for &cs in changesets.iter().take(MAX_CHANGESETS) {
        let body = engine
            .get_changeset(cs)
            .await
            .map_err(|e| crate::review::internal("changeset", &e))?;
        for (path, change) in body.changes.as_slice() {
            if let Change::Write {
                kind: Kind::File | Kind::Exec,
                file,
                size,
                ..
            } = change
            {
                out.push(FileRef {
                    path: path.as_bytes().to_vec(),
                    file: *file,
                    size: *size,
                    changeset: cs,
                });
            }
        }
    }
    Ok(AddedFiles {
        files: out,
        truncated: changesets.len() > MAX_CHANGESETS,
    })
}

#[cfg(test)]
mod tests;
