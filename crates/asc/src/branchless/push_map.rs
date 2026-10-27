//! Client push mapping in branchless mode (SPEC §21.4.3, PLAN B12).
//!
//! `asc push origin feat` of a non-shared branch is one `UpdateRefs`: the user ref
//! (`refs/heads/feat`, which the server maps to the pusher's user ref) plus one
//! `refs/for/<trunk>/asc-<change>` AGit ref per change of `trunk..feat`, bottom to top. The
//! server fails the whole update when any of them is refused, so nothing moves.
//!
//! This file holds the decisions and the local range walk; `cli/push.rs` owns the output, which
//! stays git's for the branch line (the AGit refs never appear in it).
//!
//! * The mapping is on only when the client is branchless (`asc.branchless`), the server
//!   advertises `branchless.v1` and its `RepoConfig` says the repo is branchless. Anything else,
//!   including an answer that cannot be read, is a plain push, as git does it.
//! * `--all`, `--mirror`, tags, deletions and shared refs never map. `asc.push.review=none` maps
//!   to the user ref only.
//! * A range that contains a merge changeset is refused before anything is sent (SPEC §20.3 ends
//!   a stack at a merge, and D1 allows one change per review).
//! * Commits without a `ChangeMeta` get an id by the C01 rule: jj header, Gerrit trailer, then
//!   the migration-mode derived id (B18a), scoped by the branch ref.
//!
//! Known gap (C05): the trunk and the range are derived here from `asc.stack.trunk` and first-parent
//! ancestry, because the stack derivation context is private to the stack verbs. It agrees with
//! `asc submit` for a stack on one trunk.

use asc_core::change::ChangeId;
use asc_core::change_migrate::migration_id;
use asc_core::git::EXTRA_GIT_COMMIT;
use asc_core::id::{ChangesetId, RefTarget};
use asc_engine::change::ChangeRows;
use asc_engine::history::HistoryApi;
use asc_engine::sync::{Error as SyncError, PushUpdate};
use asc_rpc::server::{MAX_MESSAGE, wire};
use asc_rpc::services::repo_client::RepoClient;
use asc_store::refs::Expect;
use tonic::codec::CompressionEncoding;

use super::mode::{PushReview, Settings};
use std::collections::HashMap;

use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::switch::{ref_name, ref_target};
use crate::remote::server::ServerRemote;

/// The server feature that says branchless repositories exist (SPEC §21.3.5).
pub const FEATURE: &str = "branchless.v1";

/// Why a range with a merge is refused (SPEC §21.4.3 step 3).
pub const MERGE_REASON: &str =
    "asc: stack contains a merge commit; rebase it or submit changes individually";

/// Most changes one push sends (the server's `Changes` limit per stack, as `asc submit`).
pub const MAX_CHANGES: usize = 256;

/// The AGit ref of one change, as `asc submit` names it: `refs/for/<trunk>/asc-<change>`.
pub fn agit_ref(trunk: &str, change: &ChangeId) -> String {
    format!("refs/for/{trunk}/asc-{change}")
}

/// Longest reason printed for a server refusal.
const MAX_REASON: usize = 512;
/// Most shared-ref patterns taken from the server.
const MAX_PATTERNS: usize = 256;
/// Longest shared-ref pattern taken from the server.
const MAX_PATTERN_LEN: usize = 256;

/// What the server said about the repo.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// `RepoConfig.shared_refs`; empty means the defaults of SPEC §21.5.1.
    pub shared_refs: Vec<String>,
    /// `VersionResponse.features`, for the `m/` objects of `changes.v1` (C23).
    pub features: Vec<String>,
}

