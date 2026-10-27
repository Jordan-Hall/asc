//! Server ref policy (SPEC §21.5.1, PLAN B08a): which pushed refs are shared, which become the
//! pusher's user ref `refs/users/<owner-id>/heads/<n>`, which are refused with `REF_NOT_SHARED`,
//! and what `ListRefs` advertises.
//!
//! The name decides the class, so a user ref and a shared ref can never collide:
//! user refs live under `refs/users/`, a namespace no client may name in an update.
//! `<owner-id>` is the numeric Forgejo user id, never the login.
//!
//! * [`RepoPolicy::classify`] is called by `UpdateRefs` step 1 and by `ImportGitPush`
//!   (a plain `git push` of a non-shared branch becomes the pusher's user ref).
//! * [`advertise`] is `ListRefs`: shared refs plus the caller's own user refs renamed to
//!   `refs/heads/<n>`. It reads only those prefixes, so it costs O(shared + own refs) however
//!   many user refs other users hold.
//!
//! Authorization of user refs (until B08b adds its own module): the owner comes from a verified
//! Forgejo identity only ([`owner_of`]), the stored name is built from it, and the `UpdateRefs`
//! transaction refuses a write whose owner segment is not the actor's and one that takes the owner
//! past [`MAX_USER_REFS`] ([`check_user_ref_writes`]). Forgejo's pre-receive is not run for user
//! refs (SPEC §21.5.1: no pre-receive). The byte quota stays B08b.

use asc_rpc::auth::Principal;
use asc_rpc::errors::RpcError;
use asc_rpc::server::{DEFAULT_REF_PAGE, MAX_REF_PAGE, MAX_STRING, wire};
use asc_store::refs::{RefEntry, Refs};
use sqlx::{AnyPool, Row};
use tonic::Status;

use crate::authz::UserIdentity;
use crate::update::{info, internal, rpc};

/// The namespace of user refs. A client never names it in an update.
pub const USER_NS: &str = "refs/users/";
const HEADS: &str = "refs/heads/";
/// The roots a shared ref can live under. `refs/pull/*` and `refs/snapshots/*` are server-managed.
pub(crate) const SHARED_ROOTS: [&str; 4] =
    ["refs/heads/", "refs/tags/", "refs/pull/", "refs/snapshots/"];
/// Most `shared_refs` patterns, and the longest one.
const MAX_PATTERNS: usize = 256;
const MAX_PATTERN_LEN: usize = 1024;

/// Most user refs one owner holds in one repository (SPEC §21.6 `[cloud] max_user_refs`).
/// Enforced in the `UpdateRefs` transaction ([`check_user_ref_writes`]).
pub const MAX_USER_REFS: i64 = 1000;

/// The numeric owner id of a principal, taken only from a provider-verified [`UserIdentity`]: the
/// authorizer encodes `<user_id>:<login>` there, and no other issuer (a token-store or SSH-key
/// principal is `pat:<user>`, `deploy:<repo>` and so on) can produce that form. A bare number is
/// not an identity. A deploy key acts for its repository, not for the user who owns it, so it has
/// no user refs; neither has the anonymous caller, an Actions task, the internal principal or any
/// other principal.
pub fn owner_of(principal: &Principal) -> Option<i64> {
    // An agent acts in the draft namespace of the person it acts for (agents/tokens.rs).
    if let Some(agent) = crate::agents::tokens::parse_principal(&principal.id) {
        return Some(agent.owner_id);
    }
    let who = UserIdentity::of(principal)?;
    who.is_user().then_some(who.user_id)
}

/// The prefix of `owner`'s user refs, `refs/users/<owner>/`.
pub fn user_prefix(owner: i64) -> String {
    format!("{USER_NS}{owner}/")
}

/// Whether `refname` lies in `owner`'s user namespace. The one ownership test: `ListRefs`,
/// the `UpdateRefs` transaction and `FindLanding` all use it.
pub fn owns_user_ref(owner: i64, refname: &str) -> bool {
    refname.starts_with(&user_prefix(owner))
}

/// The stored name of the user ref for `refs/heads/<n>`.
pub fn user_ref_name(owner: i64, heads_ref: &str) -> Option<String> {
    let branch = heads_ref.strip_prefix(HEADS)?;
    Some(format!("{USER_NS}{owner}/heads/{branch}"))
}

