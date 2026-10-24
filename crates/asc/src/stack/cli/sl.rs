//! `asc sl` / `asc smartlog` (SPEC §20.7): the draft changes of every stack, relative to trunk.
//! `--json` is `Workspace.Stacks`. This file also owns [`Local`] and [`Env`], the local
//! stack-derivation context (graph over the engine, the change index of `state.db`, and the
//! ref-derived inputs of SPEC §20.3) that the other read verbs of the stack CLI share.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::process::ExitCode;

use async_trait::async_trait;
use asc_core::change::ChangeId;
use asc_core::id::{ChangesetId, RefTarget};
use asc_engine::change::ChangeRow;
use asc_engine::graph::{EngineSource, Graph, LogQuery, NoJump};
use asc_engine::history::HistoryApi;
use asc_engine::stack::{self as engine_stack, Stack, StackEnv, Stacks};
use asc_state::changes::StateChanges;
use asc_state::state::workspaces;
use asc_store::refs::Refs;
use asc_store::sqlrefs::SqlRefs;
use futures::TryStreamExt;

use crate::cli::common::{CliError, Ctx, ObjectStack, Result, WsCtx};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::native::NativeSpec;
use crate::refs_guard::ReadOnlyRefs;
use crate::stack::json::{self, ChangeOut, ChangeState, Signature, Sl, StackOut, Trunk};

#[derive(clap::Parser, Debug)]
#[command(name = "sl", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Only the stack of the current HEAD.
    #[arg(short = 's')]
    pub stack: bool,
    /// Also list landed and obsolete changes.
    #[arg(short = 'a')]
    pub all: bool,
    /// Only the changes in this revset (SPEC §20.3 subset).
    #[arg(short = 'r', long = "revset")]
    pub revset: Option<String>,
    /// Review data (needs an asc server).
    #[arg(long)]
    pub remote: bool,
    #[arg(long)]
    pub json: bool,
}

const FLAGS: &[&str] = &["-s", "-a", "-r", "--revset", "--remote", "--json"];

pub const NATIVE: &[NativeSpec] = &[NativeSpec { cmd: "sl", flags: FLAGS, placeholders: &[] }];

super::verb!(SL, "sl");

mod alias {
    use super::{Args, FLAGS, run};
    use crate::native::NativeSpec;

    pub const NATIVE: &[NativeSpec] = &[NativeSpec { cmd: "smartlog", flags: FLAGS, placeholders: &[] }];

    crate::stack::cli::verb!(SMARTLOG, "smartlog");
}

/// Most refs read into one derivation.
const MAX_REFS: usize = 100_000;
/// Most changes one `sl` lists.
const MAX_CHANGES: usize = 100_000;
/// The "behind" count stops here.
const BEHIND_CAP: usize = 10_000;
/// Longest subject kept.
const SUBJECT_MAX: usize = 1024;
/// The default `asc.stack.immutable` (SPEC §20.3).
const DEFAULT_IMMUTABLE: &str = "refs/remotes/*/release/*";

pub(crate) fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {e}"))
}

pub(crate) fn stack_err(e: engine_stack::Error) -> CliError {
    match e {
        engine_stack::Error::Revset(m) => CliError::Usage(format!("asc: revset: {m}")),
        other => fatal(other),
    }
}

/// Whether `pat` matches `s`, where `*` matches any run of bytes (slashes included, so a
/// pattern can only over-protect) and everything else is literal.
pub(crate) fn glob(pat: &[u8], s: &[u8]) -> bool {
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

/// Text safe to print: control characters (escape sequences included) become `?`.
pub(crate) fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// The ref-derived inputs of stack derivation, read once from `state.db` and the config.
pub(crate) struct Env<'a> {
    ws: &'a WsCtx,
    /// Every non-tag ref with a UTF-8 name.
    refs: BTreeMap<String, ChangesetId>,
    trunks: Vec<String>,
    immutable: Vec<String>,
    /// HEADs of every workspace of the repository, the current one included.
    ws_heads: Vec<ChangesetId>,
    /// Other workspaces' HEAD and path.
    others: Vec<(ChangesetId, String)>,
    current: Option<ChangesetId>,
    branches: HashMap<ChangesetId, Vec<String>>,
    email: Option<Vec<u8>>,
}

