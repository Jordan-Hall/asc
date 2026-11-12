//! The surface the daemon drives on either Windows backend, so one host serves both.

use std::time::SystemTime;

use asc_core::path::RepoPath;
use asc_wc::intent::Replayed;
use asc_wc::times::Switched;

use super::Reconciled;
use super::engine::{Report, Request, Store};
use super::idle;
use crate::core::{Provider, Result};

/// A mounted Windows provider: ProjFS or Cloud Files.
pub trait WinProvider: Provider {
    /// Which backend this is, for logs and `asc doctor`.
    fn backend(&self) -> &'static str;
    /// Gives the provider the database that holds the switch intent.
    fn set_store(&self, store: Store);
    /// Runs the switch of `req`; `times.time` is the time of every rewritten placeholder.
    fn switch(&self, req: &Request<'_>, times: Switched) -> Result<Report>;
    /// Re-runs the stored intent with the SAFE rules. `None` when there is none.
    fn resume(&self, time: i64) -> Result<Option<Replayed>>;
    /// Journals what changed while no provider was running.
    fn reconcile(&self) -> Result<Reconciled>;
    /// Bytes of tracked content held on disk.
    fn hydrated_bytes(&self) -> Result<u64>;
    /// Returns clean files idle for `policy.idle` to placeholders, within `policy.budget`.
    fn dehydrate_idle(&self, policy: &idle::Policy, now: SystemTime) -> Result<idle::Report>;
    /// Tracked paths the user deleted.
    fn tombstones(&self) -> Result<Vec<RepoPath>>;
}

macro_rules! forward {
    ($ty:ty, $name:literal) => {
        impl WinProvider for $ty {
            fn backend(&self) -> &'static str {
                $name
            }
            fn set_store(&self, store: Store) {
                <$ty>::set_store(self, store)
            }
            fn switch(&self, req: &Request<'_>, times: Switched) -> Result<Report> {
                <$ty>::switch(self, req, times)
            }
            fn resume(&self, time: i64) -> Result<Option<Replayed>> {
                <$ty>::resume(self, time)
            }
            fn reconcile(&self) -> Result<Reconciled> {
                <$ty>::reconcile(self)
            }
            fn hydrated_bytes(&self) -> Result<u64> {
                <$ty>::hydrated_bytes(self)
            }
            fn dehydrate_idle(
                &self,
                policy: &idle::Policy,
                now: SystemTime,
            ) -> Result<idle::Report> {
                <$ty>::dehydrate_idle(self, policy, now)
            }
            fn tombstones(&self) -> Result<Vec<RepoPath>> {
                <$ty>::tombstones(self)
            }
        }
    };
}

#[cfg(feature = "projfs")]
forward!(crate::projfs::ProjfsProvider, "projfs");
#[cfg(feature = "cloudfiles")]
forward!(crate::cloudfiles::CloudFilesProvider, "cloudfiles");