/// `(owner, branch)` of a stored user ref name.
pub fn parse_user_ref(name: &str) -> Option<(i64, &str)> {
    let rest = name.strip_prefix(USER_NS)?;
    let (id, rest) = rest.split_once('/')?;
    let branch = rest.strip_prefix("heads/")?;
    let n: i64 = id.parse().ok().filter(|i| *i > 0)?;
    // Canonical decimal only, so one owner has one prefix.
    (n.to_string() == id).then_some((n, branch))
}

/// How a pushed ref is treated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Class {
    /// A shared ref (or, in a branch-mode repo, any branch or tag): the full pipeline.
    Shared,
    /// `refs/pull/*`, `refs/snapshots/*`: server-managed, treated as today.
    Managed,
    /// The pusher's user ref; `stored` is its name in the `refs` table.
    User { owner: i64, stored: String },
}

impl Class {
    /// The name the `refs` table holds for a pushed `name`.
    pub fn stored<'a>(&'a self, pushed: &'a str) -> &'a str {
        match self {
            Class::User { stored, .. } => stored,
            _ => pushed,
        }
    }

    pub fn is_user(&self) -> bool {
        matches!(self, Class::User { .. })
    }
}

/// A repository's ref policy: its mode and `shared_refs`.
#[derive(Clone, Debug)]
pub struct RepoPolicy {
    branchless: bool,
    patterns: Vec<String>,
}

impl RepoPolicy {
    /// Builds the policy from the `repos` columns. `shared_refs` is a JSON array of globs
    /// (`*` matches any run of characters); a short name means `refs/heads/<name>`. `None` is the
    /// default set. `land_branches` is added to either.
    pub fn from_parts(
        branchless: bool,
        shared_refs: Option<&str>,
        default_branch: &str,
        land_branches: &str,
    ) -> Result<Self, String> {
        let mut raw: Vec<String> = match shared_refs {
            Some(json) => serde_json::from_str(json).map_err(|e| format!("shared_refs: {e}"))?,
            None => vec![
                format!("{HEADS}{default_branch}"),
                format!("{HEADS}release/*"),
                format!("{HEADS}green/*"),
                "refs/tags/*".to_owned(),
            ],
        };
        // A malformed `land_branches` only drops the extras, as `land::check_branch` does.
        let land: Vec<String> = serde_json::from_str(land_branches).unwrap_or_default();
        raw.extend(land);
        if raw.len() > MAX_PATTERNS {
            return Err("too many shared_refs patterns".into());
        }
        let mut patterns = Vec::with_capacity(raw.len());
        for p in raw {
            if p.is_empty() || p.len() > MAX_PATTERN_LEN {
                return Err("a shared_refs pattern is empty or too long".into());
            }
            let full = if p.starts_with("refs/") {
                p
            } else {
                format!("{HEADS}{p}")
            };
            // A pattern can never reach into the user namespace.
            if full.starts_with(USER_NS) {
                continue;
            }
            patterns.push(full);
        }
        Ok(Self {
            branchless,
            patterns,
        })
    }

