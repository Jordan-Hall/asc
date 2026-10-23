//! Turning the revision words and pathspecs of `log`/`show` into a [`Selection`].

use std::ffi::OsString;

use asc_core::id::ChangesetId;
use asc_core::path::RepoPath;
use asc_engine::history::HistoryApi;

use super::opts::{Opts, Tok};
use super::refs;
use crate::cli::add::{cwd_prefix, spec::repo_path};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::restore::{resolve_commit, tool};

/// What to walk: start points, hidden points and the path filter.
#[derive(Debug, Default)]
pub struct Selection {
    pub include: Vec<ChangesetId>,
    pub exclude: Vec<ChangesetId>,
    /// `A...B`, for `--left-right`.
    pub symmetric: Option<(ChangesetId, ChangesetId)>,
    /// The name a start point was typed as, for `--source`.
    pub tips: Vec<(ChangesetId, String)>,
    pub paths: Vec<RepoPath>,
    /// No revision was given, so `HEAD` was used.
    pub implicit_head: bool,
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

fn ambiguous(ctx: &Ctx, word: &str) -> CliError {
    CliError::Raw(
        format!(
            "fatal: ambiguous argument '{word}': unknown revision or path not in the working tree.\n\
             Use '--' to separate paths from revisions, like this:\n\
             '{} <command> [<revision>...] -- [<file>...]'",
            tool(ctx)
        ),
        128,
    )
}

/// One revision word as a commit.
async fn commit_of(ctx: &Ctx, word: &str) -> Result<Option<ChangesetId>> {
    let word = if word.is_empty() { "HEAD" } else { word };
    resolve_commit(ctx, word).await
}

struct Builder<'a> {
    ctx: &'a Ctx,
    ws: &'a WsCtx,
    hist: &'a dyn HistoryApi,
    sel: Selection,
}

