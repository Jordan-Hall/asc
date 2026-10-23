//! `rev-parse` (SPEC §11.2, §11.5): revisions to ids, plus the repository-location queries
//! tools ask (`--show-toplevel`, `--git-dir`, `--is-inside-work-tree`, …).
//!
//! Arguments are handled in order, as git does: a flag such as `--short` or `--asc` applies to
//! the revisions after it. `--git`/`--asc` choose which id kind prints (SPEC §11.1). The helpers
//! [`commit_of`] and [`ref_of`] are shared with the other rev commands.

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use asc_core::git::{ObjectKind, export_tag, object_oid, tree_oid};
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TagId, TreeId};
use asc_core::object::Kind;
use asc_core::path::{RefName, RepoPath};
use asc_engine::history::HistoryApi as _;
use asc_engine::revspec::dwim_candidates;
use asc_git::projection::Head;
use asc_store::refs::Refs;

use super::add::cwd_prefix;
use super::common::{CliError, Ctx, Result, WsCtx};
use super::log::refs;
use super::log::walk::Hist;
use super::output::Output;
use super::pager::PagerWant;
use super::restore::tool;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "rev-parse",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    /// Flags and revisions, in git's order-sensitive grammar.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

/// The flags `rev-parse` serves. `--git` and `--asc` are asc's additions (SPEC §11.2).
const FLAGS: &[&str] = &[
    "--git",
    "--asc",
    "--show-toplevel",
    "--git-dir",
    "--absolute-git-dir",
    "--git-common-dir",
    "--abbrev-ref",
    "--verify",
    "-q",
    "--quiet",
    "--short",
    "--is-inside-work-tree",
    "--is-inside-git-dir",
    "--is-bare-repository",
    "--is-shallow-repository",
    "--show-prefix",
    "--show-cdup",
    "--show-object-format",
    "--symbolic",
    "--symbolic-full-name",
    "--all",
    "--branches",
    "--tags",
    "--remotes",
    "--end-of-options",
];

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "rev-parse",
    flags: FLAGS,
    placeholders: &[],
}];

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

// ------------------------------------------------------------------------------ resolution