/// One configured trunk with its tip.
pub(crate) struct TrunkInfo {
    pub name: String,
    pub tip: ChangesetId,
    pub remote: Option<String>,
}

impl<'a> Env<'a> {
    async fn load(ctx: &'a Ctx, ws: &'a WsCtx) -> Result<Self> {
        let entries = ws.db.refs().list(ws.repo.key, "refs/").await?;
        if entries.len() > MAX_REFS {
            return Err(fatal(format!("more than {MAX_REFS} refs")));
        }
        let mut refs = BTreeMap::new();
        let mut branches: HashMap<ChangesetId, Vec<String>> = HashMap::new();
        for e in entries {
            let RefTarget::Changeset(cs) = e.target else { continue };
            let Ok(name) = String::from_utf8(e.name.as_bytes().to_vec()) else { continue };
            if let Some(b) = name.strip_prefix("refs/heads/") {
                branches.entry(cs).or_default().push(b.to_owned());
            }
            refs.insert(name, cs);
        }
        let mut trunks = ctx.config.get_all("asc.stack.trunk");
        trunks.retain(|t| !t.is_empty());
        if trunks.is_empty() {
            trunks.push("master".into());
        }
        let mut seen = HashSet::new();
        trunks.retain(|t| seen.insert(t.clone()));
        let mut immutable = ctx.config.get_all("asc.stack.immutable");
        immutable.retain(|t| !t.is_empty());
        if immutable.is_empty() {
            immutable.push(DEFAULT_IMMUTABLE.into());
        }
        let current = ws.head_commit().await?;
        let mut ws_heads: Vec<ChangesetId> = current.into_iter().collect();
        let mut others = Vec::new();
        for w in workspaces::list(&ws.db, Some(ws.repo.key)).await? {
            if w.ws.id == ws.workspace.ws.id {
                continue;
            }
            let head = if w.ws.head.is_empty() {
                w.ws.base_cs
            } else {
                let name = w.ws.head.strip_prefix("refs/heads/").unwrap_or(&w.ws.head);
                refs.get(&format!("refs/heads/{name}")).copied()
            };
            if let Some(h) = head {
                ws_heads.push(h);
                others.push((h, w.ws.path.clone()));
            }
        }
        for v in branches.values_mut() {
            v.sort();
        }
        Ok(Self {
            ws,
            refs,
            trunks,
            immutable,
            ws_heads,
            others,
            current,
            branches,
            email: ctx.config.get("user.email").map(String::into_bytes),
        })
    }

    fn remotes(&self) -> Vec<String> {
        let mut set = BTreeSet::new();
        for k in self.refs.keys() {
            if let Some(rest) = k.strip_prefix("refs/remotes/")
                && let Some((r, _)) = rest.split_once('/')
            {
                set.insert(r.to_owned());
            }
        }
        let mut out: Vec<String> = set.into_iter().collect();
        if let Some(i) = out.iter().position(|r| r == "origin") {
            out.swap(0, i);
        }
        out
    }

    /// The trunks that exist: the remote-tracking ref if present, else the local branch.
    pub fn trunk_info(&self) -> Vec<TrunkInfo> {
        let remotes = self.remotes();
        let mut out = Vec::new();
        for t in &self.trunks {
            let remote = remotes
                .iter()
                .find_map(|r| self.refs.get(&format!("refs/remotes/{r}/{t}")).map(|cs| (r.clone(), *cs)));
            if let Some((r, tip)) = remote {
                out.push(TrunkInfo { name: t.clone(), tip, remote: Some(r) });
            } else if let Some(&tip) = self.refs.get(&format!("refs/heads/{t}")) {
                out.push(TrunkInfo { name: t.clone(), tip, remote: None });
            }
        }
        out
    }