/// Asks the server whether the repo is branchless. `None`: it is not, or the server cannot say.
pub async fn probe(server: &ServerRemote) -> Option<Policy> {
    let version = server.client().server_version().await.ok()?;
    if !version.features.iter().any(|f| f == FEATURE) {
        return None;
    }
    let wire_conn = server.session().wire.clone();
    let req = wire::RepoConfigRequest {
        repo: Some(wire::RepoRef {
            name: wire_conn.repo.clone(),
            id_mode: asc_rpc::IdMode::Asc.to_wire(),
        }),
        rev: String::new(),
    };
    let conf = RepoClient::new(wire_conn.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
        .accept_compressed(CompressionEncoding::Zstd)
        .repo_config(wire_conn.request(req, None))
        .await
        .ok()?
        .into_inner();
    conf.branchless.then(|| Policy {
        features: version.features.clone(),
        shared_refs: conf
            .shared_refs
            .into_iter()
            .filter(|p| !p.is_empty() && p.len() <= MAX_PATTERN_LEN)
            .take(MAX_PATTERNS)
            .collect(),
    })
}

/// `*` matches any run of bytes, everything else is literal.
fn glob(pat: &[u8], s: &[u8]) -> bool {
    let (mut p, mut i) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while i < s.len() {
        if p < pat.len() && pat[p] == b'*' {
            star = Some((p, i));
            p += 1;
        } else if p < pat.len() && pat[p] == s[i] {
            p += 1;
            i += 1;
        } else if let Some((sp, si)) = star {
            p = sp + 1;
            i = si + 1;
            star = Some((sp, si + 1));
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&b| b == b'*')
}

impl Policy {
    /// Whether `full` (`refs/heads/<n>`) is a shared ref, that is, one a push updates as
    /// itself and not as the pusher's user ref (SPEC §21.5.1). `trunk` is a shared ref always.
    pub fn is_shared(&self, full: &str, trunk: Option<&str>) -> bool {
        if trunk.is_some_and(|t| full == format!("refs/heads/{t}")) {
            return true;
        }
        if self.shared_refs.is_empty() {
            return full.strip_prefix("refs/heads/").is_some_and(|n| {
                matches!(n, "master" | "main")
                    || n.starts_with("release/")
                    || n.starts_with("green/")
            });
        }
        self.shared_refs
            .iter()
            .any(|p| glob(p.as_bytes(), full.as_bytes()))
    }
}

/// The push option for `asc.push.review`, `None` when only the user ref goes (`none`).
pub fn review_option(review: PushReview) -> Option<&'static str> {
    match review {
        PushReview::Draft => Some("asc.draft=true"),
        PushReview::Publish => Some("asc.draft=false"),
        PushReview::None => None,
    }
}

/// What the walk of `trunk..tip` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Range {
    /// The changes to submit, bottom first, under `trunk`.
    Changes {
        trunk: String,
        items: Vec<(ChangesetId, ChangeId)>,
    },
    /// The range contains a merge changeset.
    Merge,
    /// More changes than one submit may send.
    TooMany(usize),
    /// No configured trunk exists locally: only the user ref goes.
    NoTrunk,
}

fn jj_header(raw: &[u8]) -> Option<&[u8]> {
    let end = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .unwrap_or(raw.len());
    raw[..end]
        .split(|&c| c == b'\n')
        .find_map(|l| l.strip_prefix(b"change-id "))
}

/// The value of the last `Change-Id:` line of the last paragraph of a message (the bounds of the
/// server's reader, so both sides derive the same id).
fn gerrit_trailer(message: &[u8]) -> Option<&[u8]> {
    let end = message
        .iter()
        .rposition(|c| !matches!(c, b'\n' | b'\r'))
        .map_or(0, |i| i + 1);
    let message = &message[..end];
    let start = message.windows(2).rposition(|w| w == b"\n\n")? + 2;
    message[start..]
        .split(|&c| c == b'\n')
        .rev()
        .find_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let colon = line.iter().position(|&c| c == b':')?;
            let (key, value) = line.split_at(colon);
            if !key.eq_ignore_ascii_case(b"Change-Id") {
                return None;
            }
            let value = value[1..].trim_ascii();
            (!value.is_empty() && value.len() <= 128 && value.iter().all(|c| c.is_ascii_graphic()))
                .then_some(value)
        })
}

/// The change id of `cs`: its `ChangeMeta` row, else the C01 rule scoped by `scope`.
async fn change_of(
    ctx: &Ctx,
    ws: &WsCtx,
    cs: ChangesetId,
    body: &asc_core::object::Changeset,
    scope: &str,
) -> Result<ChangeId> {
    let rows = crate::stack::change_rows(ctx)?;
    if let Some(row) = rows.get(cs).await.map_err(fatal)? {
        return Ok(row.change);
    }
    let _ = ws;
    let raw = body.extra.get(&EXTRA_GIT_COMMIT.to_vec());
    let jj = raw.and_then(|r| jj_header(r));
    let gerrit = gerrit_trailer(&body.message);
    migration_id(&cs, jj, gerrit, scope.as_bytes())
        .ok_or_else(|| CliError::Fatal("asc: push: no change id".into()))
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {e}"))
}

/// The configured trunk names, `master` when there are none.
fn trunk_names(ctx: &Ctx) -> Vec<String> {
    let mut names = ctx.config.get_all("asc.stack.trunk");
    names.retain(|t| !t.is_empty());
    if names.is_empty() {
        names.push("master".into());
    }
    let mut seen = std::collections::HashSet::new();
    names.retain(|t| seen.insert(t.clone()));
    names
}

