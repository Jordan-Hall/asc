//! The atoms that need the repository beyond the object: unambiguous short names, `upstream` and
//! `push`, `worktreepath`, `describe`, `ahead-behind` and `is-base`. Each is computed once per
//! ref before rendering and stored in `Item::extra`.

use std::collections::{HashMap, HashSet};

use asc_core::id::ChangesetId;
use asc_engine::derive::Gen;
use asc_engine::graph::LogQuery;
use asc_engine::history::{HistoryApi, PageToken};
use asc_state::state::workspaces;
use asc_store::refs::Refs as _;

use super::item::Item;
use super::load::fatal;
use super::parse::Strip;
use super::parse::{Atom, Kind, Remote, RemoteWhat};
use super::track;
use super::value::{lstrip, rstrip};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::log::describe::Describer;
use crate::cli::log::walk::Hist;
use crate::cli::revparse::commit_of;

/// git's `ref_rev_parse_rules`, as prefix and suffix around the short name.
const RULES: [(&str, &str); 6] = [
    ("", ""),
    ("refs/", ""),
    ("refs/tags/", ""),
    ("refs/heads/", ""),
    ("refs/remotes/", ""),
    ("refs/remotes/", "/HEAD"),
];

/// `shorten_unambiguous_ref` in strict mode: the shortest form no other lookup rule resolves.
pub fn shorten(names: &HashSet<String>, full: &str) -> String {
    for i in (1..RULES.len()).rev() {
        let (pre, post) = RULES[i];
        let Some(short) = full
            .strip_prefix(pre)
            .and_then(|r| r.strip_suffix(post))
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let clash = RULES
            .iter()
            .enumerate()
            .any(|(j, (p, s))| j != i && names.contains(&format!("{p}{short}{s}")));
        if !clash {
            return short.to_owned();
        }
    }
    full.to_owned()
}

/// Commits reachable from `include` and not from `exclude`.
pub async fn count(
    api: &dyn HistoryApi,
    include: ChangesetId,
    exclude: ChangesetId,
) -> Result<u64> {
    let query = LogQuery {
        include: vec![include],
        exclude: vec![exclude],
        ..LogQuery::default()
    };
    let (mut total, mut page): (u64, Option<PageToken>) = (0, None);
    loop {
        let got = api.log(&query, page.as_ref()).await.map_err(fatal)?;
        total += got.entries.len() as u64;
        match got.next {
            Some(next) => page = Some(next),
            None => return Ok(total),
        }
    }
}

fn strip(name: &str, how: Strip, names: &HashSet<String>) -> String {
    match how {
        Strip::Whole => name.to_owned(),
        Strip::Short => shorten(names, name),
        Strip::Lstrip(n) => lstrip(name, n),
        Strip::Rstrip(n) => rstrip(name, n),
    }
}

/// What `%(upstream:…)` or `%(push:…)` prints for the branch `branch`.
async fn remote_value(
    ctx: &Ctx,
    ws: &WsCtx,
    api: &dyn HistoryApi,
    names: &HashSet<String>,
    branch: &str,
    remote: Remote,
    push: bool,
) -> Result<String> {
    match remote.what {
        RemoteWhat::RemoteName => {
            let (name, explicit) = if push {
                track::push_remote(&ctx.config, branch)
            } else {
                track::fetch_remote(&ctx.config, branch)
            };
            return Ok(if explicit { name } else { String::new() });
        }
        RemoteWhat::RemoteRef => {
            let merge = if push {
                track::push_remote_ref(&ctx.config, branch)
            } else {
                track::upstream(&ctx.config, branch).merge
            };
            return Ok(merge.unwrap_or_default());
        }
        _ => {}
    }
    let dst = if push {
        track::push_target(&ctx.config, branch)
    } else {
        track::upstream(&ctx.config, branch).upstream
    };
    let Some(dst) = dst else {
        return Ok(String::new());
    };
    match remote.what {
        RemoteWhat::Ref(how) => Ok(strip(&dst, how, names)),
        RemoteWhat::RemoteName | RemoteWhat::RemoteRef => Ok(String::new()),
        RemoteWhat::Track | RemoteWhat::TrackShort => {
            let tip = tip_of(ws, &format!("refs/heads/{branch}")).await?;
            let base = tip_of(ws, &dst).await?;
            let (Some(tip), Some(base)) = (tip, base) else {
                return Ok(if remote.what == RemoteWhat::Track {
                    bracket("gone", remote.nobracket)
                } else {
                    String::new()
                });
            };
            let (ahead, behind) = (count(api, tip, base).await?, count(api, base, tip).await?);
            Ok(match remote.what {
                RemoteWhat::TrackShort => match (ahead, behind) {
                    (0, 0) => "=",
                    (0, _) => "<",
                    (_, 0) => ">",
                    _ => "<>",
                }
                .to_owned(),
                _ => {
                    let text = match (ahead, behind) {
                        (0, 0) => String::new(),
                        (0, b) => format!("behind {b}"),
                        (a, 0) => format!("ahead {a}"),
                        (a, b) => format!("ahead {a}, behind {b}"),
                    };
                    if text.is_empty() {
                        text
                    } else {
                        bracket(&text, remote.nobracket)
                    }
                }
            })
        }
    }
}

