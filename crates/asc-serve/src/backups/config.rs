//! `[backups]`: where scheduled backups go. Safe when absent: backups are off.
//!
//! ```toml
//! [backups]
//! enabled = true
//! key_file = "/etc/asc/backup.key"        # 32 raw bytes or 64 hex digits, mode 0600; encrypts uploads
//! staging_dir = "/var/lib/asc/backup-staging"
//!
//! [backups.targets.local-disk]
//! kind = "local"
//! path = "/srv/backups"
//!
//! [backups.targets.r2-backup]
//! kind = "s3"
//! bucket = "asc-backups"
//! endpoint = "https://example.r2.cloudflarestorage.com"
//! region = "auto"
//! prefix = "prod/"
//! access_key_id_env = "ASC_BACKUP_S3_KEY_ID"
//! secret_access_key_env = "ASC_BACKUP_S3_SECRET"
//! ```
//!
//! A credential is never in this file: an S3 target names the environment variables that hold it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

/// One place a backup is kept.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum TargetConfig {
    /// A directory on this machine; each backup is a subdirectory.
    Local { path: PathBuf },
    /// An S3-compatible bucket; each backup is a prefix of `prefix`.
    S3 {
        bucket: String,
        #[serde(default)]
        endpoint: String,
        #[serde(default)]
        region: String,
        #[serde(default)]
        prefix: String,
        #[serde(default)]
        access_key_id_env: String,
        #[serde(default)]
        secret_access_key_env: String,
    },
}

impl TargetConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Local { .. } => "local",
            Self::S3 { .. } => "s3",
        }
    }

    /// Where it is, without a credential.
    pub fn location(&self) -> String {
        match self {
            Self::Local { path } => path.display().to_string(),
            Self::S3 { bucket, prefix, .. } => format!("{bucket}/{}", prefix.trim_matches('/')),
        }
    }
}

/// `[backups]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BackupsConfig {
    pub enabled: bool,
    /// The key an upload to an S3 target is encrypted under, and the sealed vault key.
    pub key_file: Option<PathBuf>,
    /// Where an S3 backup is written before it is uploaded (default: the system temporary directory).
    pub staging_dir: Option<PathBuf>,
    /// The targets, by name (1 to 64 characters of `[a-z0-9_-]`).
    pub targets: BTreeMap<String, TargetConfig>,
}

/// Most targets.
pub const MAX_TARGETS: usize = 16;

/// Whether `name` can name a target.
pub fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

impl BackupsConfig {
    /// Checks the bounds; the message names the key.
    pub fn validate(&self) -> Result<(), String> {
        if self.targets.len() > MAX_TARGETS {
            return Err(format!("[backups] has more than {MAX_TARGETS} targets"));
        }
        for (name, t) in &self.targets {
            if !name_ok(name) {
                return Err(format!(
                    "[backups.targets.{name}]: a name is 1 to 64 characters of [a-z0-9_-]"
                ));
            }
            match t {
                TargetConfig::Local { path } if path.as_os_str().is_empty() => {
                    return Err(format!("[backups.targets.{name}] needs a path"));
                }
                TargetConfig::S3 {
                    bucket, endpoint, ..
                } => {
                    if bucket.is_empty() {
                        return Err(format!("[backups.targets.{name}] needs a bucket"));
                    }
                    if !endpoint.is_empty() && !endpoint.starts_with("https://") {
                        return Err(format!("[backups.targets.{name}] endpoint must be https"));
                    }
                }
                TargetConfig::Local { .. } => {}
            }
        }
        Ok(())
    }

    /// Whether any target can receive a backup.
    pub fn usable(&self) -> bool {
        self.enabled && !self.targets.is_empty()
    }
}
