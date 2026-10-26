//! Printing commits the way git's `show_log` does: the `commit` line, the pretty-formatted
//! message, the graph to the left of every line, and the diff underneath.

use std::collections::HashMap;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use asc_core::git::{EXTRA_GIT_COMMIT, tree_oid};
use asc_core::id::{ChangesetId, GitOid};
use asc_engine::history::HistoryApi;

use super::decorate;
use super::describe::Describer;
use super::diffout::DiffSetup;
use super::graph::Graph;
use super::opts::{ColorWhen, Decorate, Opts};
use super::pretty::{self, Builtin};
use super::revs::Selection;
use super::walk::Entry;
use crate::cli::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use crate::cli::format::decor::{Decor, Style};
use crate::cli::format::mailmap::Mailmap;
use crate::cli::format::{Commit, Env, Extras, Reflog, Template, date};

/// How the message of each commit is produced.
enum Kind {
    Builtin(Builtin),
    Template(Template),
}

/// Which command prints, which decides defaults.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Log,
    Show,
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

fn io(e: std::io::Error) -> CliError {
    CliError::Io(e)
}

/// What the `--pretty` word means: the format and whether entries are terminated (not separated).
fn pick_pretty(ctx: &Ctx, word: &str, depth: u8) -> Result<(Kind, bool)> {
    if let Some(f) = word.strip_prefix("format:") {
        return Ok((Kind::Template(Template::parse(f.as_bytes())), false));
    }
    if let Some(f) = word.strip_prefix("tformat:") {
        return Ok((Kind::Template(Template::parse(f.as_bytes())), true));
    }
    if word == "reference" {
        return Ok((
            Kind::Template(Template::parse(b"%C(auto)%h (%s, %ad)")),
            true,
        ));
    }
    if let Some(b) = Builtin::named(word) {
        return Ok((Kind::Builtin(b), b == Builtin::Oneline));
    }
    if depth < 8
        && let Some(alias) = ctx.config.get(&format!("pretty.{word}"))
    {
        return pick_pretty(ctx, &alias, depth + 1);
    }
    if word.contains('%') {
        return Ok((Kind::Template(Template::parse(word.as_bytes())), true));
    }
    Err(CliError::Raw(
        format!("fatal: invalid --pretty format: {word}"),
        128,
    ))
}

fn columns() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .filter(|&c| c > 0)
        .unwrap_or(80)
}

/// The `encoding` header of the raw git commit kept with the changeset, if any.
fn encoding_of(cs: &asc_core::object::Changeset) -> Option<Vec<u8>> {
    let raw = cs.extra.get(&EXTRA_GIT_COMMIT.to_vec())?;
    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            break;
        }
        if let Some(e) = line.strip_prefix(b"encoding ") {
            return Some(e.to_vec());
        }
    }
    None
}

/// Prints commits.
pub struct Printer<'a> {
    ws: &'a WsCtx,
    hist: &'a dyn HistoryApi,
    kind: Kind,
    terminator: bool,
    line_term: u8,
    abbrev_commit: bool,
    abbrev: usize,
    date: date::Mode,
    now: i64,
    color: bool,
    decorate: Decorate,
    decor: HashMap<ChangesetId, Decor>,
    graph: Option<Graph>,
    diff: DiffSetup,
    describer: Option<Describer>,
    mailmap: Option<Mailmap>,
    sources: HashMap<ChangesetId, String>,
    show_source: bool,
    left_right: bool,
    first_parent: bool,
    cmd: Cmd,
    format_empty: bool,
    shown_one: bool,
    /// The previous message did not end in a newline.
    last_open: bool,
}

