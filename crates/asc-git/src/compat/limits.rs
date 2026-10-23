//! `[limits.git]` (SPEC §25.5): the types and defaults of the git protocol layer's bounds.
//!
//! Every number the layer enforces is named here or in a sibling module; nothing is a bare
//! literal at a use site. `asc-serve` parses these from `asc-serve.toml` and hands the result to
//! [`ServerGitConfig`](super::ServerGitConfig) and [`GitView`](super::GitView).

use std::time::Duration;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Pack size a push may carry (`receive.maxInputSize`).
pub const DEFAULT_MAX_PUSH_BYTES: u64 = 2 * GIB;
/// Objects a push may carry, checked from the quarantine before import.
pub const DEFAULT_MAX_PUSH_OBJECTS: u64 = 2_000_000;
/// One object's size, checked from the quarantine before import.
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 512 * MIB;
/// Wall clock for one request; the child's process tree is killed at the deadline.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Time with no bytes either way before the child is killed.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Memory of each child process (Job Object or `RLIMIT_AS`).
pub const DEFAULT_CHILD_MEMORY: u64 = 2 * GIB;
/// Processes one child tree may hold at once (`git`, `pack-objects`, `index-pack`, a hook, the
/// Windows launcher), with generous room.
pub const DEFAULT_CHILD_PROCESSES: u32 = 64;
/// `pack.threads` passed to every child.
pub const DEFAULT_PACK_THREADS: u32 = 2;
/// Free disk below which a push is refused with `503`.
pub const DEFAULT_MIN_FREE_DISK: u64 = 10 * GIB;
/// A fetch request body (negotiation), after decompression.
pub const DEFAULT_FETCH_BODY_BYTES: u64 = 16 * MIB;
/// Concurrent fetches per principal.
pub const DEFAULT_FETCH_PER_PRINCIPAL: u32 = 4;
/// Concurrent fetches per replica.
pub const DEFAULT_FETCH_PER_REPLICA: u32 = 16;
/// Concurrent pushes per principal.
pub const DEFAULT_PUSH_PER_PRINCIPAL: u32 = 2;
/// Concurrent pushes per replica.
pub const DEFAULT_PUSH_PER_REPLICA: u32 = 8;
/// View builds running at once on a replica.
pub const DEFAULT_VIEW_BUILDS_PER_REPLICA: u32 = 2;
/// A view whose exported closure is estimated under this many objects builds inline; a larger
/// one builds in a background job and the request gets `503` with `Retry-After`.
pub const INLINE_BUILD_OBJECTS: u64 = 50_000;

/// Hard ceilings: a configured value above one of these is refused, so a typo cannot turn a
/// bound into "unbounded".
pub const CEILING_PUSH_BYTES: u64 = 64 * GIB;
pub const CEILING_PUSH_OBJECTS: u64 = 100_000_000;
pub const CEILING_OBJECT_BYTES: u64 = 16 * GIB;
pub const CEILING_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
pub const CEILING_CHILD_MEMORY: u64 = 256 * GIB;
pub const CEILING_CHILD_PROCESSES: u32 = 4096;
pub const CEILING_PACK_THREADS: u32 = 64;
pub const CEILING_FETCH_BODY_BYTES: u64 = GIB;
pub const CEILING_CONCURRENCY: u32 = 100_000;

/// A limit that is zero or above its ceiling.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("limits.git.{name} {problem}")]
pub struct LimitError {
    pub name: &'static str,
    pub problem: &'static str,
}

/// The `[limits.git]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLimits {
    pub max_push_bytes: u64,
    pub max_push_objects: u64,
    pub max_object_bytes: u64,
    pub request_timeout: Duration,
    pub idle_timeout: Duration,
    pub child_memory: u64,
    pub child_processes: u32,
    pub pack_threads: u32,
    pub min_free_disk: u64,
    pub fetch_body_bytes: u64,
    pub fetch_per_principal: u32,
    pub fetch_per_replica: u32,
    pub push_per_principal: u32,
    pub push_per_replica: u32,
    pub view_builds_per_replica: u32,
}