fn bracket(text: &str, nobracket: bool) -> String {
    if nobracket {
        text.to_owned()
    } else {
        format!("[{text}]")
    }
}

/// The commit a ref leads to (a tag peeled), `None` when the ref does not exist.
async fn tip_of(ws: &WsCtx, name: &str) -> Result<Option<ChangesetId>> {
    let Ok(name) = asc_core::path::RefName::new(name.as_bytes().to_vec()) else {
        return Ok(None);
    };
    let Some(entry) = ws.db.refs().get(ws.repo.key, &name).await? else {
        return Ok(None);
    };
    Ok(Some(crate::cli::log::refs::peel(ws, entry.target).await?.0))
}

/// Fills `Item::extra` for every extra atom of the format and sort keys.
pub async fn fill(
    ctx: &Ctx,
    ws: &WsCtx,
    hist: &Hist,
    items: &mut [Item],
    atoms: &[&Atom],
) -> Result<()> {
    let mut seen = HashSet::new();
    let wanted: Vec<&Atom> = atoms
        .iter()
        .copied()
        .filter(|a| a.is_extra() && seen.insert(a.text.clone()))
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let api = hist.api(ws);
    let needs_names = wanted.iter().any(|a| match &a.kind {
        Kind::Refname(Strip::Short) | Kind::Symref(Strip::Short) => true,
        Kind::Upstream(r) | Kind::Push(r) => matches!(r.what, RemoteWhat::Ref(Strip::Short)),
        _ => false,
    });
    let mut names: HashSet<String> = HashSet::new();
    if needs_names {
        names = ws
            .db
            .refs()
            .list(ws.repo.key, "refs/")
            .await?
            .into_iter()
            .map(|e| String::from_utf8_lossy(e.name.as_bytes()).into_owned())
            .collect();
        names.insert("HEAD".to_owned());
    }
    let describer = if wanted.iter().any(|a| matches!(a.kind, Kind::Describe(_))) {
        Some(
            Describer::load(
                ws,
                ctx.config
                    .get("core.abbrev")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(7),
            )
            .await?,
        )
    } else {
        None
    };
    let mut bases: HashMap<String, ChangesetId> = HashMap::new();
    for atom in &wanted {
        if let Kind::AheadBehind(arg) = &atom.kind {
            let base = commit_of(ctx, arg)
                .await?
                .ok_or_else(|| CliError::Fatal(format!("failed to find '{arg}'")))?;
            bases.insert(arg.clone(), base);
        }
    }
    for item in items.iter_mut() {
        for atom in &wanted {
            let value = match &atom.kind {
                Kind::Refname(Strip::Short) => shorten(&names, &item.name),
                Kind::Symref(Strip::Short) => item
                    .symref
                    .as_deref()
                    .map(|s| shorten(&names, s))
                    .unwrap_or_default(),
                _ if atom.deref => continue,
                Kind::Upstream(r) | Kind::Push(r) => match item.name.strip_prefix("refs/heads/") {
                    Some(branch) => {
                        let push = matches!(atom.kind, Kind::Push(_));
                        remote_value(ctx, ws, api, &names, branch, *r, push).await?
                    }
                    None => String::new(),
                },
                Kind::Worktreepath => match item.name.strip_prefix("refs/heads/") {
                    Some(branch) => workspaces::holding_branch(&ws.db, ws.repo.key, branch)
                        .await?
                        .map(|w| w.ws.path.replace('\\', "/"))
                        .unwrap_or_default(),
                    None => String::new(),
                },
                Kind::Describe(options) => match (&describer, item.commit) {
                    (Some(d), Some(cs)) => {
                        let git = ws.engine.git_commit_of(cs).await.map_err(fatal)?;
                        d.describe(api, cs, &git.to_string(), options).await
                    }
                    _ => String::new(),
                },
                Kind::AheadBehind(arg) => match item.commit {
                    Some(cs) => {
                        let base = bases[arg];
                        format!(
                            "{} {}",
                            count(api, cs, base).await?,
                            count(api, base, cs).await?
                        )
                    }
                    None => String::new(),
                },
                _ => String::new(),
            };
            item.extra.insert(atom.text.clone(), value.into_bytes());
        }
    }
    Ok(())
}