impl<'a> Printer<'a> {
    pub async fn new(
        ctx: &'a Ctx,
        ws: &'a WsCtx,
        hist: &'a dyn HistoryApi,
        o: &Opts,
        sel: &Selection,
        cmd: Cmd,
        out_color: bool,
    ) -> Result<Self> {
        let word = o
            .pretty
            .clone()
            .or_else(|| ctx.config.get("format.pretty"))
            .unwrap_or_else(|| "medium".into());
        let (kind, terminator) = pick_pretty(ctx, &word, 0)?;
        let kind_empty = match &kind {
            Kind::Template(t) => Some(t.is_empty()),
            Kind::Builtin(_) => None,
        };
        let color = match o.color {
            Some(ColorWhen::Always) => true,
            Some(ColorWhen::Never) => false,
            _ => out_color,
        };
        let abbrev_commit = o
            .abbrev_commit
            .or_else(|| ctx.config.bool("log.abbrevCommit"))
            .unwrap_or(false);
        let abbrev = o
            .abbrev
            .or_else(|| {
                ctx.config
                    .get("core.abbrev")
                    .and_then(|v| v.parse::<usize>().ok())
                    .map(|n| n.clamp(4, 40))
            })
            .unwrap_or(7);
        let date_word = o.date.clone().or_else(|| ctx.config.get("log.date"));
        let date = match (&date_word, o.relative_date) {
            (_, true) => date::Mode::parse("relative").unwrap_or_default(),
            (Some(d), false) => date::Mode::parse(d)
                .ok_or_else(|| CliError::Raw(format!("fatal: unknown date format {d}"), 128))?,
            (None, false) if word == "reference" => date::Mode::parse("short").unwrap_or_default(),
            (None, false) => date::Mode::default(),
        };
        let decorate = match o.decorate {
            Some(d) => d,
            None => match ctx.config.get("log.decorate").as_deref() {
                Some("full") => Decorate::Full,
                Some("short" | "true" | "yes" | "on" | "1") => Decorate::Short,
                _ => Decorate::No,
            },
        };
        let template_decor = matches!(&kind, Kind::Template(t) if t.uses_decorations());
        let decor = if decorate != Decorate::No || template_decor {
            decorate::load(ws).await?
        } else {
            HashMap::new()
        };
        let mut describer = None;
        if let Kind::Template(t) = &kind
            && !t.describe_options().is_empty()
        {
            describer = Some(Describer::load(ws, abbrev).await?);
        }
        let mailmap = match &kind {
            Kind::Template(_) => load_mailmap(ctx, ws),
            Kind::Builtin(_) => None,
        };
        let graph = o.graph.then(|| {
            let mut g = Graph::new(color, o.left_right);
            g.first_parent_only(o.first_parent);
            g
        });
        let diff = DiffSetup::new(ctx, o, sel, cmd == Cmd::Show, abbrev)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let wants_source = o.source || matches!(&kind, Kind::Template(t) if t.uses_source());
        let sources = if wants_source {
            sel.tips.iter().map(|(id, n)| (*id, n.clone())).collect()
        } else {
            HashMap::new()
        };
        Ok(Self {
            ws,
            hist,
            kind,
            terminator,
            line_term: if o.nul { 0 } else { b'\n' },
            abbrev_commit,
            abbrev,
            date,
            now,
            color,
            decorate,
            decor,
            graph,
            diff,
            describer,
            mailmap,
            sources,
            show_source: o.source,
            left_right: o.left_right,
            first_parent: o.first_parent,
            cmd,
            format_empty: matches!(&kind_empty, Some(true)),
            shown_one: false,
            last_open: false,
        })
    }

    /// Marks that something was printed, so the next commit is separated from it.
    pub fn mark_shown(&mut self) {
        self.shown_one = true;
    }

    fn short(&self, oid: &GitOid) -> String {
        let hex = oid.to_string();
        hex[..self.abbrev.min(40)].to_owned()
    }