impl Default for GitLimits {
    fn default() -> Self {
        Self {
            max_push_bytes: DEFAULT_MAX_PUSH_BYTES,
            max_push_objects: DEFAULT_MAX_PUSH_OBJECTS,
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            child_memory: DEFAULT_CHILD_MEMORY,
            child_processes: DEFAULT_CHILD_PROCESSES,
            pack_threads: DEFAULT_PACK_THREADS,
            min_free_disk: DEFAULT_MIN_FREE_DISK,
            fetch_body_bytes: DEFAULT_FETCH_BODY_BYTES,
            fetch_per_principal: DEFAULT_FETCH_PER_PRINCIPAL,
            fetch_per_replica: DEFAULT_FETCH_PER_REPLICA,
            push_per_principal: DEFAULT_PUSH_PER_PRINCIPAL,
            push_per_replica: DEFAULT_PUSH_PER_REPLICA,
            view_builds_per_replica: DEFAULT_VIEW_BUILDS_PER_REPLICA,
        }
    }
}

fn check(name: &'static str, value: u64, ceiling: u64) -> Result<(), LimitError> {
    let problem = if value == 0 {
        "must be greater than zero"
    } else if value > ceiling {
        "is above its ceiling"
    } else {
        return Ok(());
    };
    Err(LimitError { name, problem })
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl GitLimits {
    /// Every value must be positive (`min_free_disk` may be zero: no disk gate) and at most its
    /// ceiling.
    pub fn validate(&self) -> Result<(), LimitError> {
        let ceiling_ms = millis(CEILING_TIMEOUT);
        check("max_push_bytes", self.max_push_bytes, CEILING_PUSH_BYTES)?;
        check(
            "max_push_objects",
            self.max_push_objects,
            CEILING_PUSH_OBJECTS,
        )?;
        check(
            "max_object_bytes",
            self.max_object_bytes,
            CEILING_OBJECT_BYTES,
        )?;
        check("request_timeout", millis(self.request_timeout), ceiling_ms)?;
        check("idle_timeout", millis(self.idle_timeout), ceiling_ms)?;
        check("child_memory", self.child_memory, CEILING_CHILD_MEMORY)?;
        check(
            "child_processes",
            self.child_processes.into(),
            CEILING_CHILD_PROCESSES.into(),
        )?;
        check(
            "pack_threads",
            self.pack_threads.into(),
            CEILING_PACK_THREADS.into(),
        )?;
        check(
            "fetch_body_bytes",
            self.fetch_body_bytes,
            CEILING_FETCH_BODY_BYTES,
        )?;
        let c = u64::from(CEILING_CONCURRENCY);
        check("fetch_per_principal", self.fetch_per_principal.into(), c)?;
        check("fetch_per_replica", self.fetch_per_replica.into(), c)?;
        check("push_per_principal", self.push_per_principal.into(), c)?;
        check("push_per_replica", self.push_per_replica.into(), c)?;
        check(
            "view_builds_per_replica",
            self.view_builds_per_replica.into(),
            c,
        )?;
        if self.max_object_bytes > self.max_push_bytes {
            return Err(LimitError {
                name: "max_object_bytes",
                problem: "is above max_push_bytes",
            });
        }
        if self.idle_timeout > self.request_timeout {
            return Err(LimitError {
                name: "idle_timeout",
                problem: "is above request_timeout",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_spec_values_and_valid() {
        let l = GitLimits::default();
        assert_eq!(l.max_push_bytes, 2 * GIB);
        assert_eq!(l.max_push_objects, 2_000_000);
        assert_eq!(l.max_object_bytes, 512 * MIB);
        assert_eq!(l.request_timeout, Duration::from_secs(900));
        assert_eq!(l.idle_timeout, Duration::from_secs(60));
        assert_eq!((l.fetch_per_principal, l.fetch_per_replica), (4, 16));
        assert_eq!((l.push_per_principal, l.push_per_replica), (2, 8));
        assert_eq!(l.min_free_disk, 10 * GIB);
        assert_eq!(l.fetch_body_bytes, 16 * MIB);
        assert_eq!(l.pack_threads, 2);
        assert_eq!(l.validate(), Ok(()));
    }

    #[test]
    fn zero_and_oversized_values_are_refused() {
        let zero = GitLimits {
            max_push_bytes: 0,
            ..GitLimits::default()
        };
        assert_eq!(zero.validate().unwrap_err().name, "max_push_bytes");
        let big = GitLimits {
            child_processes: CEILING_CHILD_PROCESSES + 1,
            ..GitLimits::default()
        };
        assert_eq!(big.validate().unwrap_err().name, "child_processes");
        let order = GitLimits {
            idle_timeout: Duration::from_secs(3600),
            ..GitLimits::default()
        };
        assert_eq!(order.validate().unwrap_err().name, "idle_timeout");
    }
}