    /// Reads the policy of repository `repo_id`.
    pub async fn load(pool: &AnyPool, repo_id: i64) -> Result<Self, Status> {
        let row = sqlx::query(
            "SELECT CAST(CASE WHEN branchless THEN 1 ELSE 0 END AS BIGINT) AS bl, shared_refs, \
             default_branch, land_branches FROM repos WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_one(pool)
        .await
        .map_err(|e| internal("repos", &e))?;
        let bl: i64 = row.try_get("bl").map_err(|e| internal("repos", &e))?;
        let shared: Option<String> = row
            .try_get("shared_refs")
            .map_err(|e| internal("repos", &e))?;
        let default_branch: String = row
            .try_get("default_branch")
            .map_err(|e| internal("repos", &e))?;
        let land: String = row
            .try_get("land_branches")
            .map_err(|e| internal("repos", &e))?;
        Self::from_parts(bl != 0, shared.as_deref(), &default_branch, &land)
            .map_err(|e| internal("repos", &e))
    }

    pub fn branchless(&self) -> bool {
        self.branchless
    }

    /// Whether `name` (a full ref name) matches `shared_refs`.
    pub fn is_shared(&self, name: &str) -> bool {
        self.patterns.iter().any(|p| glob(p, name))
    }

    /// Classifies a pushed ref for `owner` (SPEC §21.5.1). `Err` is the status the push gets:
    /// `REF_NOT_SHARED` for a ref that is neither shared nor a branch, `invalid_argument` for a
    /// client naming the user namespace, `permission_denied` for a user ref without an owner.
    pub fn classify(&self, owner: Option<i64>, name: &str) -> Result<Class, Status> {
        if name.starts_with(USER_NS) {
            return Err(Status::invalid_argument(
                "refs/users/ is server-managed; push refs/heads/<name>",
            ));
        }
        if !self.branchless {
            return Ok(Class::Shared);
        }
        if name.starts_with("refs/pull/") || name.starts_with("refs/snapshots/") {
            return Ok(Class::Managed);
        }
        if self.is_shared(name) {
            return Ok(Class::Shared);
        }
        if name.starts_with(HEADS) {
            let owner = owner.ok_or_else(|| {
                Status::permission_denied("this principal has no user id, so no user refs")
            })?;
            let stored = user_ref_name(owner, name)
                .ok_or_else(|| Status::invalid_argument("bad branch name"))?;
            return Ok(Class::User { owner, stored });
        }
        Err(rpc(RpcError::RefNotShared(info(vec![name.to_owned()]))))
    }
}

/// `*` matches any run of bytes (slashes included); everything else is literal.
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

// ------------------------------------------------------------------------------ authorization

/// Who writes user refs in a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserRefWriter {
    /// A client (any RPC or push path): the actor must own every user ref the transaction touches,
    /// whether it writes or deletes it.
    Client,
    /// The expiry worker's internal path ([`crate::update::ServerRefs::expire_user_refs`]), which
    /// no RPC reaches: it may only delete user refs whose expiry is `<= at`, never create or move
    /// one. It acts for the server, not for the owner.
    Expiry { at: i64 },
}

/// Inside the `UpdateRefs` transaction, after the updates applied: every user ref the transaction
/// touches, deletes included, must belong to the actor (`permission_denied` otherwise; a malformed
/// `refs/users/` name is `invalid_argument`), and the owner must not hold more than
/// `max` user refs (the server's `[cloud] max_user_refs`, [`MAX_USER_REFS`] by default) in the
/// repository afterwards (`USER_REFS_LIMIT`). This is the only place the cap is enforced: it is
/// counted after the writes, under the lock below, so no earlier look can be raced. A delete is
/// exempt from the cap only, so an owner over it can always shrink. Fails closed: an actor with no
/// verified owner id owns nothing.
///
/// For [`UserRefWriter::Expiry`] the ownership test is replaced by "this is a delete of a
/// well-formed user ref", and a transaction that writes any other ref is refused.
///
/// The cap is counted under a per-(repository, owner) lock: on Postgres two transactions that
/// each add a ref would otherwise both count the old total and both pass.
pub(crate) async fn check_user_ref_writes(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    repo: asc_store::refs::RepoKey,
    txn: &asc_store::refs::RefTxn,
    writer: UserRefWriter,
    max: usize,
) -> Result<(), Status> {
    let max = i64::try_from(max).unwrap_or(i64::MAX);
    let actor = owner_of(&Principal {
        id: txn.actor.id.clone(),
    });
    let mut owners: Vec<i64> = Vec::new();
    for u in &txn.updates {
        let name = std::str::from_utf8(u.name.as_bytes())
            .ok()
            .filter(|n| n.starts_with(USER_NS));
        let Some(name) = name else {
            if matches!(writer, UserRefWriter::Expiry { .. }) {
                return Err(Status::permission_denied(
                    "the expiry path deletes user refs only",
                ));
            }
            continue;
        };
        let Some((owner, _)) = parse_user_ref(name) else {
            return Err(Status::invalid_argument("malformed user ref name"));
        };
        match writer {
            UserRefWriter::Client => {
                if !actor.is_some_and(|a| owns_user_ref(a, name)) {
                    return Err(Status::permission_denied(
                        "a user ref can be changed only by its owner",
                    ));
                }
            }
            UserRefWriter::Expiry { .. } => {
                if u.new.is_some() {
                    return Err(Status::permission_denied(
                        "the expiry path never creates or moves a user ref",
                    ));
                }
            }
        }
        if u.new.is_some() && !owners.contains(&owner) {
            owners.push(owner);
        }
    }
    // A fixed order, so two transactions that lock several owners cannot deadlock.
    owners.sort_unstable();
    for owner in owners {
        if tx.backend_name() == "PostgreSQL" {
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(owner_lock_key(repo.0, owner))
                .execute(&mut **tx)
                .await
                .map_err(|e| internal("refs", &e))?;
        }
        // `refs/users/<n>/heads/` .. `refs/users/<n>/heads0` ('0' follows '/'): this owner's rows.
        let lo = format!("{USER_NS}{owner}/heads/");
        let hi = format!("{USER_NS}{owner}/heads0");
        let held: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM refs WHERE repo_id = $1 AND name >= $2 AND name < $3",
        )
        .bind(repo.0)
        .bind(lo)
        .bind(hi)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| internal("refs", &e))?;
        if held > max {
            return Err(rpc(RpcError::UserRefsLimit(info(vec![format!(
                "{USER_NS}{owner}/"
            )]))));
        }
    }
    Ok(())
}

