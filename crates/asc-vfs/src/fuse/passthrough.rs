//! FUSE passthrough (SPEC §10.3): a clean file opened read-only is served by the kernel straight
//! from the local content cache, with no copy and no extra disk. The kernel needs one backing id
//! per inode, shared by every open handle of it, so the ids are reference counted here. Without
//! kernel support (before 6.9) or without the privilege to register a backing file, opens fall
//! back to reads through the daemon.

use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use fuser::{BackingId, ReplyOpen};

/// Backing ids by inode, with the number of handles using each.
#[derive(Default)]
pub struct Passthrough {
    /// The kernel accepted `FUSE_PASSTHROUGH` at init.
    negotiated: AtomicBool,
    /// Set once a registration was refused, so later opens do not retry a doomed call.
    refused: AtomicBool,
    ids: Mutex<HashMap<u64, (Arc<BackingId>, u32)>>,
    registered: std::sync::atomic::AtomicU64,
}

impl Passthrough {
    fn ids(&self) -> MutexGuard<'_, HashMap<u64, (Arc<BackingId>, u32)>> {
        self.ids.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records the result of the init negotiation.
    pub fn set_negotiated(&self, on: bool) {
        self.negotiated.store(on, Ordering::SeqCst);
    }

    /// Whether the next open should try passthrough.
    pub fn usable(&self) -> bool {
        self.negotiated.load(Ordering::SeqCst) && !self.refused.load(Ordering::SeqCst)
    }

    /// How many registrations succeeded (for tests and statistics).
    pub fn registered(&self) -> u64 {
        self.registered.load(Ordering::SeqCst)
    }

    /// The backing id of `ino`, registering `file` if the inode has none. `None` when the kernel
    /// or the process cannot do passthrough; the caller then reads through.
    pub fn acquire(&self, ino: u64, reply: &ReplyOpen, file: &File) -> Option<Arc<BackingId>> {
        if !self.usable() {
            return None;
        }
        let mut ids = self.ids();
        if let Some((id, n)) = ids.get_mut(&ino) {
            *n += 1;
            return Some(id.clone());
        }
        match reply.open_backing(file) {
            Ok(id) => {
                let id = Arc::new(id);
                ids.insert(ino, (id.clone(), 1));
                self.registered.fetch_add(1, Ordering::SeqCst);
                Some(id)
            }
            Err(_) => {
                self.refused.store(true, Ordering::SeqCst);
                None
            }
        }
    }

    /// The content of `ino` changed: later opens register the new file. Open handles keep the
    /// file they were opened on.
    pub fn invalidate(&self, ino: u64) {
        self.ids().remove(&ino);
    }

    /// A passthrough handle of `ino` closed. The last one drops the backing id. A handle opened
    /// before an [`Self::invalidate`] holds an id that is no longer the inode's current one and
    /// leaves the current count alone.
    pub fn release(&self, ino: u64, id: &Arc<BackingId>) {
        let mut ids = self.ids();
        if let Some((cur, n)) = ids.get_mut(&ino)
            && Arc::ptr_eq(cur, id)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                ids.remove(&ino);
            }
        }
    }
}
