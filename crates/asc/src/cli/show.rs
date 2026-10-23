//! `show` (SPEC §11.2): commits (message and patch), annotated tags, and `<rev>:<path>` blobs and
//! trees. Output flags are `log`'s, shared through [`super::log`].

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use asc_core::id::{ChangesetId, RefTarget, TagId};
use asc_core::object::Kind;
use asc_core::path::{RefName, RepoPath};
use asc_store::refs::Refs;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::format::date::Mode;
use super::log::emit::{Cmd, Printer};
use super::log::opts::{Opts, Tok};
use super::log::revs::{self, Selection};
use super::log::walk::{Hist, Walker};
use super::log::{OUTPUT_FLAGS, PLACEHOLDERS, load, refs};
use super::output::Output;
use super::pager::PagerWant;
use super::restore::{resolve_commit, tool};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "show", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Flags, objects and pathspecs, in git's grammar (parsed by [`Opts`]).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] =
    &[NativeSpec { cmd: "show", flags: OUTPUT_FLAGS, placeholders: PLACEHOLDERS }];

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = Opts::parse(&a.rest)?;
    match show(ctx, o).await {
        Err(CliError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        other => other,
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

/// The annotated tag a word names, if it is one.
async fn tag_of(ws: &WsCtx, word: &str) -> Result<Option<TagId>> {
    for cand in [word.to_owned(), format!("refs/{word}"), format!("refs/tags/{word}")] {
        let Ok(name) = RefName::new(cand.into_bytes()) else { continue };
        if let Some(e) = ws.db.refs().get(ws.repo.key, &name).await? {
            return Ok(match e.target {
                RefTarget::Tag(t) => Some(t),
                RefTarget::Changeset(_) => None,
            });
        }
    }
    Ok(None)
}

async fn show(ctx: &Ctx, o: Opts) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let hist = Hist::select(ws);
    let api = hist.api(ws);
    let mut words: Vec<String> = o
        .revs
        .iter()
        .filter_map(|t| match t {
            Tok::Rev(w) => Some(w.clone()),
            _ => None,
        })
        .collect();
    if words.is_empty() {
        words.push("HEAD".into());
    }
    let path_sel: Selection = revs::paths_only(ctx, &o.paths)?;
    let mut out = Output::open(ctx, "show", PagerWant::Auto)?;
    let mut printer = Printer::new(ctx, ws, api, &o, &path_sel, Cmd::Show, out.color()).await?;
    let mut result = Ok(());
    for word in &words {
        result = one(ctx, ws, &hist, &o, &mut printer, &mut out, word).await;
        if result.is_err() {
            break;
        }
    }
    let flushed = out.finish();
    result?;
    flushed?;
    Ok(ExitCode::SUCCESS)
}

async fn one(
    ctx: &Ctx,
    ws: &WsCtx,
    hist: &Hist,
    o: &Opts,
    printer: &mut Printer<'_>,
    out: &mut Output,
    word: &str,
) -> Result<()> {
    if let Some((rev, path)) = word.split_once(':').filter(|(r, _)| !r.is_empty()) {
        let Some(commit) = resolve_commit(ctx, rev).await? else {
            return Err(ambiguous(ctx, word));
        };
        return tree_entry(ws, out, word, commit, rev, path).await;
    }
    if word.contains("..") {
        let ranged = Opts { revs: vec![Tok::Rev(word.to_owned())], ..o.clone() };
        let sel = revs::select(ctx, hist.api(ws), &ranged).await?;
        let mut walker = Walker::new(ws, hist, &sel, &ranged).await?;
        while let Some(e) = walker.next().await? {
            printer.print(out, &e, None).await?;
        }
        return Ok(());
    }
    if let Some(tag) = tag_of(ws, word).await? {
        show_tag(printer, out, ws, tag).await?;
    }
    match resolve_commit(ctx, word).await? {
        Some(id) => {
            let e = load(ws, id).await?;
            printer.print(out, &e, None).await
        }
        None => Err(ambiguous(ctx, word)),
    }
}

/// An annotated tag's own header and message.
async fn show_tag(printer: &mut Printer<'_>, out: &mut Output, ws: &WsCtx, id: TagId) -> Result<()> {
    let rec = refs::tag_record(ws, id).await?;
    let mut text: Vec<u8> = Vec::new();
    text.extend_from_slice(b"tag ");
    text.extend_from_slice(&rec.name);
    text.push(b'\n');
    if let Some(t) = &rec.tagger {
        text.extend_from_slice(b"Tagger: ");
        text.extend_from_slice(&[t.name(), b" <", t.email(), b">\n"].concat());
        let date = Mode::default().render(t.time(), t.tz_min(), 0);
        text.extend_from_slice(format!("Date:   {date}\n").as_bytes());
    }
    text.push(b'\n');
    text.extend_from_slice(&rec.message);
    if !rec.message.ends_with(b"\n") {
        text.push(b'\n');
    }
    out.write_all(&text)?;
    printer.mark_shown();
    Ok(())
}

/// `<rev>:<path>`: a blob's bytes, or a tree's listing.
async fn tree_entry(
    ws: &WsCtx,
    out: &mut Output,
    word: &str,
    commit: ChangesetId,
    rev: &str,
    path: &str,
) -> Result<()> {
    let cs = ws.engine.get_changeset(commit).await?;
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return list_tree(ws, out, word, cs.root).await;
    }
    let repo_path = RepoPath::new(trimmed.as_bytes().to_vec()).map_err(fatal)?;
    let Some(entry) = ws.engine.lookup(cs.root, &repo_path).await? else {
        return Err(CliError::Raw(format!("fatal: path '{path}' does not exist in '{rev}'"), 128));
    };
    match entry.kind {
        Kind::Dir => {
            let tree = ws.engine.get_tree(asc_core::id::TreeId::from_bytes(entry.id)).await?;
            list_names(out, word, tree.entries().iter().map(|e| (e.name.as_bytes(), e.kind == Kind::Dir)))
        }
        Kind::Gitlink => Err(fatal(format!("cannot show submodule entry '{path}'"))),
        _ => {
            let bytes = ws.engine.read_file(asc_core::id::FileId::from_bytes(entry.id), entry.size).await.map_err(fatal)?;
            out.write_all(&bytes)?;
            Ok(())
        }
    }
}

async fn list_tree(ws: &WsCtx, out: &mut Output, word: &str, root: asc_core::id::TreeId) -> Result<()> {
    let tree = ws.engine.get_tree(root).await?;
    list_names(out, word, tree.entries().iter().map(|e| (e.name.as_bytes(), e.kind == Kind::Dir)))
}

fn list_names<'a>(out: &mut Output, word: &str, names: impl Iterator<Item = (&'a [u8], bool)>) -> Result<()> {
    out.write_all(format!("tree {word}\n\n").as_bytes())?;
    for (name, dir) in names {
        out.write_all(name)?;
        if dir {
            out.write_all(b"/")?;
        }
        out.write_all(b"\n")?;
    }
    Ok(())
}
