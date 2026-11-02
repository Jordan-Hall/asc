//! What one caller may see of one repository: the compiled path rules and the viewer, ready for
//! the one predicate every read path applies (SPEC §15.5.1).

use std::sync::Arc;

use asc_core::pathacl::{Level, PathAcl, Viewer};
use tonic::Status;

use super::PathRulesSlot;

/// The path rules applied to one caller on one repository. A caller with no restriction gets
/// no `Scope` at all, so the unrestricted path costs nothing.
#[derive(Debug)]
pub struct Scope {
    acl: Arc<PathAcl>,
    viewer: Viewer,
}

/// The refusal of a restricted path: `PERMISSION_DENIED` with the `PATH_POLICY` marker.
pub fn path_policy(path: &[u8]) -> Status {
    Status::permission_denied(format!(
        "PATH_POLICY: {}: restricted",
        String::from_utf8_lossy(path)
    ))
}

impl Scope {
    /// The scope of `principal_id` on `repo` with `repo_level`, or `None` when nothing is
    /// restricted for it: no rules service, no rules on the repository, the host service's own
    /// work, or rules that leave every path at `repo_level` (so a read scope exists only when a
    /// path is hidden, and a write scope also when a path is read-only).
    pub async fn of(
        slot: &PathRulesSlot,
        repo: &str,
        principal_id: &str,
        repo_level: Level,
    ) -> Result<Option<Arc<Self>>, Status> {
        if crate::authz::is_host_service(principal_id) {
            return Ok(None);
        }
        Self::of_who(slot, repo, crate::policy::who_of(principal_id), repo_level).await
    }

    /// The scope of the viewer `who` on `repo` with `repo_level` (see [`Self::of`]).
    pub async fn of_who(
        slot: &PathRulesSlot,
        repo: &str,
        who: asc_core::policy::Who,
        repo_level: Level,
    ) -> Result<Option<Arc<Self>>, Status> {
        let Some(hub) = slot.get() else {
            return Ok(None);
        };
        let acl = hub.acl(repo).await.map_err(|e| e.status())?;
        if acl.is_empty() {
            return Ok(None);
        }
        let viewer = hub
            .viewer(repo, who, repo_level)
            .await
            .map_err(|e| e.status())?;
        if !acl.limits(&viewer) {
            return Ok(None);
        }
        Ok(Some(Arc::new(Self { acl, viewer })))
    }

    /// The scope of a call that passed `level` on its repository. A call the host service makes
    /// for a person (it forwards their claim) is scoped to that person at the claim's role; one
    /// it makes without a claim is its own work and unrestricted.
    pub async fn for_call(
        slot: &PathRulesSlot,
        ctx: &asc_rpc::server::CallCtx,
        level: Level,
    ) -> Result<Option<Arc<Self>>, Status> {
        if crate::authz::is_host_service(&ctx.principal.id) {
            let Some(claim) = ctx.admin.forwarded_claim() else {
                return Ok(None);
            };
            let role = match claim.role {
                asc_rpc::forwarded::Role::Admin => Level::Admin,
                asc_rpc::forwarded::Role::Write => Level::Write,
                _ => Level::Read,
            };
            let who = asc_core::policy::Who::new(asc_core::policy::Kind::User, claim.login());
            return Self::of_who(slot, ctx.repo.as_str(), who, role).await;
        }
        Self::of(slot, ctx.repo.as_str(), &ctx.principal.id, level).await
    }

    /// The read scope of a call that passed read on its repository.
    pub async fn read(
        slot: &PathRulesSlot,
        ctx: &asc_rpc::server::CallCtx,
    ) -> Result<Option<Arc<Self>>, Status> {
        Self::for_call(slot, ctx, Level::Read).await
    }

    /// A scope over `acl` for `viewer`, for tests and callers that hold both.
    pub fn new(acl: Arc<PathAcl>, viewer: Viewer) -> Self {
        Self { acl, viewer }
    }

    pub fn can_read(&self, path: &[u8]) -> bool {
        self.acl.can_read(&self.viewer, path)
    }

    pub fn can_write(&self, path: &[u8]) -> bool {
        self.acl.can_write(&self.viewer, path)
    }