/// The first configured trunk that exists, with its tip: the remote-tracking ref of `remote`
/// when there is one, else the local branch.
pub async fn trunk_of(
    ctx: &Ctx,
    ws: &WsCtx,
    remote: &str,
) -> Result<Option<(String, ChangesetId)>> {
    for t in trunk_names(ctx) {
        for full in [
            format!("refs/remotes/{remote}/{t}"),
            format!("refs/heads/{t}"),
        ] {
            if let Some(RefTarget::Changeset(tip)) = ref_target(ws, &full).await? {
                return Ok(Some((t, tip)));
            }
        }
    }
    Ok(None)
}

/// Walks `trunk..tip` first-parent. `scope` is the pushed branch ref, which scopes the ids of
/// commits that carry only a Gerrit trailer.
pub async fn range(
    ctx: &Ctx,
    ws: &WsCtx,
    remote: &str,
    tip: ChangesetId,
    scope: &str,
) -> Result<Range> {
    let Some((trunk, trunk_tip)) = trunk_of(ctx, ws, remote).await? else {
        return Ok(Range::NoTrunk);
    };
    let mut chain: Vec<(ChangesetId, std::sync::Arc<asc_core::object::Changeset>)> = Vec::new();
    let mut cur = tip;
    loop {
        if cur == trunk_tip
            || HistoryApi::is_ancestor(&ws.engine, cur, trunk_tip)
                .await
                .map_err(fatal)?
        {
            break;
        }
        let body = ws.engine.get_changeset(cur).await.map_err(fatal)?;
        if body.parents.len() > 1 {
            return Ok(Range::Merge);
        }
        if chain.len() >= MAX_CHANGES {
            return Ok(Range::TooMany(chain.len() + 1));
        }
        let next = body.parents.first().copied();
        chain.push((cur, body));
        match next {
            Some(p) => cur = p,
            None => break,
        }
    }
    chain.reverse();
    let mut items = Vec::with_capacity(chain.len());
    for (cs, body) in &chain {
        items.push((*cs, change_of(ctx, ws, *cs, body, scope).await?));
    }
    Ok(Range::Changes { trunk, items })
}

/// The AGit updates for `items`, bottom to top. They carry no expectation: the server decides
/// who may update a change, and a refusal fails the whole call.
pub fn agit_updates(trunk: &str, items: &[(ChangesetId, ChangeId)]) -> Result<Vec<PushUpdate>> {
    items
        .iter()
        .map(|(cs, change)| {
            Ok(PushUpdate {
                dst: ref_name(&agit_ref(trunk, change))?,
                new: Some(RefTarget::Changeset(*cs)),
                expect: Expect::Any,
            })
        })
        .collect()
}

/// What the mapping decided for one push.
#[derive(Debug, Default)]
pub struct Mapping {
    /// Branch refs refused before anything is sent, with the reason.
    pub refused: Vec<(String, String)>,
    /// The push option that carries `asc.push.review`.
    pub option: Option<&'static str>,
    /// The server's features, for the `m/` objects.
    pub features: Vec<String>,
    per_dst: HashMap<String, (String, Vec<(ChangesetId, ChangeId)>)>,
}

impl Mapping {
    /// The AGit updates of the branches in `dsts`, bottom to top, without repeats.
    pub fn updates_for(&self, dsts: &[&str]) -> Result<Vec<PushUpdate>> {
        let mut out: Vec<PushUpdate> = Vec::new();
        for d in dsts {
            if let Some((trunk, items)) = self.per_dst.get(*d) {
                for u in agit_updates(trunk, items)? {
                    if !out.iter().any(|o| o.dst == u.dst) {
                        out.push(u);
                    }
                }
            }
        }
        Ok(out)
    }

    /// The changesets behind [`Self::updates_for`].
    pub fn changesets_for(&self, dsts: &[&str]) -> Vec<ChangesetId> {
        let mut out: Vec<ChangesetId> = Vec::new();
        for d in dsts {
            if let Some((_, items)) = self.per_dst.get(*d) {
                for (cs, _) in items {
                    if !out.contains(cs) {
                        out.push(*cs);
                    }
                }
            }
        }
        out
    }
}

