//! Git protocol compatibility (SPEC §25, decision 18), behind the Cargo feature `git-compat`.
//!
//! `asc serve` answers plain `git clone`, `fetch` and `push` by serving a derived git view
//! through real `git upload-pack` and `git receive-pack` in `--stateless-rpc` mode. This module
//! holds the parts that live in `asc-git`:
//!
//! - [`ServerGit`]: the only way the server layer starts a git child. Empty environment, a
//!   fixed allow-list, every behaviour passed as `-c`, no repository or user config, no hooks
//!   but asc's own, supervised limits ([`server_git`]).
//! - [`GitView`]: the derived bare repository per repo and its build state machine ([`view`]).
//! - A bounded pkt-line reader and writer for the advertisement prefix, the `0000` probe and the
//!   v2 capability check ([`pktline`]).
//! - The `[limits.git]` types ([`limits`]).
//! - The upload-pack rules that need no HTTP framework: the repository path, the media types,
//!   the bounded fetch body and the `0000` probe ([`upload`]).
//!
//! Nothing outside `asc-git` names this module except the feature-gated wiring of `asc-serve` and
//! `asc`; `asc-engine`, `asc-rpc`, `asc-store` and `asc-core` never see it.

pub mod limits;
pub mod pktline;
pub mod server_git;
pub mod upload;
pub mod view;

pub use limits::{GitLimits, INLINE_BUILD_OBJECTS, LimitError};
pub use pktline::{GitProtocol, Packet, ProtocolVersion, Service};
pub use server_git::{
    Activity, ChildExit, FsckMode, KillReason, MIGRATION_RELAXED, PackRequest, PackService,
    RefUpdate, ServerChild, ServerGit, ServerGitConfig, ServerGitError, valid_ref_name,
};
pub use upload::{
    BodyEncoding, BodyError, Endpoint, HIDDEN_REF_PREFIXES, PathError, RepoPath, UploadBody,
    is_hidden_ref,
};
pub use view::{
    BuildSlots, BuildTicket, GitView, Readiness, RefSource, SourceRef, SyncReport, ViewError,
    ViewKind, ViewState,
};