    /// The configured trunk names (existing or not).
    pub fn trunk_names(&self) -> &[String] {
        &self.trunks
    }

    pub fn branches_at(&self, cs: ChangesetId) -> Vec<String> {
        self.branches.get(&cs).cloned().unwrap_or_default()
    }

    /// The path of another workspace whose HEAD is `cs`.
    pub fn worktree_at(&self, cs: ChangesetId) -> Option<String> {
        self.others.iter().find(|(h, _)| *h == cs).map(|(_, p)| p.clone())
    }
}

fn env_err(e: impl std::fmt::Display) -> engine_stack::Error {
    engine_stack::Error::Env(e.to_string())
}

#[async_trait]
impl StackEnv for Env<'_> {
    async fn trunk_tips(&self) -> engine_stack::Result<Vec<ChangesetId>> {
        Ok(self.trunk_info().into_iter().map(|t| t.tip).collect())
    }

    async fn immutable_tips(&self) -> engine_stack::Result<Vec<ChangesetId>> {
        Ok(self
            .refs
            .iter()
            .filter(|(k, _)| self.immutable.iter().any(|p| glob(p.as_bytes(), k.as_bytes())))
            .map(|(_, cs)| *cs)
            .collect())
    }

    async fn heads(&self) -> engine_stack::Result<Vec<ChangesetId>> {
        let mut out: Vec<ChangesetId> = self
            .refs
            .iter()
            .filter(|(k, _)| k.starts_with("refs/heads/") || k.starts_with("refs/asc/stack/"))
            .map(|(_, cs)| *cs)
            .collect();
        out.extend(self.ws_heads.iter().copied());
        Ok(out)
    }

    async fn current_head(&self) -> engine_stack::Result<Option<ChangesetId>> {
        Ok(self.current)
    }

    fn user_email(&self) -> Option<Vec<u8>> {
        self.email.clone()
    }

    async fn author_email(&self, cs: ChangesetId) -> engine_stack::Result<Vec<u8>> {
        let c = self.ws.engine.get_changeset(cs).await.map_err(env_err)?;
        Ok(c.author.email().to_vec())
    }

    async fn landed(&self, _change: ChangeId) -> engine_stack::Result<bool> {
        // `landed_as` and `land_map` arrive with the server units; a local repository has none.
        Ok(false)
    }

    async fn resolve_git(&self, spec: &str) -> engine_stack::Result<Vec<ChangesetId>> {
        let found = self.ws.engine.resolve(&[spec.to_owned()]).await.map_err(env_err)?;
        Ok(found.into_iter().flatten().collect())
    }
}

pub(crate) type Src<'a> = EngineSource<'a, ObjectStack, ReadOnlyRefs<SqlRefs>>;
pub(crate) type LStacks<'l, 'a> = Stacks<'l, Src<'a>, NoJump, StateChanges, Env<'a>>;

/// The local derivation context of one invocation.
pub(crate) struct Local<'a> {
    pub ws: &'a WsCtx,
    pub graph: Graph<Src<'a>>,
    pub rows: StateChanges,
    pub env: Env<'a>,
}

impl<'a> Local<'a> {
    pub async fn open(ctx: &'a Ctx) -> Result<Self> {
        let ws = ctx.ws()?;
        Ok(Self {
            ws,
            graph: Graph::new(EngineSource::new(&ws.engine)),
            rows: crate::stack::change_rows(ctx)?,
            env: Env::load(ctx, ws).await?,
        })
    }

    pub async fn stacks(&self) -> Result<LStacks<'_, 'a>> {
        Stacks::load(&self.graph, &self.rows, &self.env).await.map_err(stack_err)
    }