/// Whether `s` is a full git object id.
fn is_oid(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `name@{inner}rest` split into its three parts.
fn brace(word: &str) -> Option<(&str, &str, &str)> {
    let at = word.find("@{")?;
    let end = at + word[at..].find('}')?;
    Some((&word[..at], &word[at + 2..end], &word[end + 1..]))
}

/// `branch`'s upstream as a ref name (`refs/remotes/origin/x`), from `branch.<b>.remote`/`.merge`.
fn upstream_of(ctx: &Ctx, ws: &WsCtx, name: &str) -> Result<String> {
    let branch = if name.is_empty() || name == "HEAD" {
        match ws.projection.read_head()? {
            Head::Branch(b) => b,
            Head::Detached(_) => {
                return Err(CliError::Fatal("HEAD does not point to a branch".into()));
            }
        }
    } else {
        name.to_owned()
    };
    let none = || CliError::Fatal(format!("no upstream configured for branch '{branch}'"));
    let remote = ctx
        .config
        .get(&format!("branch.{branch}.remote"))
        .ok_or_else(none)?;
    let merge = ctx
        .config
        .get(&format!("branch.{branch}.merge"))
        .ok_or_else(none)?;
    if remote == "." {
        return Ok(merge);
    }
    let short = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
    Ok(format!("refs/remotes/{remote}/{short}"))
}

/// The attached branch's full ref name for `HEAD@{n}`; `None` when detached.
fn head_branch(ws: &WsCtx) -> Result<Option<String>> {
    Ok(match ws.projection.read_head()? {
        Head::Branch(b) => Some(format!("refs/heads/{b}")),
        Head::Detached(_) => None,
    })
}

/// Rewrites what the engine's resolver cannot see: a full git id, `@{upstream}`, and `HEAD@{n}`.
async fn expand(ctx: &Ctx, ws: &WsCtx, word: &str) -> Result<String> {
    if let Some((name, inner, rest)) = brace(word) {
        if matches!(inner, "u" | "upstream") {
            return Ok(format!("{}{rest}", upstream_of(ctx, ws, name)?));
        }
        if matches!(name, "" | "HEAD" | "@")
            && inner.bytes().all(|b| b.is_ascii_digit())
            && let Some(full) = head_branch(ws)?
        {
            return Ok(format!("{full}@{{{inner}}}{rest}"));
        }
    }
    let lead = word.get(..40).filter(|l| is_oid(l));
    if let Some(lead) =
        lead.filter(|_| word.len() == 40 || matches!(word.as_bytes()[40], b'~' | b'^'))
        && let Some(asc) = git_object(ws, lead, ObjectKind::Commit).await?
    {
        return Ok(format!("{}{}", ChangesetId::from_bytes(asc), &word[40..]));
    }
    Ok(word.to_owned())
}

/// The asc id of the git object `hex` of `kind`, from the git map.
async fn git_object(ws: &WsCtx, hex: &str, kind: ObjectKind) -> Result<Option<[u8; 32]>> {
    let Ok(oid) = hex.parse::<GitOid>() else {
        return Ok(None);
    };
    Ok(asc_state::state::git_map::git_to_asc(&ws.db, ws.repo.key, oid, kind).await?)
}

/// `word` as a commit; `None` when it names none. Beyond the engine's revspecs, this accepts a
/// full git id, `<branch>@{upstream}` and `HEAD@{n}`.
pub(super) async fn commit_of(ctx: &Ctx, word: &str) -> Result<Option<ChangesetId>> {
    let ws = ctx.ws()?;
    let word = expand(ctx, ws, word).await?;
    // `HEAD` and `@` name the workspace's HEAD, which the engine's refs do not hold.
    let head_len = if word.starts_with("HEAD") {
        4
    } else {
        usize::from(word.starts_with('@'))
    };
    let spec = match (head_len, word.as_bytes().get(head_len)) {
        (0, _) => word,
        (_, None | Some(b'~' | b'^')) => match ws.head_commit().await? {
            Some(h) => format!("{h}{}", &word[head_len..]),
            None => return Ok(None),
        },
        _ => word,
    };
    match ws.engine.resolve(&[spec]).await {
        Ok(mut found) => Ok(found.pop().flatten()),
        // A revision that does not parse or exist (`HEAD~99`, `^2` of a non-merge) is the
        // engine's `Remote` error for the revspec failures: here that only means "not a revision".
        Err(
            asc_engine::graph::Error::Engine(_)
            | asc_engine::graph::Error::Missing(_)
            | asc_engine::graph::Error::Remote(_),
        ) => Ok(None),
        Err(e) => Err(fatal(e)),
    }
}

/// The full name of the ref `name` denotes (`HEAD` gives its branch, or `HEAD` when detached),
/// by git's DWIM order; `None` when it is no ref.
pub(super) async fn ref_of(ws: &WsCtx, name: &str) -> Result<Option<String>> {
    if matches!(name, "HEAD" | "@") {
        return Ok(Some(head_branch(ws)?.unwrap_or_else(|| "HEAD".into())));
    }
    for cand in dwim_candidates(name) {
        if has_ref(ws, &cand).await? {
            return Ok(Some(cand));
        }
    }
    Ok(None)
}

async fn has_ref(ws: &WsCtx, full: &str) -> Result<bool> {
    let Ok(name) = RefName::new(full.as_bytes().to_vec()) else {
        return Ok(false);
    };
    Ok(ws.db.refs().get(ws.repo.key, &name).await?.is_some())
}

/// git's strict `--abbrev-ref`: the shortest form of `full` no earlier lookup rule would
/// resolve differently.
async fn short_ref(ws: &WsCtx, full: &str) -> Result<String> {
    let rules: [(&str, &str); 6] = [
        ("", ""),
        ("refs/", ""),
        ("refs/tags/", ""),
        ("refs/heads/", ""),
        ("refs/remotes/", ""),
        ("refs/remotes/", "/HEAD"),
    ];
    for i in (0..rules.len()).rev() {
        let (pre, post) = rules[i];
        let Some(short) = full
            .strip_prefix(pre)
            .and_then(|r| r.strip_suffix(post))
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let mut ambiguous = false;
        for (p, q) in &rules[..i] {
            ambiguous = ambiguous || has_ref(ws, &format!("{p}{short}{q}")).await?;
        }
        if !ambiguous {
            return Ok(short.to_owned());
        }
    }
    Ok(full.to_owned())
}

/// The annotated tag a plain name denotes, if it is one.
async fn annotated_tag(ws: &WsCtx, name: &str) -> Result<Option<TagId>> {
    for cand in dwim_candidates(name) {
        let Ok(rn) = RefName::new(cand.into_bytes()) else {
            continue;
        };
        if let Some(e) = ws.db.refs().get(ws.repo.key, &rn).await? {
            return Ok(match e.target {
                RefTarget::Tag(t) => Some(t),
                RefTarget::Changeset(_) => None,
            });
        }
    }
    Ok(None)
}

/// What a revision word denotes.
#[derive(Clone, Debug)]
enum Obj {
    Commit(ChangesetId),
    Tag(TagId),
    Tree(TreeId),
    /// A path of a tree: its kind, asc id and git id.
    Entry(Kind, [u8; 32], GitOid),
    /// A git object only the git map knows.
    Git(GitOid),
}

impl Obj {
    async fn hex(&self, ws: &WsCtx, asc: bool) -> Result<String> {
        if asc {
            return match self {
                Self::Commit(c) => Ok(c.to_string()),
                Self::Tag(t) => Ok(t.to_string()),
                Self::Tree(t) => Ok(t.to_string()),
                Self::Entry(Kind::Dir, id, _) => Ok(TreeId::from_bytes(*id).to_string()),
                Self::Entry(Kind::Gitlink, ..) | Self::Git(_) => {
                    Err(CliError::Fatal("this object has no asc id".into()))
                }
                Self::Entry(_, id, _) => Ok(FileId::from_bytes(*id).to_string()),
            };
        }
        Ok(match self {
            Self::Commit(c) => ws
                .engine
                .git_commit_of(*c)
                .await
                .map_err(fatal)?
                .to_string(),
            Self::Tag(t) => {
                let rec = refs::tag_record(ws, *t).await?;
                let target = ws.engine.git_commit_of(rec.target).await.map_err(fatal)?;
                let raw = export_tag(&rec, target).map_err(fatal)?;
                object_oid(ObjectKind::Tag, &raw).to_string()
            }
            Self::Tree(t) => tree_oid(&*ws.engine.get_tree(*t).await?).to_string(),
            Self::Entry(_, _, git) | Self::Git(git) => git.to_string(),
        })
    }
}

/// One printed revision: whether it is excluded (`^id`), the object and the name it was typed as.
struct Item {
    hidden: bool,
    obj: Obj,
    name: String,
}

fn item(hidden: bool, obj: Obj, name: &str) -> Item {
    Item {
        hidden,
        obj,
        name: name.to_owned(),
    }
}

/// The objects and ranges a word stands for.
struct Resolver<'a> {
    ctx: &'a Ctx,
    ws: &'a WsCtx,
}

