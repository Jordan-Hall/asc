//! The hook between the landing gate and the dependency scanner.
//!
//! The gate (`policy::gate`) knows the policy; the scanner knows advisories. This trait is all they
//! share: the gate asks whether an update adds a dependency the policy forbids, and the scanner answers
//! with the lines of the refusal, or none. When the scanner is not running (`[deps] enabled = false`)
//! the slot is empty and a policy that blocks on advisories refuses every update that touches a
//! dependency file, because it cannot check it.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

use asc_core::id::ChangesetId;
use asc_core::policy::DepsPolicy;
use async_trait::async_trait;
use tonic::Status;

use super::parse::Format;
use crate::registry::RepoEntry;

/// Most lines of a refusal.
pub const MAX_LINES: usize = 20;

/// What the gate asks.
pub struct CheckRequest<'a> {
    pub entry: &'a RepoEntry,
    /// The changeset the ref moves to.
    pub new: ChangesetId,
    /// What the ref was (or trunk was, for a new ref); the dependencies it had are not "added".
    pub base: Option<ChangesetId>,
    /// The paths the update changes.
    pub touched: &'a BTreeSet<Vec<u8>>,
    pub policy: DepsPolicy,
}

#[async_trait]
pub trait DepsCheck: Send + Sync {
    /// The lines of a refusal (the first says why, the rest name the packages), or none when the update
    /// is fine. An error is an internal failure, which refuses the update as well.
    async fn check(&self, req: &CheckRequest<'_>) -> Result<Vec<String>, Status>;
}

/// The place the scanner puts itself, once, for the gate to find.
#[derive(Default)]
pub struct DepsGateSlot(OnceLock<Arc<dyn DepsCheck>>);

impl DepsGateSlot {
    pub fn set(&self, c: Arc<dyn DepsCheck>) -> bool {
        self.0.set(c).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<dyn DepsCheck>> {
        self.0.get()
    }
}

/// The dependency files among `touched`.
pub fn dependency_files(touched: &BTreeSet<Vec<u8>>) -> Vec<String> {
    touched
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .filter(|p| Format::of(super::model::base_of(p)).is_some())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_dependency_files_are_picked_out_of_the_touched_paths() {
        let touched: BTreeSet<Vec<u8>> = [
            "src/main.rs",
            "Cargo.lock",
            "web/package-lock.json",
            "docs/requirements-dev.txt",
            "README.md",
        ]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .chain([vec![0xff, 0xfe]])
        .collect();
        assert_eq!(
            dependency_files(&touched),
            [
                "Cargo.lock",
                "docs/requirements-dev.txt",
                "web/package-lock.json"
            ]
        );
    }

    #[test]
    fn the_slot_is_set_once() {
        struct Never;
        #[async_trait]
        impl DepsCheck for Never {
            async fn check(&self, _: &CheckRequest<'_>) -> Result<Vec<String>, Status> {
                Ok(Vec::new())
            }
        }
        let slot = DepsGateSlot::default();
        assert!(slot.get().is_none());
        assert!(slot.set(Arc::new(Never)));
        assert!(!slot.set(Arc::new(Never)));
        assert!(slot.get().is_some());
    }
}