    pub async fn git_id(&self, cs: ChangesetId) -> Result<String> {
        Ok(self.ws.engine.git_commit_of(cs).await.map_err(fatal)?.to_string())
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn subject_of(message: &[u8]) -> String {
    let line = message.split(|&b| b == b'\n').next().unwrap_or_default();
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let mut s = lossy(line);
    if s.len() > SUBJECT_MAX {
        let mut end = SUBJECT_MAX;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// The `--json` row of one draft change.
pub(crate) async fn change_out(l: &Local<'_>, st: &LStacks<'_, '_>, cs: ChangesetId, row: &ChangeRow) -> Result<ChangeOut> {
    let c = l.ws.engine.get_changeset(cs).await.map_err(fatal)?;
    let state = if st.is_obsolete(cs).await.map_err(stack_err)? {
        ChangeState::Obsolete
    } else if l.env.landed(row.change).await.map_err(stack_err)? {
        ChangeState::Landed
    } else if st.is_divergent(row.change).await.map_err(stack_err)? {
        ChangeState::Divergent
    } else {
        ChangeState::Draft
    };
    Ok(ChangeOut {
        change: row.change.to_string(),
        short: row.change.short(),
        commit: l.git_id(cs).await?,
        asc: cs.to_string(),
        parent_change: st.parent_change(cs).await.map_err(stack_err)?.map(|p| p.to_string()),
        branches: l.env.branches_at(cs),
        subject: subject_of(&c.message),
        author: Signature {
            name: lossy(c.author.name()),
            email: lossy(c.author.email()),
            time: c.author.time(),
            tz: json::tz(i32::from(c.author.tz_min())),
        },
        state,
        needs_restack: st.needs_restack(cs).await.map_err(stack_err)?,
        frozen: l.rows.is_frozen(row.change).await.map_err(fatal)?,
        empty: c.changes.is_empty(),
        current: l.env.current == Some(cs),
        worktree: l.env.worktree_at(cs),
        review: None,
    })
}

async fn behind_count(l: &Local<'_>, trunk: ChangesetId, base: Option<ChangesetId>) -> Result<u32> {
    let q = LogQuery {
        include: vec![trunk],
        exclude: base.into_iter().collect(),
        limit: Some(BEHIND_CAP + 1),
        ..LogQuery::default()
    };
    let ids: Vec<ChangesetId> = l.graph.log(q).try_collect().await.map_err(fatal)?;
    Ok(ids.len().min(BEHIND_CAP) as u32)
}

/// Builds `Workspace.Stacks` for these options.
async fn build(l: &Local<'_>, st: &LStacks<'_, '_>, a: &Args) -> Result<Sl> {
    let trunk_info = l.env.trunk_info();
    let mut trunks = Vec::new();
    for t in &trunk_info {
        trunks.push(Trunk { name: t.name.clone(), tip: l.git_id(t.tip).await?, remote: t.remote.clone() });
    }
    let filter = match &a.revset {
        Some(r) => Some(engine_stack::revset::eval_str(st, r).await.map_err(stack_err)?),
        None => None,
    };
    let heads: Vec<ChangesetId> = if a.stack { st.current_head().into_iter().collect() } else { st.heads().to_vec() };
    let mut found: Vec<(ChangesetId, Stack)> = Vec::new();
    let mut total = 0usize;
    for h in heads {
        let s = st.stack_of(h).await.map_err(stack_err)?;
        if s.changes.is_empty() {
            continue;
        }
        total += s.changes.len();
        if total > MAX_CHANGES {
            return Err(fatal(format!("more than {MAX_CHANGES} draft changes")));
        }
        found.push((h, s));
    }
    // A stack that is the lower part of another is that stack's own prefix: list it once.
    let covered: HashSet<ChangesetId> =
        found.iter().flat_map(|(_, s)| s.changes.iter().take(s.changes.len() - 1).copied()).collect();
    found.retain(|(h, _)| !covered.contains(h));
    found.sort_by(|a, b| a.1.changes.cmp(&b.1.changes));
    let mut behind_cache: HashMap<(Option<ChangesetId>, ChangesetId), u32> = HashMap::new();
    let mut stacks = Vec::new();
    for (_, s) in found {
        let mut changes = Vec::new();
        for &cs in &s.changes {
            if filter.as_ref().is_some_and(|f| !f.contains(&cs)) {
                continue;
            }
            let Some(row) = st.draft_row(cs).await.map_err(stack_err)? else { continue };
            let out = change_out(l, st, cs, &row).await?;
            if !a.all && matches!(out.state, ChangeState::Landed | ChangeState::Obsolete) {
                continue;
            }
            changes.push(out);
        }
        if changes.is_empty() {
            continue;
        }
        let trunk = s
            .trunk
            .and_then(|t| trunk_info.iter().find(|i| i.tip == t))
            .map(|i| i.name.clone())
            .unwrap_or_default();
        let base = match s.base {
            Some(b) => l.git_id(b).await?,
            None => "0".repeat(40),
        };
        let behind = match (s.behind, s.trunk) {
            (true, Some(t)) => match behind_cache.get(&(s.base, t)) {
                Some(&n) => n,
                None => {
                    let n = behind_count(l, t, s.base).await?;
                    behind_cache.insert((s.base, t), n);
                    n
                }
            },
            _ => 0,
        };
        stacks.push(StackOut { trunk, base, behind, changes });
    }
    Ok(Sl::new(trunks, stacks))
}

/// The human form: each stack top to bottom, `@` marking the current change.
fn render(sl: &Sl) -> String {
    let mut out = String::new();
    for (i, s) in sl.stacks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for c in s.changes.iter().rev() {
            out.push_str(if c.current { "@  " } else { "o  " });
            out.push_str(&c.short);
            out.push(' ');
            out.push_str(&clean(&c.subject));
            if !c.branches.is_empty() {
                let names: Vec<String> = c.branches.iter().map(|b| clean(b)).collect();
                out.push_str(&format!(" ({})", names.join(", ")));
            }
            let mut flags: Vec<String> = Vec::new();
            match c.state {
                ChangeState::Draft => {}
                ChangeState::Landed => flags.push("landed".into()),
                ChangeState::Obsolete => flags.push("obsolete".into()),
                ChangeState::Divergent => flags.push("divergent".into()),
            }
            if c.needs_restack {
                flags.push("needs restack".into());
            }
            if c.frozen {
                flags.push("frozen".into());
            }
            if c.empty {
                flags.push("empty".into());
            }
            if let Some(w) = &c.worktree {
                flags.push(format!("worktree {}", clean(w)));
            }
            for f in flags {
                out.push_str(&format!(" [{f}]"));
            }
            out.push('\n');
        }
        let trunk = if s.trunk.is_empty() { "trunk".to_owned() } else { clean(&s.trunk) };
        let base: String = s.base.chars().take(7).collect();
        out.push_str(&format!("~  on {trunk} {base}"));
        if s.behind > 0 {
            out.push_str(&format!(" ({} behind)", s.behind));
        }
        out.push('\n');
    }
    out
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let st = l.stacks().await?;
    if a.remote {
        eprintln!("asc: --remote: no review data is available from this repository; showing local state");
    }
    let sl = build(&l, &st, &a).await?;
    let mut out = Output::open(ctx, "sl", PagerWant::Never)?;
    if a.json {
        out.write_all(json::line(&sl).map_err(fatal)?.as_bytes())?;
    } else {
        out.write_all(render(&sl).as_bytes())?;
    }
    out.finish()?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_stars_and_literals() {
        assert!(glob(b"refs/remotes/*/release/*", b"refs/remotes/origin/release/1.x"));
        assert!(!glob(b"refs/remotes/*/release/*", b"refs/remotes/origin/master"));
        assert!(glob(b"*", b""));
        assert!(glob(b"a*b*c", b"aXXbYYc"));
        assert!(!glob(b"a*b*c", b"aXXbYY"));
        assert!(!glob(b"abc", b"abcd"));
    }

    #[test]
    fn clean_removes_escapes() {
        assert_eq!(clean("a\u{1b}[31mb"), "a?[31mb");
    }

    #[test]
    fn subject_is_the_first_line_bounded() {
        assert_eq!(subject_of(b"one\r\ntwo"), "one");
        assert_eq!(subject_of(&vec![b'x'; 5000]).len(), SUBJECT_MAX);
    }
}