impl Resolver<'_> {
    /// `word` as one object, with `^{tree}`/`^{commit}`/`^{}` and `<rev>:<path>`.
    async fn object(&self, word: &str) -> Result<Option<Obj>> {
        if let Some(at) = word
            .find(':')
            .filter(|&at| at > 0 && !word[..at].contains("@{"))
        {
            return self.path_object(&word[..at], &word[at + 1..]).await;
        }
        let (base, tree) = match word.strip_suffix("^{tree}") {
            Some(b) => (b, true),
            None => (
                word.strip_suffix("^{commit}")
                    .or_else(|| word.strip_suffix("^{}"))
                    .unwrap_or(word),
                false,
            ),
        };
        let plain = base == word && !word.contains(['^', '~']) && !word.contains("@{");
        if plain && let Some(tag) = annotated_tag(self.ws, base).await? {
            return Ok(Some(Obj::Tag(tag)));
        }
        let Some(commit) = commit_of(self.ctx, base).await? else {
            return self.git_only(word).await;
        };
        Ok(Some(if tree {
            Obj::Tree(self.ws.engine.get_changeset(commit).await?.root)
        } else {
            Obj::Commit(commit)
        }))
    }

    /// A full git id that is not a commit (a tree, blob or tag known to the git map).
    async fn git_only(&self, word: &str) -> Result<Option<Obj>> {
        if !is_oid(word) {
            return Ok(None);
        }
        for kind in [ObjectKind::Tree, ObjectKind::Blob, ObjectKind::Tag] {
            if git_object(self.ws, word, kind).await?.is_some() {
                return Ok(word.parse().ok().map(Obj::Git));
            }
        }
        Ok(None)
    }

    async fn path_object(&self, rev: &str, path: &str) -> Result<Option<Obj>> {
        let Some(commit) = commit_of(self.ctx, rev).await? else {
            return Ok(None);
        };
        let root = self.ws.engine.get_changeset(commit).await?.root;
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            return Ok(Some(Obj::Tree(root)));
        }
        let Ok(repo_path) = RepoPath::new(path.as_bytes().to_vec()) else {
            return Ok(None);
        };
        Ok(self
            .ws
            .engine
            .lookup(root, &repo_path)
            .await?
            .map(|e| Obj::Entry(e.kind, e.id, e.git)))
    }

    async fn commit(&self, word: &str) -> Result<Option<ChangesetId>> {
        commit_of(self.ctx, word).await
    }

    /// The items `word` prints, or `None` when it names no revision.
    async fn items(&self, word: &str) -> Result<Option<Vec<Item>>> {
        if let Some(rest) = word.strip_prefix('^') {
            return Ok(self.object(rest).await?.map(|o| vec![item(true, o, rest)]));
        }
        if !word.contains(':') {
            if let Some((a, b)) = word.split_once("...") {
                return self.symmetric(a, b).await;
            }
            if let Some((a, b)) = word.split_once("..") {
                let (a, b) = (
                    if a.is_empty() { "HEAD" } else { a },
                    if b.is_empty() { "HEAD" } else { b },
                );
                let (Some(x), Some(y)) = (self.object(a).await?, self.object(b).await?) else {
                    return Ok(None);
                };
                return Ok(Some(vec![item(false, y, b), item(true, x, a)]));
            }
            if let Some(base) = word.strip_suffix("^@") {
                let Some(c) = self.commit(base).await? else {
                    return Ok(None);
                };
                let parents = self.ws.engine.get_changeset(c).await?.parents.clone();
                return Ok(Some(
                    parents
                        .into_iter()
                        .map(|p| item(false, Obj::Commit(p), word))
                        .collect(),
                ));
            }
            if let Some(base) = word.strip_suffix("^!") {
                let Some(c) = self.commit(base).await? else {
                    return Ok(None);
                };
                let parents = self.ws.engine.get_changeset(c).await?.parents.clone();
                let mut out = vec![item(false, Obj::Commit(c), base)];
                out.extend(
                    parents
                        .into_iter()
                        .map(|p| item(true, Obj::Commit(p), base)),
                );
                return Ok(Some(out));
            }
        }
        Ok(self.object(word).await?.map(|o| vec![item(false, o, word)]))
    }

    async fn symmetric(&self, a: &str, b: &str) -> Result<Option<Vec<Item>>> {
        let (a, b) = (
            if a.is_empty() { "HEAD" } else { a },
            if b.is_empty() { "HEAD" } else { b },
        );
        let (Some(x), Some(y)) = (self.commit(a).await?, self.commit(b).await?) else {
            return Ok(None);
        };
        let hist = Hist::select(self.ws);
        let bases = hist
            .api(self.ws)
            .merge_base(x, &[y], true)
            .await
            .map_err(fatal)?;
        let mut out = vec![
            item(false, Obj::Commit(y), b),
            item(false, Obj::Commit(x), a),
        ];
        out.extend(
            bases
                .into_iter()
                .map(|c| item(true, Obj::Commit(c), "merge-base")),
        );
        Ok(Some(out))
    }
}