    pub fn level(&self, path: &[u8]) -> Level {
        self.acl.level(&self.viewer, path)
    }

    /// Whether the directory may be listed at all (read on it, or a readable rule below it).
    pub fn can_list(&self, dir: &[u8]) -> bool {
        self.acl.can_list(&self.viewer, dir)
    }

    /// How an entry at `path` shows in a listing of its parent `dir`: `Some(false)` with its
    /// contents, `Some(true)` by name only and flagged restricted, `None` not at all (the
    /// parent itself is restricted, so even the name is withheld).
    pub fn listing(&self, dir: &[u8], path: &[u8], is_dir: bool) -> Option<bool> {
        if self.acl.entry_visible(&self.viewer, path, is_dir) {
            Some(false)
        } else if self.can_read(dir) {
            Some(true)
        } else {
            None
        }
    }

    /// The directory a history entry names for an unreadable `path`: its shallowest unreadable
    /// ancestor (or itself), whose parent is readable, so the name is one a listing of that
    /// parent already shows as restricted. `None` when `path` is readable.
    pub fn restricted_root<'p>(&self, path: &'p [u8]) -> Option<&'p [u8]> {
        if self.can_read(path) {
            return None;
        }
        let ends = path
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'/')
            .map(|(i, _)| i)
            .chain([path.len()]);
        for end in ends {
            if !self.can_read(&path[..end]) {
                return Some(&path[..end]);
            }
        }
        Some(path)
    }

    /// `Ok` when filtering by the pathspecs `items` reveals nothing under a restricted
    /// directory: a literal path must be readable, a pattern (or magic) must lie in a directory
    /// whose whole subtree is readable, so no hidden name can be probed for.
    pub fn require_spec(&self, items: &[String]) -> Result<(), Status> {
        for item in items {
            let p = item.trim_matches('/').as_bytes();
            match p.iter().position(|b| b"*?[\\:!".contains(b)) {
                None => self.require_read(p)?,
                Some(i) => {
                    let dir = parent(&p[..i]);
                    let dir = if p[..i].ends_with(b"/") {
                        &p[..i - 1]
                    } else {
                        dir
                    };
                    if !self.acl.subtree_readable(&self.viewer, dir) {
                        return Err(path_policy(p));
                    }
                }
            }
        }
        Ok(())
    }

    /// `Ok` when `path` may be read, else the `PATH_POLICY` refusal.
    pub fn require_read(&self, path: &[u8]) -> Result<(), Status> {
        if self.can_read(path) {
            Ok(())
        } else {
            Err(path_policy(path))
        }
    }
}

/// What a restricted caller sees of one changeset's changed paths.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Redaction {
    /// The restricted directories some change lies under, sorted and distinct.
    pub dirs: Vec<Vec<u8>>,
    /// Whether every change was under a restricted directory (the message is withheld).
    pub whole: bool,
}

impl Scope {
    /// The redaction of a changeset touching `paths` (each change's paths, old and new).
    pub fn redaction<'a>(&self, paths: impl IntoIterator<Item = &'a [u8]>) -> Redaction {
        let mut dirs = std::collections::BTreeSet::new();
        let (mut any, mut shown) = (false, false);
        for p in paths {
            any = true;
            match self.restricted_root(p) {
                Some(d) => {
                    dirs.insert(d.to_vec());
                }
                None => shown = true,
            }
        }
        Redaction {
            whole: any && !shown,
            dirs: dirs.into_iter().collect(),
        }
    }
}

impl Scope {
    /// The redaction of the changeset `cs`: its changed paths and the paths copied from.
    pub fn redaction_of(&self, cs: &asc_core::object::Changeset) -> Redaction {
        use asc_core::object::Change;
        self.redaction(cs.changes.as_slice().iter().flat_map(|(path, change)| {
            let from = match change {
                Change::Write {
                    copy_from: Some(f), ..
                } => Some(f.as_bytes()),
                _ => None,
            };
            std::iter::once(path.as_bytes()).chain(from)
        }))
    }
}