impl Builder<'_> {
    fn add(&mut self, id: ChangesetId, hidden: bool, name: &str) {
        if hidden {
            self.sel.exclude.push(id);
        } else {
            self.sel.include.push(id);
            self.sel.tips.push((id, name.to_owned()));
        }
    }

    async fn named(&self, word: &str) -> Result<Option<ChangesetId>> {
        match self.named_inner(word).await {
            Err(CliError::Fatal(_)) => Ok(None),
            other => other,
        }
    }

    async fn named_inner(&self, word: &str) -> Result<Option<ChangesetId>> {
        // A full git SHA-1, optionally followed by `~n` and `^n` steps, through the git map.
        let (base, suffix) = word.split_at(word.len().min(40));
        if base.len() == 40
            && (suffix.is_empty() || suffix.starts_with(['~', '^']))
            && let Ok(oid) = base.parse::<asc_core::id::GitOid>()
        {
            let row = asc_state::state::git_map::git_to_asc(&self.ws.db, self.ws.repo.key, oid, asc_core::git::ObjectKind::Commit).await?;
            if let Some(asc) = row {
                return self.steps(ChangesetId::from_bytes(asc), suffix).await;
            }
        }
        commit_of(self.ctx, word).await
    }

    /// Applies `~n` (first-parent steps) and `^n` (the n-th parent) to `id`.
    async fn steps(&self, mut id: ChangesetId, mut rest: &str) -> Result<Option<ChangesetId>> {
        while let Some(op) = rest.chars().next() {
            let digits: String = rest[1..].chars().take_while(char::is_ascii_digit).collect();
            rest = &rest[1 + digits.len()..];
            let n: usize = if digits.is_empty() { 1 } else { digits.parse().unwrap_or(usize::MAX) };
            match op {
                '~' => {
                    for _ in 0..n {
                        let Some(p) = self.ws.engine.get_changeset(id).await?.parents.first().copied() else { return Ok(None) };
                        id = p;
                    }
                }
                '^' if n == 0 => {}
                '^' => {
                    let Some(p) = self.ws.engine.get_changeset(id).await?.parents.get(n - 1).copied() else { return Ok(None) };
                    id = p;
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(id))
    }

    /// One word; `Ok(false)` when it names no revision.
    async fn word(&mut self, word: &str, not: bool) -> Result<bool> {
        let (word, not) = match word.strip_prefix('^') {
            Some(rest) => (rest, !not),
            None => (word, not),
        };
        if let Some((a, b)) = word.split_once("...") {
            let (Some(a), Some(b)) = (self.named(a).await?, self.named(b).await?) else {
                return Ok(false);
            };
            let bases = self.hist.merge_base(a, &[b], true).await.map_err(fatal)?;
            self.add(a, not, word);
            self.add(b, not, word);
            if not {
                return Ok(true);
            }
            self.sel.exclude.extend(bases);
            self.sel.symmetric = Some((a, b));
            return Ok(true);
        }
        if let Some((a, b)) = word.split_once("..") {
            let (Some(a), Some(b)) = (self.named(a).await?, self.named(b).await?) else {
                return Ok(false);
            };
            self.add(a, !not, word);
            self.add(b, not, word);
            return Ok(true);
        }
        if let Some(base) = word.strip_suffix("^!") {
            let Some(c) = self.named(base).await? else { return Ok(false) };
            let parents = self.ws.engine.get_changeset(c).await?.parents.clone();
            self.add(c, not, word);
            for p in parents {
                self.add(p, !not, word);
            }
            return Ok(true);
        }
        if let Some(base) = word.strip_suffix("^@") {
            let Some(c) = self.named(base).await? else { return Ok(false) };
            for p in self.ws.engine.get_changeset(c).await?.parents.clone() {
                self.add(p, not, word);
            }
            return Ok(true);
        }
        let Some(c) = self.named(word).await? else { return Ok(false) };
        self.add(c, not, word);
        Ok(true)
    }

    async fn refs_under(&mut self, prefix: &str, not: bool) -> Result<()> {
        for tip in refs::tips(self.ws, prefix).await? {
            self.add(tip.commit, not, &tip.name);
        }
        Ok(())
    }
}

/// Resolves the revision words and pathspecs of `opts`.
pub async fn select(ctx: &Ctx, hist: &dyn HistoryApi, opts: &Opts) -> Result<Selection> {
    let ws = ctx.ws()?;
    let mut b = Builder { ctx, ws, hist, sel: Selection::default() };
    let mut paths: Vec<OsString> = opts.paths.clone();
    let mut not = false;
    let mut words = opts.revs.iter().enumerate();
    while let Some((_, tok)) = words.next() {
        match tok {
            Tok::Not => not = !not,
            Tok::All => {
                if let Some(h) = ws.head_commit().await? {
                    b.add(h, not, "HEAD");
                }
                b.refs_under("refs/", not).await?;
            }
            Tok::Branches => b.refs_under("refs/heads/", not).await?,
            Tok::Tags => b.refs_under("refs/tags/", not).await?,
            Tok::Remotes => b.refs_under("refs/remotes/", not).await?,
            Tok::Rev(word) => {
                if b.word(word, not).await? {
                    continue;
                }
                if opts.dashdash {
                    return Err(CliError::Raw(format!("fatal: bad revision '{word}'"), 128));
                }
                // The first word that is no revision starts the paths, if it is one.
                let exists = ctx.cwd.join(word).exists();
                if !exists {
                    return Err(ambiguous(ctx, word));
                }
                let mut rest: Vec<OsString> = vec![word.into()];
                for (_, t) in words.by_ref() {
                    if let Tok::Rev(w) = t {
                        rest.push(w.into());
                    }
                }
                rest.extend(paths);
                paths = rest;
                break;
            }
        }
    }
    let mut sel = b.sel;
    if opts.revs.is_empty() {
        sel.implicit_head = true;
        match ws.head_commit().await? {
            Some(h) => {
                sel.include.push(h);
                sel.tips.push((h, "HEAD".into()));
            }
            None => return Err(unborn(ws)),
        }
    }
    resolve_paths(ctx, ws, &paths, &mut sel)?;
    Ok(sel)
}

/// git's message for `log` on a branch without commits.
fn unborn(ws: &WsCtx) -> CliError {
    let branch = match ws.projection.read_head() {
        Ok(asc_git::projection::Head::Branch(b)) => b,
        _ => "HEAD".into(),
    };
    CliError::Raw(format!("fatal: your current branch '{branch}' does not have any commits yet"), 128)
}

fn resolve_paths(ctx: &Ctx, ws: &WsCtx, items: &[OsString], sel: &mut Selection) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let prefix = cwd_prefix(ctx, ws);
    let mut bytes = Vec::new();
    for item in items {
        let text = item.to_string_lossy();
        if text.starts_with(':') || text.contains(['*', '?', '[']) {
            return Err(CliError::Fatal(format!(
                "asc: pathspec '{text}': magic and wildcards are not supported by this command yet"
            )));
        }
        let path = repo_path(item, &prefix, ws.projection.toplevel())?;
        if path.is_empty() {
            // The repository root selects everything.
            return Ok(());
        }
        bytes.push(path);
    }
    for p in &bytes {
        sel.paths.push(RepoPath::new(p.clone()).map_err(fatal)?);
    }
    Ok(())
}

/// A selection that only carries pathspecs (`show` limits its diffs with them).
pub fn paths_only(ctx: &Ctx, items: &[OsString]) -> Result<Selection> {
    let mut sel = Selection::default();
    resolve_paths(ctx, ctx.ws()?, items, &mut sel)?;
    Ok(sel)
}