// ----------------------------------------------------------------------------------- output

/// How a revision prints.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Show {
    Id,
    /// `--abbrev-ref`: the short name of the ref, nothing for a non-ref.
    ShortRef,
    /// `--symbolic-full-name`.
    FullRef,
    /// `--symbolic`: as typed.
    Symbolic,
}

struct Printer<'a> {
    ctx: &'a Ctx,
    ws: &'a WsCtx,
    asc: bool,
    show: Show,
    short: Option<usize>,
}

impl Printer<'_> {
    /// `core.abbrev`, clamped to git's range, else 7.
    fn default_abbrev(&self) -> usize {
        self.ctx
            .config
            .get("core.abbrev")
            .and_then(|v| v.parse::<usize>().ok())
            .map_or(7, |n| n.clamp(4, 40))
    }

    async fn line(&self, it: &Item) -> Result<Option<String>> {
        let text = match self.show {
            Show::Id => {
                let hex = it.obj.hex(self.ws, self.asc).await?;
                match self.short {
                    Some(n) => hex[..n.min(hex.len())].to_owned(),
                    None => hex,
                }
            }
            Show::Symbolic => it.name.clone(),
            Show::ShortRef | Show::FullRef => {
                let Some(full) = ref_of(self.ws, &it.name).await? else {
                    return Ok(None);
                };
                if self.show == Show::FullRef || full == "HEAD" {
                    full
                } else {
                    short_ref(self.ws, &full).await?
                }
            }
        };
        Ok(Some(if it.hidden { format!("^{text}") } else { text }))
    }

    async fn print(&self, out: &mut Output, items: &[Item]) -> Result<()> {
        for it in items {
            if let Some(line) = self.line(it).await? {
                writeln!(out, "{line}")?;
            }
        }
        Ok(())
    }
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

