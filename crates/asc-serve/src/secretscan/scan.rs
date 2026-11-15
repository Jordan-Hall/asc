//! Scanning files of a repository: the bounded loop between the engine's file reads and the pure
//! detectors of [`asc_core::secrets`].
//!
//! Work is bounded three ways (`[secrets] max_files`, `max_scan_bytes`, `max_file_bytes`): a file is
//! skipped when it is on the policy's `allow_paths`, when it is over `max_file_bytes` or when it is
//! binary; the scan stops, and says `truncated`, when it has read `max_files` files or
//! `max_scan_bytes` bytes. Identical content is read once however many paths hold it.

use std::collections::HashMap;

use asc_core::id::{ChangesetId, FileId};
use asc_core::policy::SecretsPolicy;
use asc_core::secrets::{self, RuleSet, Severity, Skipped};
use tonic::Status;

use super::Config;
use crate::idmode::Eng;

/// A file to look at.
#[derive(Clone, Debug)]
pub struct FileRef {
    pub path: Vec<u8>,
    pub file: FileId,
    pub size: u64,
    /// The changeset the file was added by (or the revision scanned).
    pub changeset: ChangesetId,
}

/// A secret found in a file. It holds no value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: Vec<u8>,
    pub rule_id: String,
    pub severity: Severity,
    pub line: u32,
    pub column: u32,
    pub fingerprint: String,
    pub preview: String,
    pub changeset: ChangesetId,
}

impl Found {
    /// `<path>:<line> <rule id>`, the text a refusal carries.
    pub fn location(&self) -> String {
        format!(
            "{}:{} {}",
            String::from_utf8_lossy(&self.path),
            self.line,
            self.rule_id
        )
    }
}

/// What a scan did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub found: Vec<Found>,
    /// Distinct files read and scanned.
    pub files_scanned: u32,
    pub skipped_large: u32,
    pub skipped_binary: u32,
    /// The scan stopped at a limit before it saw every file.
    pub truncated: bool,
}

/// Scans `files` in order.
pub async fn scan_files(
    engine: &Eng,
    rules: &RuleSet,
    policy: &SecretsPolicy,
    cfg: &Config,
    key: &[u8; 32],
    files: Vec<FileRef>,
) -> Result<Report, Status> {
    let mut scan = Scan::new(engine, rules, policy, cfg, key);
    scan.feed(files).await?;
    Ok(scan.finish())
}

/// A scan fed in batches: the limits, the identical-content cache and the report are carried from
/// one [`feed`](Self::feed) to the next, so the files of many changesets are read without all of
/// their references being held at once, and the result is the one a single [`scan_files`] gives.
pub struct Scan<'a> {
    engine: &'a Eng,
    rules: &'a RuleSet,
    policy: &'a SecretsPolicy,
    cfg: &'a Config,
    key: &'a [u8; 32],
    report: Report,
    /// Per distinct content: the hits.
    by_content: HashMap<FileId, Vec<Hit>>,
    bytes_read: u64,
}

impl<'a> Scan<'a> {
    pub fn new(
        engine: &'a Eng,
        rules: &'a RuleSet,
        policy: &'a SecretsPolicy,
        cfg: &'a Config,
        key: &'a [u8; 32],
    ) -> Self {
        Self {
            engine,
            rules,
            policy,
            cfg,
            key,
            report: Report::default(),
            by_content: HashMap::new(),
            bytes_read: 0,
        }
    }

    /// Whether a limit stopped the scan: further files are not read.
    pub fn stopped(&self) -> bool {
        self.report.truncated
    }

    /// Scans `files` after the ones fed before.
    pub async fn feed(&mut self, files: Vec<FileRef>) -> Result<(), Status> {
        let (rules, cfg) = (self.rules, self.cfg);
        for f in files {
            if self.report.truncated {
                break;
            }
            if self.policy.allows(&f.path) {
                continue;
            }
            let hits = if let Some(h) = self.by_content.get(&f.file) {
                h.clone()
            } else {
                if f.size > cfg.max_file_bytes.0 {
                    self.report.skipped_large += 1;
                    continue;
                }
                if self.report.files_scanned as usize >= cfg.max_files
                    || self.bytes_read.saturating_add(f.size) > cfg.max_scan_bytes.0
                {
                    self.report.truncated = true;
                    break;
                }
                let data = self
                    .engine
                    .read_file(f.file, f.size)
                    .await
                    .map_err(|e| crate::review::internal("secret scan read", &e))?;
                self.bytes_read = self.bytes_read.saturating_add(f.size);
                let scanned = rules.scan(&data);
                let hits = match scanned.skipped {
                    Some(Skipped::Binary) => {
                        self.report.skipped_binary += 1;
                        Vec::new()
                    }
                    Some(Skipped::TooLarge) => {
                        self.report.skipped_large += 1;
                        Vec::new()
                    }
                    None => {
                        self.report.files_scanned += 1;
                        scanned
                            .hits
                            .iter()
                            .map(|h| {
                                let info = &rules.infos()[h.rule];
                                let value = &data[h.value.clone()];
                                Hit {
                                    rule_id: info.id.clone(),
                                    severity: h.severity,
                                    line: h.line,
                                    column: h.column,
                                    fingerprint: super::store::fingerprint(
                                        self.key, &info.id, value,
                                    ),
                                    preview: secrets::preview(value),
                                }
                            })
                            .collect()
                    }
                };
                self.by_content.insert(f.file, hits.clone());
                hits
            };
            for h in hits {
                self.report.found.push(Found {
                    path: f.path.clone(),
                    rule_id: h.rule_id,
                    severity: h.severity,
                    line: h.line,
                    column: h.column,
                    fingerprint: h.fingerprint,
                    preview: h.preview,
                    changeset: f.changeset,
                });
            }
        }
        Ok(())
    }

    /// The report so far.
    pub fn finish(self) -> Report {
        self.report
    }
}

#[derive(Clone)]
struct Hit {
    rule_id: String,
    severity: Severity,
    line: u32,
    column: u32,
    fingerprint: String,
    preview: String,
}