/// Applies `scope` to a history entry of `cs`: an entry with a restricted change is marked and
/// names the restricted directories, loses its whole-changeset counts, and when every change is
/// restricted shows [`RESTRICTED_MESSAGE`] and no change metadata.
pub fn redact_entry(
    scope: Option<&Scope>,
    cs: &asc_core::object::Changeset,
    e: &mut asc_rpc::server::wire::LogEntry,
) {
    let Some(scope) = scope else { return };
    let red = scope.redaction_of(cs);
    if red.dirs.is_empty() {
        return;
    }
    e.redacted = true;
    e.stats = None;
    if red.whole {
        e.message = bytes::Bytes::from_static(RESTRICTED_MESSAGE);
        e.meta = bytes::Bytes::new();
    }
    e.restricted_dirs = red.dirs.into_iter().map(bytes::Bytes::from).collect();
}

/// The answer to git over HTTP or SSH for a caller with a restricted path: git cannot leave a
/// directory out of a clone or a push, so the caller uses `asc`, whose views apply the rules.
pub const GIT_REFUSED: &str = "PATH_POLICY: path rules restrict part of this repository for you, and git cannot leave part of a repository out; use asc clone
";

/// The message a changeset shows when every one of its changes is restricted.
pub const RESTRICTED_MESSAGE: &[u8] = b"(restricted change)";

/// The parent directory of `path` (empty for a top-level name).
pub fn parent(path: &[u8]) -> &[u8] {
    path.iter()
        .rposition(|&b| b == b'/')
        .map_or(&path[..0], |i| &path[..i])
}

#[cfg(test)]
mod tests {
    use asc_core::pathacl::{Level, PathAcl, Rule, Subject, Viewer};
    use asc_core::policy::{Kind, Who};
    use tonic::Code;

    use super::*;

    fn scope() -> Scope {
        let rule = |dir: &str, level| Rule {
            dir: dir.into(),
            subject: Subject::parse("*").unwrap(),
            level,
        };
        let acl = PathAcl::new(&[
            rule("secret", Level::None),
            rule("secret/docs", Level::Read),
        ])
        .unwrap();
        Scope::new(
            Arc::new(acl),
            Viewer {
                who: Who::new(Kind::User, "bob"),
                teams: Default::default(),
                repo_level: Level::Write,
            },
        )
    }

    #[test]
    fn the_restricted_root_is_the_shallowest_hidden_directory() {
        let s = scope();
        assert_eq!(s.restricted_root(b"secret/a/b.rs"), Some(&b"secret"[..]));
        assert_eq!(s.restricted_root(b"secret"), Some(&b"secret"[..]));
        assert_eq!(s.restricted_root(b"secret/docs/x.md"), None);
        assert_eq!(s.restricted_root(b"src/main.rs"), None);
    }

    #[test]
    fn pathspecs_may_not_probe_hidden_names() {
        let s = scope();
        let spec =
            |v: &[&str]| s.require_spec(&v.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        spec(&[
            "src/main.rs",
            "secret/docs/a.md",
            "src/*.rs",
            "secret/docs/**",
        ])
        .unwrap();
        for bad in [
            &["secret/key.pem"][..],
            &["*"],
            &["secret/*"],
            &["**/*.pem"],
            &[":!src"],
        ] {
            let e = spec(bad).unwrap_err();
            assert_eq!(e.code(), Code::PermissionDenied, "{bad:?}");
            assert!(e.message().starts_with("PATH_POLICY: "), "{bad:?}");
        }
    }

    #[test]
    fn a_redaction_names_hidden_directories_and_is_whole_only_when_nothing_shows() {
        let s = scope();
        let r = s.redaction([&b"secret/a"[..], b"secret/b/c", b"src/x"]);
        assert_eq!(r.dirs, vec![b"secret".to_vec()]);
        assert!(!r.whole);
        assert!(s.redaction([&b"secret/a"[..]]).whole);
        assert_eq!(s.redaction([&b"src/x"[..]]), Redaction::default());
        assert!(!s.redaction(std::iter::empty()).whole);
    }

    #[test]
    fn the_parent_of_a_top_level_name_is_the_root() {
        assert_eq!(parent(b"a/b/c"), b"a/b");
        assert_eq!(parent(b"a"), b"");
    }
}