/// A path with forward slashes and without the Windows verbatim prefix, as git prints it.
fn display(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    s.strip_prefix("//?/").map_or(s.clone(), str::to_owned)
}

fn inside(ctx: &Ctx, dir: &Path) -> bool {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
    canon(&ctx.cwd).starts_with(canon(dir))
}

/// Prints the location queries; `true` when `arg` was one.
fn location(ctx: &Ctx, ws: &WsCtx, arg: &str, out: &mut Output) -> Result<bool> {
    let p = &ws.projection;
    let text = match arg {
        "--show-toplevel" => display(p.toplevel()),
        "--git-dir" | "--absolute-git-dir" => display(p.git_dir()),
        "--git-common-dir" => display(p.common_dir()),
        "--is-inside-work-tree" => {
            (inside(ctx, p.toplevel()) && !inside(ctx, p.git_dir())).to_string()
        }
        "--is-inside-git-dir" => inside(ctx, p.git_dir()).to_string(),
        "--is-bare-repository" | "--is-shallow-repository" => "false".into(),
        "--show-object-format" => "sha1".into(),
        "--show-prefix" => {
            let prefix = cwd_prefix(ctx, ws);
            if prefix.is_empty() {
                prefix
            } else {
                format!("{prefix}/")
            }
        }
        "--show-cdup" => {
            let prefix = cwd_prefix(ctx, ws);
            "../".repeat(prefix.split('/').filter(|s| !s.is_empty()).count())
        }
        _ => return Ok(false),
    };
    writeln!(out, "{text}")?;
    Ok(true)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let args: Vec<String> = a
        .rest
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let mut out = Output::open(ctx, "rev-parse", PagerWant::Never)?;
    let result = parse(ctx, &args, &mut out).await;
    let flushed = out.finish();
    let result = match result {
        Err(CliError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        other => other,
    };
    flushed?;
    result
}

async fn parse(ctx: &Ctx, args: &[String], out: &mut Output) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let verify = args.iter().any(|a| a == "--verify");
    let quiet = args.iter().any(|a| a == "-q" || a == "--quiet");
    let resolver = Resolver { ctx, ws };
    let mut p = Printer {
        ctx,
        ws,
        asc: false,
        show: Show::Id,
        short: None,
    };
    let mut single: Option<Vec<Item>> = None;
    let mut revs = 0usize;
    let (mut dashdash, mut literal) = (false, false);
    for arg in args {
        if dashdash {
            writeln!(out, "{arg}")?;
            continue;
        }
        if !literal && arg.len() > 1 && arg.starts_with('-') {
            if arg == "--" {
                writeln!(out, "--")?;
                dashdash = true;
            } else if arg == "--end-of-options" {
                // git echoes it (outside `--verify`), and takes the next word as a revision.
                if !verify {
                    writeln!(out, "{arg}")?;
                }
                literal = true;
            } else if !flag(ctx, ws, &mut p, arg, out).await? {
                writeln!(out, "{arg}")?;
            }
            continue;
        }
        literal = false;
        match resolver.items(arg).await? {
            Some(items) if verify && items.len() == 1 => {
                revs += 1;
                single = Some(items);
            }
            Some(items) if verify => {
                p.print(out, &items).await?;
                revs += 2;
            }
            Some(items) => p.print(out, &items).await?,
            None if verify => {}
            None if ctx.cwd.join(arg).exists() => writeln!(out, "{arg}")?,
            None => {
                writeln!(out, "{arg}")?;
                return Err(ambiguous(ctx, arg));
            }
        }
    }
    if verify {
        match (revs, single) {
            (1, Some(items)) => p.print(out, &items).await?,
            _ if quiet => return Ok(ExitCode::from(1)),
            _ => return Err(CliError::Fatal("Needed a single revision".into())),
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Applies or prints one flag; `false` for a flag git would echo back.
async fn flag(
    ctx: &Ctx,
    ws: &WsCtx,
    p: &mut Printer<'_>,
    arg: &str,
    out: &mut Output,
) -> Result<bool> {
    let (name, value) = arg
        .split_once('=')
        .map_or((arg, None), |(n, v)| (n, Some(v)));
    match name {
        "--git" => p.asc = false,
        "--asc" => p.asc = true,
        "--verify" | "-q" | "--quiet" => {}
        "--short" => {
            let n = match value {
                Some(v) => v
                    .parse::<usize>()
                    .map_err(|_| fatal(format!("'{v}': not an integer")))?
                    .clamp(4, 40),
                None if p.asc => 12,
                None => p.default_abbrev(),
            };
            p.short = Some(n);
        }
        "--abbrev-ref" => p.show = Show::ShortRef,
        "--symbolic-full-name" => p.show = Show::FullRef,
        "--symbolic" => p.show = Show::Symbolic,
        "--all" | "--branches" | "--tags" | "--remotes" => {
            let prefix = match name {
                "--branches" => "refs/heads/",
                "--tags" => "refs/tags/",
                "--remotes" => "refs/remotes/",
                _ => "refs/",
            };
            for tip in refs::tips(ws, prefix).await? {
                let obj = tip.tag.map_or(Obj::Commit(tip.commit), Obj::Tag);
                p.print(out, &[item(false, obj, &tip.name)]).await?;
            }
        }
        _ => return location(ctx, ws, name, out),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_needs_forty_lower_hex() {
        assert!(is_oid(&"a1".repeat(20)));
        assert!(!is_oid(&"A1".repeat(20)));
        assert!(!is_oid("abcd"));
    }

    #[test]
    fn brace_splits_around_the_selector() {
        assert_eq!(brace("master@{u}"), Some(("master", "u", "")));
        assert_eq!(brace("@{0}~1"), Some(("", "0", "~1")));
        assert_eq!(brace("master"), None);
    }

    #[test]
    fn windows_paths_print_like_git() {
        assert_eq!(display(Path::new(r"C:\work\repo")), "C:/work/repo");
        assert_eq!(display(Path::new(r"\\?\C:\work")), "C:/work");
    }
}
