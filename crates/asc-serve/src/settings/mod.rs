//! The account, organisation and administration settings services the hub's boards call
//! (docs/BUILDOUT.md sections 26 to 31): backups, usage, organisation settings, sessions,
//! data export and the sharing of AI keys.
//!
//! [`access`] is the one rule they share. [`register`] builds every hub on the server's
//! database and installs them in [`SettingsSlot`]; `build_grpc` serves the services from there.

pub mod access;
#[cfg(test)]
pub(crate) mod testkit;

use std::sync::{Arc, OnceLock};

use tokio::task::JoinHandle;

use crate::ai::audit::{Audit, PoolAudit};
use crate::backups::{BackupsHub, RealEngine};
use crate::config::ServeConfig;
use crate::export::ExportHub;
use crate::org::OrgHub;
use crate::registry::{Extensions, Registry};
use crate::sessions::SessionsHub;
use crate::usage::UsageHub;

/// Every hub of the settings services.
pub struct SettingsHubs {
    pub backups: Arc<BackupsHub>,
    pub usage: Arc<UsageHub>,
    pub org: Arc<OrgHub>,
    pub sessions: Arc<SessionsHub>,
    pub export: Arc<ExportHub>,
}

/// The place the hubs are registered, once.
#[derive(Default)]
pub struct SettingsSlot(OnceLock<Arc<SettingsHubs>>);

impl SettingsSlot {
    pub fn set(&self, hubs: Arc<SettingsHubs>) -> bool {
        self.0.set(hubs).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<SettingsHubs>> {
        self.0.get()
    }
}

/// Builds the hubs and installs them. An invalid configuration or a second registration is an
/// error the server refuses to start with.
pub fn register(
    ext: &Extensions,
    registry: &Arc<Registry>,
    cfg: &ServeConfig,
) -> Result<Arc<SettingsHubs>, String> {
    cfg.backups.validate()?;
    cfg.usage.validate()?;
    let pool = registry.pool().clone();
    let audit: Arc<dyn Audit> = Arc::new(PoolAudit::new(pool.clone()));
    let backups = BackupsHub::new(
        pool.clone(),
        cfg.backups.clone(),
        Arc::new(RealEngine {
            cfg: Arc::new(cfg.clone()),
        }),
        crate::backups::state_dir(cfg),
        audit.clone(),
    );
    let hubs = Arc::new(SettingsHubs {
        backups,
        usage: UsageHub::new(pool.clone(), cfg.usage.clone(), audit.clone()),
        org: OrgHub::new(pool.clone(), audit.clone()),
        sessions: SessionsHub::new(pool.clone(), audit.clone()),
        export: ExportHub::new(pool, crate::workdir::default_dir(cfg, "exports"), audit),
    });
    if !ext.settings.set(hubs.clone()) {
        return Err("the settings services were already registered".into());
    }
    Ok(hubs)
}

impl SettingsHubs {
    /// What `Version.features` adds.
    pub fn features(&self) -> Vec<String> {
        [
            crate::backups::FEATURE,
            crate::usage::FEATURE,
            crate::org::FEATURE,
            crate::sessions::FEATURE,
            crate::export::FEATURE,
        ]
        .map(str::to_owned)
        .to_vec()
    }

    /// Starts the background workers (the backup scheduler).
    pub fn start(&self) -> Vec<JoinHandle<()>> {
        let usage = self.usage.clone();
        let sweep = tokio::spawn(async move {
            loop {
                usage.sweep().await;
                tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
            }
        });
        vec![
            self.backups.spawn_scheduler(),
            self.export.spawn_sweeper(),
            sweep,
        ]
    }

    /// Tells the workers to stop.
    pub fn stop(&self, tasks: &[JoinHandle<()>]) {
        self.backups.stop();
        for t in tasks {
            t.abort();
        }
    }
}