    /// Writes graph-prefixed lines of `text`.
    fn prefixed<W: Write>(&mut self, out: &mut W, text: &[u8]) -> Result<()> {
        let Some(g) = &mut self.graph else {
            return out.write_all(text).map_err(io);
        };
        let mut p = 0;
        while p < text.len() {
            let end = text[p..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(text.len(), |i| p + i + 1);
            out.write_all(&g.padding_line()).map_err(io)?;
            out.write_all(&text[p..end]).map_err(io)?;
            p = end;
        }
        Ok(())
    }

    /// `graph_show_commit`: the graph lines up to and including this commit's own.
    fn graph_commit_line<W: Write>(&mut self, out: &mut W) -> Result<()> {
        let Some(g) = &mut self.graph else {
            return Ok(());
        };
        if g.is_commit_finished() {
            out.write_all(&g.padding_line()).map_err(io)?;
            return Ok(());
        }
        loop {
            let (line, is_commit) = g.next_line();
            out.write_all(&line).map_err(io)?;
            if is_commit {
                return Ok(());
            }
            out.write_all(b"\n").map_err(io)?;
        }
    }

    /// `graph_show_oneline`: the graph text for the next line.
    fn graph_next<W: Write>(&mut self, out: &mut W) -> Result<()> {
        if let Some(g) = &mut self.graph {
            out.write_all(&g.next_line().0).map_err(io)?;
        }
        Ok(())
    }

    /// `graph_show_commit_msg`.
    fn message<W: Write>(&mut self, out: &mut W, msg: &[u8]) -> Result<()> {
        let Some(g) = &mut self.graph else {
            return out.write_all(msg).map_err(io);
        };
        let mut p = 0;
        while p < msg.len() {
            let end = msg[p..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(msg.len(), |i| p + i + 1);
            out.write_all(&msg[p..end]).map_err(io)?;
            if end < msg.len() {
                out.write_all(&g.next_line().0).map_err(io)?;
            }
            p = end;
        }
        let terminated = msg.last() == Some(&b'\n');
        if !g.is_commit_finished() {
            if !terminated {
                out.write_all(b"\n").map_err(io)?;
            }
            loop {
                out.write_all(&g.next_line().0).map_err(io)?;
                if g.is_commit_finished() {
                    break;
                }
                out.write_all(b"\n").map_err(io)?;
            }
            if terminated {
                out.write_all(b"\n").map_err(io)?;
            }
        }
        Ok(())
    }

    fn decor_text(&self, id: &ChangesetId) -> Vec<u8> {
        if self.decorate == Decorate::No {
            return Vec::new();
        }
        self.decor.get(id).map_or_else(Vec::new, |d| {
            d.text(&Style::standard(
                self.decorate == Decorate::Full,
                self.color,
            ))
        })
    }

    /// The git commit data the formats read.
    async fn commit_data(&self, e: &Entry) -> Result<(GitOid, Vec<GitOid>)> {
        let tree = self.ws.engine.get_tree(e.cs.root).await?;
        let mut parents = Vec::with_capacity(e.cs.parents.len());
        for p in &e.cs.parents {
            parents.push(self.ws.engine.git_commit_of(*p).await.map_err(fatal)?);
        }
        Ok((tree_oid(&tree), parents))
    }

    /// Prints one commit.
    pub async fn print<W: Write>(
        &mut self,
        out: &mut W,
        e: &Entry,
        reflog: Option<&Reflog>,
    ) -> Result<()> {
        let (tree, parents) = self.commit_data(e).await?;
        let cs = &e.cs;
        if let Some(g) = &mut self.graph {
            g.update(e.id, e.shown_parents.clone(), e.left);
        }
        if self.shown_one && !self.terminator {
            if !self.last_open
                && let Some(g) = &mut self.graph
            {
                out.write_all(&g.padding_line()).map_err(io)?;
            }
            out.write_all(&[self.line_term]).map_err(io)?;
        }
        self.shown_one = true;
        self.graph_commit_line(out)?;

        // The source name travels from children to parents.
        let source = self.sources.remove(&e.id);
        if let Some(s) = &source {
            for p in &e.cs.parents {
                self.sources.entry(*p).or_insert_with(|| s.clone());
            }
        }

        let hash = if self.abbrev_commit {
            self.short(&e.git)
        } else {
            e.git.to_string()
        };
        let mark = if self.graph.is_none() && self.left_right {
            if e.left { "< " } else { "> " }
        } else {
            ""
        };
        let encoding = encoding_of(cs);
        let commit = Commit {
            id: e.git,
            tree,
            parents: &parents,
            author: &cs.author,
            committer: &cs.committer,
            message: &cs.message,
            encoding: encoding.as_deref(),
        };
        let msg: Vec<u8>;
        match &self.kind {
            Kind::Template(t) => {
                let mut describe = HashMap::new();
                if let Some(d) = &self.describer {
                    for opt in t.describe_options() {
                        describe.insert(
                            opt.clone(),
                            d.describe(self.ws, self.hist, e.id, &e.git.to_string(), &opt)
                                .await,
                        );
                    }
                }
                let env = Env {
                    abbrev: self.abbrev,
                    date: self.date.clone(),
                    now: self.now,
                    color: self.color,
                    decorate_full: self.decorate == Decorate::Full,
                    mailmap: self.mailmap.as_ref(),
                    columns: columns(),
                };
                let extras = Extras {
                    decor: self.decor.get(&e.id),
                    source: source.as_deref(),
                    mark: if e.left && self.left_right { '<' } else { '>' },
                    notes: b"",
                    reflog,
                    describe: Some(&describe),
                    graph_width: self.graph.as_ref().map_or(0, Graph::width),
                };
                let mut text = Vec::new();
                t.render(&commit, &env, &extras, &mut text);
                msg = text;
            }
            Kind::Builtin(b) => {
                let b = *b;
                let view = pretty::View {
                    tree: tree.to_string(),
                    parents: parents.iter().map(ToString::to_string).collect(),
                    parents_short: parents.iter().map(|p| self.short(p)).collect(),
                    author: &cs.author,
                    committer: &cs.committer,
                    message: &cs.message,
                    date: &self.date,
                    now: self.now,
                };
                let body = pretty::print(b, &view);
                let mut head = Vec::new();
                if b.is_mail() {
                    head.extend_from_slice(
                        format!("From {} Mon Sep 17 00:00:00 2001\n", e.git).as_bytes(),
                    );
                } else {
                    let (open, close): (&[u8], &[u8]) = if self.color {
                        (b"\x1b[33m", b"\x1b[m")
                    } else {
                        (b"", b"")
                    };
                    head.extend_from_slice(open);
                    if b != Builtin::Oneline {
                        head.extend_from_slice(b"commit ");
                    }
                    head.extend_from_slice(mark.as_bytes());
                    head.extend_from_slice(hash.as_bytes());
                    if self.show_source
                        && let Some(s) = &source
                    {
                        head.extend_from_slice(format!("\t{s}").as_bytes());
                    }
                    head.extend_from_slice(close);
                    head.extend_from_slice(&self.decor_text(&e.id));
                    head.push(if b == Builtin::Oneline { b' ' } else { b'\n' });
                    if let Some(r) = reflog
                        && b != Builtin::Oneline
                    {
                        head.extend_from_slice(
                            format!(
                                "Reflog: {} ({})\nReflog message: {}\n",
                                r.selector,
                                String::from_utf8_lossy(
                                    &[r.name.as_slice(), b" <", r.email.as_slice(), b">"].concat()
                                ),
                                String::from_utf8_lossy(&r.subject)
                            )
                            .as_bytes(),
                        );
                    }
                }
                out.write_all(&head).map_err(io)?;
                if b != Builtin::Oneline && !b.is_mail() {
                    self.graph_next(out)?;
                }
                msg = body;
            }
        }
        let missing_newline = msg.last().is_none_or(|&c| c != b'\n');
        self.last_open = missing_newline;
        self.message(out, &msg)?;
        if self.terminator && !self.format_empty {
            if !missing_newline && let Some(g) = &mut self.graph {
                out.write_all(&g.padding_line()).map_err(io)?;
            }
            out.write_all(&[self.line_term]).map_err(io)?;
        }
        self.print_diff(out, e).await
    }

    async fn print_diff<W: Write>(&mut self, out: &mut W, e: &Entry) -> Result<()> {
        if !self.diff.wants() {
            return Ok(());
        }
        let new = e.cs.root;
        let old = match e.cs.parents.as_slice() {
            [] => CliEngine::empty_tree_id(),
            [p] => self.ws.engine.get_changeset(*p).await?.root,
            [p, ..] if self.first_parent => self.ws.engine.get_changeset(*p).await?.root,
            // A merge shown with its combined diff prints only the separator when clean.
            _ if self.cmd == Cmd::Show => {
                if !matches!(&self.kind, Kind::Builtin(Builtin::Oneline)) {
                    if let Some(g) = &mut self.graph {
                        out.write_all(&g.padding_line()).map_err(io)?;
                    }
                    out.write_all(&[self.line_term]).map_err(io)?;
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        let Some(text) = self.diff.render(self.ws, old, new).await? else {
            return Ok(());
        };
        let oneline = matches!(&self.kind, Kind::Builtin(Builtin::Oneline));
        if !oneline {
            if self.diff.stat_and_patch() {
                self.prefixed(out, b"---")?;
            } else if let Some(g) = &mut self.graph {
                out.write_all(&g.padding_line()).map_err(io)?;
            }
            out.write_all(b"\n").map_err(io)?;
        }
        self.prefixed(out, &text)
    }
}

/// Largest `.mailmap` read; a larger file is ignored (fail closed, not truncated).
const MAILMAP_MAX: u64 = 4 << 20;

/// The mailmap: `mailmap.file` (the user's own config path) or the work tree's `.mailmap`, which
/// is taken through `SafeRoot` so a symlinked or reparse-point ancestor reads as missing.
fn load_mailmap(ctx: &Ctx, ws: &WsCtx) -> Option<Mailmap> {
    let path = match ctx.config.get("mailmap.file") {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let name = asc_core::path::RepoPath::new(b".mailmap".to_vec()).ok()?;
            asc_wc::SafeRoot::new(ws.projection.toplevel()).resolve(&name)?
        }
    };
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAILMAP_MAX {
        return None;
    }
    std::fs::read(&path).ok().map(|b| Mailmap::parse(&b))
}