/// `%(is-base:<committish>)`: marks the one ref whose first-parent history is closest to the
/// committish's (ties go to the lowest generation, then the earliest ref).
pub async fn assign_is_base(
    ctx: &Ctx,
    ws: &WsCtx,
    items: &mut [Item],
    atoms: &[&Atom],
) -> Result<()> {
    let mut seen = HashSet::new();
    let wanted: Vec<&Atom> = atoms
        .iter()
        .copied()
        .filter(|a| matches!(a.kind, Kind::IsBase(_)) && seen.insert(a.text.clone()))
        .collect();
    for atom in wanted {
        let Kind::IsBase(arg) = &atom.kind else {
            continue;
        };
        let tip = commit_of(ctx, arg)
            .await?
            .ok_or_else(|| CliError::Fatal(format!("failed to find '{arg}'")))?;
        // Position of each commit on the first-parent chain of the committish.
        let mut chain: HashMap<ChangesetId, usize> = HashMap::new();
        let mut at = Some(tip);
        while let Some(cs) = at {
            if chain.insert(cs, chain.len()).is_some() {
                break;
            }
            at = ws.engine.get_changeset(cs).await?.parents.first().copied();
        }
        let mut best: Option<((usize, u32), usize)> = None;
        let mut known: HashMap<ChangesetId, Option<usize>> = HashMap::new();
        // Ties go to the earliest ref in listing (name) order, whatever the output order is.
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by(|&a, &b| items[a].name.cmp(&items[b].name));
        for index in order {
            let item = &items[index];
            let Some(start) = item.commit else { continue };
            let mut path = Vec::new();
            let mut cursor = Some(start);
            let mut found = None;
            while let Some(cs) = cursor {
                if let Some(hit) = known.get(&cs) {
                    found = *hit;
                    break;
                }
                if let Some(&distance) = chain.get(&cs) {
                    found = Some(distance);
                    known.insert(cs, found);
                    break;
                }
                path.push(cs);
                cursor = ws.engine.get_changeset(cs).await?.parents.first().copied();
            }
            for cs in path {
                known.insert(cs, found);
            }
            if let Some(distance) = found {
                // Nearest branch point first; among equals the ref with the lowest generation.
                let generation = ws
                    .engine
                    .get_derived::<Gen>(start)
                    .await
                    .map_err(fatal)?
                    .generation;
                let key = (distance, generation);
                if best.is_none_or(|(k, _)| key < k) {
                    best = Some((key, index));
                }
            }
        }
        for (index, item) in items.iter_mut().enumerate() {
            let text = if best.is_some_and(|(_, i)| i == index) {
                format!("({arg})")
            } else {
                String::new()
            };
            item.extra.insert(atom.text.clone(), text.into_bytes());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn short_names_avoid_ambiguity() {
        let all = names(&[
            "refs/heads/master",
            "refs/tags/v1",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/master",
        ]);
        assert_eq!(shorten(&all, "refs/heads/master"), "master");
        assert_eq!(shorten(&all, "refs/remotes/origin/master"), "origin/master");
        assert_eq!(shorten(&all, "refs/remotes/origin/HEAD"), "origin");
        let none = names(&["refs/heads/dev", "refs/tags/v1"]);
        assert_eq!(shorten(&none, "refs/heads/dev"), "dev");
        assert_eq!(shorten(&none, "refs/tags/v1"), "v1");
        let both = names(&["refs/heads/x", "refs/tags/x"]);
        assert_eq!(shorten(&both, "refs/heads/x"), "heads/x");
        assert_eq!(shorten(&none, "HEAD"), "HEAD");
    }
}