/// The advisory-lock key of `(repo_id, owner)`: the first 64 bits of a BLAKE3 hash, so it is
/// stable across processes and builds and does not collide with the plain `repo_id` keys other
/// locks use. SQLite needs none: `BEGIN IMMEDIATE` already serialises writers.
fn owner_lock_key(repo_id: i64, owner: i64) -> i64 {
    let mut h = blake3::Hasher::new();
    h.update(b"asc-serve user-ref cap");
    h.update(&repo_id.to_le_bytes());
    h.update(&owner.to_le_bytes());
    let digest = h.finalize();
    let bytes = digest.as_bytes();
    i64::from_le_bytes(std::array::from_fn(|i| bytes[i]))
}

/// Before the expiry path deletes anything: each named ref must still be expired at `at`
/// (`ABORTED` when a push refreshed it or removed its expiry since the scan).
pub(crate) async fn require_expired(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    repo: asc_store::refs::RepoKey,
    txn: &asc_store::refs::RefTxn,
    at: i64,
) -> Result<(), Status> {
    for u in &txn.updates {
        let name = String::from_utf8_lossy(u.name.as_bytes()).into_owned();
        let expires: Option<i64> =
            sqlx::query_scalar("SELECT expires_at FROM refs WHERE repo_id = $1 AND name = $2")
                .bind(repo.0)
                .bind(name)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|e| internal("refs", &e))?
                .flatten();
        if expires.is_none_or(|x| x > at) {
            return Err(Status::aborted("the ref is no longer expired"));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------ advertisement

fn wire_ref(name: String, e: &RefEntry) -> wire::Ref {
    wire::Ref {
        name,
        target: Some(wire::RefTarget::from(&e.target)),
        version: e.version,
        expires_at: e.expires_at.unwrap_or(0),
    }
}

fn text(e: &RefEntry) -> String {
    String::from_utf8_lossy(e.name.as_bytes()).into_owned()
}

async fn list<R: Refs + ?Sized>(
    refs: &R,
    repo: asc_store::refs::RepoKey,
    prefix: &str,
) -> Result<Vec<RefEntry>, Status> {
    refs.list(repo, prefix)
        .await
        .map_err(|e| internal("refs", &e))
}

/// `ListRefs` of a branchless repository: the shared refs plus `owner`'s user refs renamed to
/// `refs/heads/<n>`, filtered by `req.prefix`, ordered by name, paged by `req.after`/`req.limit`.
/// Reads only the shared roots and the owner's prefix. An explicit `refs/users/` prefix lists
/// the caller's own user refs under their stored names; another user's is refused (B08b owns
/// the cross-user listing and its permission check).
pub async fn advertise<R: Refs + ?Sized>(
    refs: &R,
    repo: asc_store::refs::RepoKey,
    policy: &RepoPolicy,
    owner: Option<i64>,
    req: &wire::ListRefsRequest,
) -> Result<wire::ListRefsResponse, Status> {
    if !req.contains.is_empty() {
        return Err(Status::unimplemented(
            "ListRefs contains needs the full server",
        ));
    }
    if req.prefix.len() > MAX_STRING || req.after.len() > MAX_STRING {
        return Err(Status::invalid_argument("prefix or page token is too long"));
    }
    let limit = match req.limit as usize {
        0 => DEFAULT_REF_PAGE,
        n => n.min(MAX_REF_PAGE),
    };
    let prefix = req.prefix.as_str();
    let mut found: Vec<wire::Ref> = Vec::new();

    if prefix.starts_with(USER_NS) {
        let Some(owner) = owner else {
            return Err(Status::permission_denied("no user refs for this principal"));
        };
        let own = user_prefix(owner);
        if !(own.starts_with(prefix) || prefix.starts_with(&own)) {
            return Err(Status::permission_denied(
                "another user's refs are not listed to you",
            ));
        }
        let from = if prefix.len() > own.len() {
            prefix
        } else {
            &own
        };
        for e in list(refs, repo, from).await? {
            let name = text(&e);
            found.push(wire_ref(name, &e));
        }
    } else {
        // Shared refs: each root that overlaps the prefix, narrowed to the prefix.
        for root in SHARED_ROOTS {
            let from = if prefix.starts_with(root) {
                prefix
            } else if root.starts_with(prefix) {
                root
            } else {
                continue;
            };
            for e in list(refs, repo, from).await? {
                let name = text(&e);
                // A branch row in a branchless repo is a shared (or legacy shared) branch.
                found.push(wire_ref(name, &e));
            }
        }
        // The caller's own user refs, renamed to `refs/heads/<n>`.
        if policy.branchless()
            && let Some(owner) = owner
            && (prefix.starts_with(HEADS) || HEADS.starts_with(prefix))
        {
            let own = format!("{USER_NS}{owner}/heads/");
            let from = if prefix.len() > HEADS.len() {
                format!("{own}{}", &prefix[HEADS.len()..])
            } else {
                own.clone()
            };
            let shared_names: std::collections::HashSet<String> =
                found.iter().map(|r| r.name.clone()).collect();
            for e in list(refs, repo, &from).await? {
                let stored = text(&e);
                let Some(branch) = stored.strip_prefix(&own) else {
                    continue;
                };
                let name = format!("{HEADS}{branch}");
                // The name decides the class, so this does not happen; if a legacy row ever
                // makes it, the shared ref wins.
                if !shared_names.contains(&name) {
                    found.push(wire_ref(name, &e));
                }
            }
        }
    }

    found.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let start = if req.after.is_empty() {
        0
    } else {
        found.partition_point(|r| r.name.as_bytes() <= req.after.as_bytes())
    };
    let page = &found[start..];
    let shown = &page[..page.len().min(limit)];
    let next = if page.len() > shown.len() {
        shown.last().map(|r| r.name.clone()).unwrap_or_default()
    } else {
        String::new()
    };
    Ok(wire::ListRefsResponse {
        refs: shown.to_vec(),
        next,
        head: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(branchless: bool) -> RepoPolicy {
        RepoPolicy::from_parts(branchless, None, "master", "[]").unwrap()
    }

    #[test]
    fn glob_matches_like_shared_refs() {
        assert!(glob("refs/heads/release/*", "refs/heads/release/1.0"));
        assert!(glob("refs/heads/release/*", "refs/heads/release/a/b"));
        assert!(!glob("refs/heads/release/*", "refs/heads/release"));
        assert!(!glob("refs/heads/master", "refs/heads/master2"));
        assert!(glob("refs/tags/*", "refs/tags/v1"));
        assert!(glob("*", "anything"));
    }

    #[test]
    fn user_ref_names_round_trip_and_only_canonical_ids_parse() {
        let n = user_ref_name(42, "refs/heads/feat/x").unwrap();
        assert_eq!(n, "refs/users/42/heads/feat/x");
        assert_eq!(parse_user_ref(&n), Some((42, "feat/x")));
        assert_eq!(parse_user_ref("refs/users/042/heads/x"), None);
        assert_eq!(parse_user_ref("refs/users/0/heads/x"), None);
        assert_eq!(parse_user_ref("refs/users/alice/heads/x"), None);
        assert_eq!(parse_user_ref("refs/users/42/tags/x"), None);
        assert_eq!(user_ref_name(1, "refs/tags/x"), None);
    }

    #[test]
    fn owner_comes_from_a_verified_forgejo_identity_only() {
        let p = |s: &str| Principal { id: s.into() };
        assert_eq!(owner_of(&p("42:alice")), Some(42));
        // A bare number is not an identity, whoever issued it.
        assert_eq!(owner_of(&p("7")), None);
        assert_eq!(owner_of(&p("pat:7")), None);
        assert_eq!(owner_of(&p("deploy:o/r")), None);
        // A deploy key acts for its repository, not for the user who owns it.
        assert_eq!(owner_of(&p("2:org:5")), None);
        assert_eq!(owner_of(&p("alice")), None);
        assert_eq!(owner_of(&p("0:root")), None);
        assert_eq!(owner_of(&p("forgejo-internal")), None);
    }

    #[test]
    fn classification_follows_the_table() {
        let b = policy(true);
        assert_eq!(
            b.classify(Some(5), "refs/heads/master").unwrap(),
            Class::Shared
        );
        assert_eq!(
            b.classify(Some(5), "refs/heads/release/1").unwrap(),
            Class::Shared
        );
        assert_eq!(b.classify(Some(5), "refs/tags/v1").unwrap(), Class::Shared);
        assert_eq!(
            b.classify(Some(5), "refs/pull/3/head").unwrap(),
            Class::Managed
        );
        assert_eq!(
            b.classify(Some(5), "refs/heads/feat").unwrap(),
            Class::User {
                owner: 5,
                stored: "refs/users/5/heads/feat".into()
            }
        );
        assert_eq!(
            b.classify(Some(5), "refs/heads/agent/x")
                .unwrap()
                .stored("refs/heads/agent/x"),
            "refs/users/5/heads/agent/x"
        );
        assert_eq!(
            b.classify(None, "refs/heads/feat").unwrap_err().code(),
            tonic::Code::PermissionDenied
        );
        assert_eq!(
            b.classify(Some(5), "refs/users/6/heads/x")
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument
        );
        // Branch mode: everything is shared, but the user namespace is still closed.
        let m = policy(false);
        assert_eq!(m.classify(None, "refs/heads/feat").unwrap(), Class::Shared);
        assert!(m.classify(Some(5), "refs/users/5/heads/x").is_err());
    }

    #[test]
    fn a_tag_outside_shared_refs_is_ref_not_shared() {
        let p = RepoPolicy::from_parts(true, Some(r#"["master"]"#), "master", "[]").unwrap();
        let e = p.classify(Some(5), "refs/tags/v1").unwrap_err();
        let code = asc_rpc::server::status_rpc_error(&e).map(|e| e.code().to_owned());
        assert_eq!(code.as_deref(), Some("REF_NOT_SHARED"));
        assert!(p.classify(Some(5), "refs/notes/x").is_err());
    }

    #[test]
    fn patterns_are_normalised_bounded_and_never_reach_the_user_namespace() {
        let p = RepoPolicy::from_parts(
            true,
            Some(r#"["trunk","refs/heads/x/*","refs/users/*","*"]"#),
            "master",
            r#"["land"]"#,
        )
        .unwrap();
        assert!(p.is_shared("refs/heads/trunk"));
        assert!(p.is_shared("refs/heads/land"));
        assert!(p.is_shared("refs/heads/x/y"));
        assert!(RepoPolicy::from_parts(true, Some("not json"), "m", "[]").is_err());
        assert!(RepoPolicy::from_parts(true, Some(r#"[""]"#), "m", "[]").is_err());
        let many = serde_json::to_string(&vec!["a"; MAX_PATTERNS + 1]).unwrap();
        assert!(RepoPolicy::from_parts(true, Some(&many), "m", "[]").is_err());
    }

    // ---------------------------------------------------------------------- authorization

    use asc_core::id::{ChangesetId, RefTarget};
    use asc_core::path::RefName;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate};
    use uuid::Uuid;

    fn update(name: &str, new: Option<RefTarget>) -> RefUpdate {
        RefUpdate {
            name: RefName::new(name.as_bytes()).unwrap(),
            expect: Expect::Any,
            new,
            expires_at: None,
        }
    }

    /// Runs the transaction check for `actor` over `updates` against an empty repository.
    async fn check(
        actor: &str,
        updates: Vec<RefUpdate>,
        writer: UserRefWriter,
    ) -> Result<(), Status> {
        let registry = crate::ingest::testutil::registry().await;
        let entry = crate::ingest::testutil::repo(&registry).await;
        let txn = RefTxn {
            updates,
            actor: Actor {
                id: actor.into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: Uuid::new_v4(),
        };
        let mut tx = registry.refs().begin().await.unwrap();
        check_user_ref_writes(&mut tx, entry.key, &txn, writer, MAX_USER_REFS as usize).await
    }

    fn tip() -> Option<RefTarget> {
        Some(RefTarget::Changeset(ChangesetId::from_bytes([1; 32])))
    }

    #[tokio::test]
    async fn only_the_owner_may_delete_a_user_ref() {
        let delete = |name: &str| vec![update(name, None)];
        let denied = |r: Result<(), Status>| r.unwrap_err().code();
        let c = UserRefWriter::Client;
        // Another owner's ref, whoever asks: a Forgejo user, a deploy key, a token, the worker.
        for who in ["43:bob", "2:org:5", "pat:42", "42", "asc-expiry", ""] {
            assert_eq!(
                denied(check(who, delete("refs/users/42/heads/x"), c).await),
                tonic::Code::PermissionDenied,
                "{who:?}"
            );
        }
        // The same refs written or moved are refused as before.
        assert_eq!(
            denied(check("43:bob", vec![update("refs/users/42/heads/x", tip())], c).await),
            tonic::Code::PermissionDenied
        );
        // One foreign delete spoils a batch that also holds the actor's own.
        let both = vec![
            update("refs/users/43/heads/mine", None),
            update("refs/users/42/heads/x", None),
        ];
        assert_eq!(
            denied(check("43:bob", both, c).await),
            tonic::Code::PermissionDenied
        );
        // The owner deletes their own, and anyone deletes a shared ref (other checks apply there).
        check("42:alice", delete("refs/users/42/heads/x"), c)
            .await
            .unwrap();
        check("43:bob", delete("refs/heads/feature"), c)
            .await
            .unwrap();
        // A malformed user ref name is invalid, deletes included.
        for bad in [
            "refs/users/alice/heads/x",
            "refs/users/042/heads/x",
            "refs/users/42/tags/x",
            "refs/users/42",
        ] {
            assert_eq!(
                denied(check("42:alice", delete(bad), c).await),
                tonic::Code::InvalidArgument,
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn the_expiry_path_may_delete_user_refs_and_nothing_else() {
        let e = UserRefWriter::Expiry { at: 10 };
        let who = "asc-expiry";
        check(who, vec![update("refs/users/42/heads/agent/x", None)], e)
            .await
            .unwrap();
        // Never a create or a move, not even of the worker's choosing.
        let r = check(who, vec![update("refs/users/42/heads/agent/x", tip())], e).await;
        assert_eq!(r.unwrap_err().code(), tonic::Code::PermissionDenied);
        // Never another kind of ref.
        for name in ["refs/heads/master", "refs/heads/agent/x", "refs/tags/v1"] {
            let r = check(who, vec![update(name, None)], e).await;
            assert_eq!(
                r.unwrap_err().code(),
                tonic::Code::PermissionDenied,
                "{name}"
            );
        }
        let r = check(who, vec![update("refs/users/x/heads/agent/x", None)], e).await;
        assert_eq!(r.unwrap_err().code(), tonic::Code::InvalidArgument);
        // The same names are refused to a client that is not the owner.
        let r = check(
            who,
            vec![update("refs/users/42/heads/agent/x", None)],
            UserRefWriter::Client,
        )
        .await;
        assert_eq!(r.unwrap_err().code(), tonic::Code::PermissionDenied);
    }

    #[test]
    fn the_owner_lock_key_is_stable_and_distinguishes_repositories_and_owners() {
        // Every server process must derive the same key for the same (repo, owner).
        assert_eq!(owner_lock_key(1, 42), owner_lock_key(1, 42));
        assert_ne!(owner_lock_key(1, 42), owner_lock_key(1, 43));
        assert_ne!(owner_lock_key(1, 42), owner_lock_key(2, 42));
        assert_ne!(owner_lock_key(1, 2), owner_lock_key(2, 1));
        assert_ne!(owner_lock_key(1, 42), 1);
    }
}