/// Decides the mapping of `candidates` (the branch refs a push creates or moves, with their new
/// tips). Nothing is asked of the server unless there is a candidate and the client is
/// branchless with `asc.push.review` not `none`.
pub async fn plan(
    ctx: &Ctx,
    ws: &WsCtx,
    server: &ServerRemote,
    remote: &str,
    candidates: &[(String, ChangesetId)],
) -> Result<Mapping> {
    let mut out = Mapping::default();
    if candidates.is_empty() {
        return Ok(out);
    }
    let settings = Settings::resolve(&ctx.config, &ctx.env, &|k| std::env::var(k).ok())
        .map_err(CliError::Fatal)?;
    if !settings.branchless {
        return Ok(out);
    }
    let Some(option) = review_option(settings.push_review) else {
        return Ok(out);
    };
    let Some(policy) = probe(server).await else {
        return Ok(out);
    };
    for (dst, tip) in candidates {
        if policy.is_shared(dst, None) {
            continue;
        }
        match range(ctx, ws, remote, *tip, dst).await? {
            Range::Changes { trunk, items } => {
                if items.is_empty() || policy.is_shared(dst, Some(&trunk)) {
                    continue;
                }
                out.per_dst.insert(dst.clone(), (trunk, items));
            }
            Range::Merge => out.refused.push((dst.clone(), MERGE_REASON.to_owned())),
            Range::TooMany(n) => out.refused.push((
                dst.clone(),
                format!(
                    "asc: {n} changes is over the limit of {MAX_CHANGES}; push a lower branch first"
                ),
            )),
            Range::NoTrunk => {}
        }
    }
    out.option = Some(option);
    out.features = policy.features;
    Ok(out)
}

/// The reason to print in `! [remote rejected] feat -> feat (<reason>)` for a refusal of the
/// update, when it is one the server made. Control characters are replaced and the length is
/// bounded. `None`: the failure is not a refusal (a ref race, a local fault).
pub fn refusal_reason(e: &SyncError) -> Option<String> {
    let text = match e {
        SyncError::Remote(m) => m.clone(),
        SyncError::Graph(g) => g.to_string(),
        SyncError::Rejected { reason, .. } => (*reason).to_owned(),
        _ => return None,
    };
    let text = text.strip_prefix("remote: ").unwrap_or(&text);
    let one_line: String = text
        .chars()
        .take(MAX_REASON)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let one_line = one_line.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(if one_line.is_empty() {
        "asc: the server refused the update".to_owned()
    } else {
        one_line
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_refs_follow_the_server_list_or_the_defaults() {
        let d = Policy::default();
        for shared in [
            "refs/heads/master",
            "refs/heads/release/1.0",
            "refs/heads/green/x",
        ] {
            assert!(d.is_shared(shared, None), "{shared}");
        }
        for own in [
            "refs/heads/feat",
            "refs/heads/releases",
            "refs/heads/me/release/1",
        ] {
            assert!(!d.is_shared(own, None), "{own}");
        }
        assert!(d.is_shared("refs/heads/develop", Some("develop")));
        let p = Policy {
            shared_refs: vec!["refs/heads/trunk".into(), "refs/heads/rel/*".into()],
            features: Vec::new(),
        };
        assert!(p.is_shared("refs/heads/trunk", None));
        assert!(p.is_shared("refs/heads/rel/2", None));
        assert!(
            !p.is_shared("refs/heads/master", None),
            "the server list wins"
        );
        assert!(!p.is_shared("refs/heads/feat", None));
    }

    #[test]
    fn review_maps_to_the_draft_option() {
        assert_eq!(review_option(PushReview::Draft), Some("asc.draft=true"));
        assert_eq!(review_option(PushReview::Publish), Some("asc.draft=false"));
        assert_eq!(review_option(PushReview::None), None);
    }

    #[test]
    fn a_refusal_reason_is_one_clean_bounded_line() {
        let e = SyncError::Remote("remote: not allowed\u{1b}[2J to\nupdate change abc".into());
        assert_eq!(
            refusal_reason(&e).unwrap(),
            "not allowed [2J to update change abc"
        );
        let long = SyncError::Remote("x".repeat(MAX_REASON * 4));
        assert_eq!(refusal_reason(&long).unwrap().len(), MAX_REASON);
        let race = SyncError::LocalMissing(asc_store::key::ObjKey::Changeset(
            ChangesetId::from_bytes([1; 32]),
        ));
        assert!(refusal_reason(&race).is_none());
    }

    #[test]
    fn trailers_and_headers_are_read_like_the_servers() {
        assert_eq!(
            gerrit_trailer(b"subject\n\nbody\n\nChange-Id: I1\n"),
            Some(&b"I1"[..])
        );
        assert_eq!(gerrit_trailer(b"subject only\n"), None);
        assert_eq!(gerrit_trailer(b"s\n\nChange-Id:\n"), None);
        assert_eq!(
            jj_header(b"tree x\nchange-id abc\n\nmsg"),
            Some(&b"abc"[..])
        );
        assert_eq!(jj_header(b"tree x\n\nchange-id abc"), None);
    }
}
