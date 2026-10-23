//! `apply` (PLAN U27b): `git apply` over the engine's patch parser. The patches are checked as a
//! whole against the work tree (default), the staged tree (`--cached`) or both (`--index`,
//! `--3way`); only when every file patch applies are the files written, so a failing patch
//! changes nothing. `--check` stops after the check. Errors use git's texts on stderr and exit 1.
//!
//! Not served: `--stat`, `--numstat`, `--summary`, `--reject`, `--directory`, `--include`,
//! `--exclude`, `-C`, whitespace repair (`--whitespace=fix|strip|error`; `nowarn` and `warn` are
//! accepted, and `warn` prints nothing), symlink and submodule patches, and a 3-way merge that
//! conflicts (it fails the patch). Paths in a patch are relative to the top of the work tree.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Read};
use std::process::ExitCode;

use asc_core::content::ingest;
use asc_core::git::{ObjectKind, blob_oid};
use asc_core::object::Kind;
use asc_core::path::{RepoPath, checkout_safe};
use asc_engine::derive::GitMapRow;
use asc_engine::diff::apply::{self, ApplyOptions, FilePatch, MAX_APPLY_BYTES, ParseOptions};
use asc_state::state::git_map;
use asc_state::state::stage::{self, Leaf, StageRow, StageSet};
use asc_wc::SafeRoot;
use asc_wc::safe_path::Verified;

use super::add;
use super::add::convert;
use super::add::target::{Edit, Staged, Target};
use super::common::{CliError, Ctx, Result, WsCtx};
use crate::msg::Msg;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "apply", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Apply to the staged tree only.
    #[arg(long = "cached")]
    pub cached: bool,
    /// Apply to the work tree and the staged tree.
    #[arg(long = "index")]
    pub index: bool,
    /// Only check that the patches apply.
    #[arg(long = "check")]
    pub check: bool,
    /// Fall back to a three-way merge when a patch does not apply.
    #[arg(short = '3', long = "3way")]
    pub three_way: bool,
    /// Count the lines of each hunk instead of trusting the `@@` header.
    #[arg(long = "recount")]
    pub recount: bool,
    /// Accept hunks without context.
    #[arg(long = "unidiff-zero")]
    pub unidiff_zero: bool,
    #[arg(long = "whitespace")]
    pub whitespace: Option<String>,
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
    /// Leading path components to remove.
    #[arg(short = 'p')]
    pub strip: Option<usize>,
    /// Apply the patch in reverse.
    #[arg(short = 'R', long = "reverse")]
    pub reverse: bool,
    pub patches: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "apply",
    flags: &[
        "--cached",
        "--index",
        "--check",
        "-3",
        "--3way",
        "--recount",
        "--unidiff-zero",
        "--whitespace",
        "-v",
        "--verbose",
        "-p",
        "-R",
        "--reverse",
    ],
    placeholders: &[],
}];

/// What a run does with the patches.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// The preimage is the staged tree, and the work tree is left alone.
    pub cached: bool,
    /// The staged tree takes the result (always set with `cached`).
    pub index: bool,
    pub check: bool,
    pub three_way: bool,
    pub unidiff_zero: bool,
    pub verbose: bool,
}

/// One file as the patches leave it.
#[derive(Clone)]
struct Slot {
    content: Vec<u8>,
    kind: Kind,
}

fn unsupported(flag: &str) -> CliError {
    CliError::Msg(Msg::AscOptionUnsupported {
        flag: flag.to_owned(),
    })
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    match a.whitespace.as_deref() {
        None | Some("nowarn") | Some("warn") => {}
        Some(other) => return Err(unsupported(&format!("--whitespace={other}"))),
    }
    let settings = Settings {
        cached: a.cached,
        index: a.cached || a.index || a.three_way,
        check: a.check,
        three_way: a.three_way,
        unidiff_zero: a.unidiff_zero,
        verbose: a.verbose,
    };
    let opts = ParseOptions {
        strip: a.strip.unwrap_or(1),
        recount: a.recount,
    };
    let mut inputs = a.patches.clone();
    if inputs.is_empty() {
        inputs.push(OsString::from("-"));
    }
    let mut applier = Applier::new(ctx, ws, &settings).await?;
    // Each input is checked and, unless `--check`, written to the work tree on its own; the
    // staged tree is written once at the end, and only when every input applied (as git does).
    for item in &inputs {
        let (label, bytes) = read_input(ctx, item)?;
        let mut patches = match apply::parse(&bytes, &opts) {
            Ok(v) => v,
            Err(e) => return Err(CliError::Raw(parse_error(&e, &label, opts.strip), 128)),
        };
        if patches.is_empty() {
            return Err(CliError::Raw(
                "error: No valid patches in input (allow with \"--allow-empty\")".into(),
                128,
            ));
        }
        if a.reverse {
            patches = patches
                .iter()
                .map(FilePatch::reversed)
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| CliError::Raw(format!("error: {e}"), 1))?;
        }
        if !applier.input(&patches).await? {
            return Ok(ExitCode::from(1));
        }
    }
    applier.finish().await?;
    Ok(ExitCode::SUCCESS)
}

/// git's text for a patch that does not parse.
pub fn parse_error(e: &apply::Error, label: &str, strip: usize) -> String {
    use apply::Error;
    match e {
        Error::Corrupt(n) => format!("error: corrupt patch at {label}:{n}"),
        Error::UnsafePath(p) => format!("error: invalid path '{p}'"),
        Error::NoHeader(n, text) => {
            format!("error: patch fragment without header at line {n}: {text}")
        }
        Error::NoName(n) => format!(
            "error: git diff header lacks filename information when removing {strip} leading pathname component{} (line {n})",
            if strip == 1 { "" } else { "s" }
        ),
        other => format!("error: {other}"),
    }
}

/// A patch file, or standard input for `-`. A bounded read.
fn read_input(ctx: &Ctx, item: &OsString) -> Result<(String, Vec<u8>)> {
    let limit = apply::MAX_PATCH_BYTES as u64 + 1;
    let mut bytes = Vec::new();
    let label = if item == "-" {
        std::io::stdin()
            .lock()
            .take(limit)
            .read_to_end(&mut bytes)?;
        "<stdin>".to_owned()
    } else {
        let path = ctx.cwd.join(item);
        let name = item.to_string_lossy().into_owned();
        let file = fs::File::open(&path).map_err(|e| {
            CliError::Raw(
                format!("error: can't open patch '{name}': {}", describe(&e)),
                128,
            )
        })?;
        file.take(limit).read_to_end(&mut bytes)?;
        name
    };
    if bytes.len() > apply::MAX_PATCH_BYTES {
        return Err(CliError::Raw("error: patch is too large".into(), 128));
    }
    Ok((label, bytes))
}

/// git's text for an I/O error (`strerror`).
fn describe(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".into(),
        std::io::ErrorKind::PermissionDenied => "Permission denied".into(),
        _ => e.to_string(),
    }
}

/// Checks every file patch of one input, then (unless `--check`) writes the work tree, and
/// finally the staged tree. `Ok(false)` means a patch did not apply: its errors are printed and
/// nothing was changed.
pub async fn apply_patches(
    ctx: &Ctx,
    ws: &WsCtx,
    patches: &[FilePatch],
    s: &Settings,
) -> Result<bool> {
    let mut applier = Applier::new(ctx, ws, s).await?;
    if !applier.input(patches).await? {
        return Ok(false);
    }
    applier.finish().await?;
    Ok(true)
}

/// The state of one `apply` run over one or more inputs.
pub struct Applier<'a> {
    ctx: &'a Ctx,
    ws: &'a WsCtx,
    s: &'a Settings,
    gate: SafeRoot,
    target: Option<Target>,
    /// Results so far by path: `None` is a deleted file. Later patches read from here first.
    overlay: BTreeMap<Vec<u8>, Option<Slot>>,
    /// Paths changed by the input being applied, not yet written to the work tree.
    pending: BTreeSet<Vec<u8>>,
    /// Names for `-v`.
    applied: Vec<String>,
}

/// What a source file looks like before a patch.
enum Pre {
    Found(Slot),
    NoIndex,
    NoWork,
}

fn show(name: &[u8]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

/// The path as a repository path, or the text after `error: ` when a patch must not write it.
/// Every path the patch names passes `checkout_safe` before the work tree, the staged tree or
/// the gate see it, so `.git` and `.asc/info` aliases never reach a file system call.
fn checked(path: &[u8]) -> std::result::Result<RepoPath, String> {
    RepoPath::new(path.to_vec())
        .ok()
        .filter(checkout_safe)
        .ok_or_else(|| format!("invalid path '{}'", show(path)))
}

fn patch_name(fp: &FilePatch) -> String {
    match (&fp.old_path, &fp.new_path) {
        (Some(o), Some(n)) if o != n => format!("{} => {}", show(o), show(n)),
        _ => show(fp.target().unwrap_or_default()),
    }
}

fn inflate(data: &[u8], size: usize) -> Option<Vec<u8>> {
    let mut state = gix::zlib::Inflate::default().state;
    let mut out = vec![0u8; size.checked_add(1)?];
    let mut input = data;
    let n = gix::zlib::stream::inflate::read(&mut input, &mut state, &mut out).ok()?;
    if n != size {
        return None;
    }
    out.truncate(n);
    Some(out)
}

impl<'a> Applier<'a> {
    /// A fresh run.
    pub async fn new(ctx: &'a Ctx, ws: &'a WsCtx, s: &'a Settings) -> Result<Self> {
        Ok(Self {
            ctx,
            ws,
            s,
            gate: SafeRoot::new(ws.projection.toplevel()),
            target: if s.index {
                Some(Target::open(ctx).await?)
            } else {
                None
            },
            overlay: BTreeMap::new(),
            pending: BTreeSet::new(),
            applied: Vec::new(),
        })
    }

    /// Checks the file patches of one input and, unless `--check`, writes the work tree.
    pub async fn input(&mut self, patches: &[FilePatch]) -> Result<bool> {
        let mut ok = true;
        for fp in patches {
            ok &= self.check(fp).await?;
        }
        if !ok {
            return Ok(false);
        }
        if !self.s.check {
            self.write_worktree().await?;
            if self.s.verbose {
                for name in std::mem::take(&mut self.applied) {
                    eprintln!("Applied patch {name} cleanly.");
                }
            }
        }
        Ok(true)
    }

    /// Writes the staged tree (after every input applied).
    pub async fn finish(mut self) -> Result<()> {
        if self.s.check || !self.s.index {
            return Ok(());
        }
        self.write_index().await
    }

    /// The text of a file that is not there, for the source of a patch.
    fn missing(&self, which: &Pre, path: &[u8]) -> Option<String> {
        match which {
            Pre::Found(_) => None,
            Pre::NoIndex => Some(format!("{}: does not exist in index", show(path))),
            Pre::NoWork => Some(format!("{}: No such file or directory", show(path))),
        }
    }

    /// The file `path` before the patch being checked: an earlier patch's result, else the staged
    /// tree (`--cached`, and the existence check of `--index`) or the work tree. `Err` is the
    /// text after `error: `.
    async fn pre(&self, path: &[u8]) -> std::result::Result<Pre, String> {
        if let Some(slot) = self.overlay.get(path) {
            return Ok(match slot {
                Some(s) => Pre::Found(s.clone()),
                None if self.s.index => Pre::NoIndex,
                None => Pre::NoWork,
            });
        }
        let rp = checked(path)?;
        let staged = match &self.target {
            Some(t) => Some(
                t.staged(self.ws, &rp)
                    .await
                    .map_err(|e| format!("{}: {e}", show(path)))?,
            ),
            None => None,
        };
        if self.s.cached {
            return match staged {
                Some(Staged::Leaf { leaf, .. }) => {
                    let content = self
                        .ws
                        .engine
                        .read_file(leaf.file, leaf.size)
                        .await
                        .map_err(|e| format!("{}: {e}", show(path)))?;
                    Ok(Pre::Found(Slot {
                        content: content.to_vec(),
                        kind: leaf.kind,
                    }))
                }
                Some(Staged::Unmerged) => Err(format!("{}: does not match index", show(path))),
                _ => Ok(Pre::NoIndex),
            };
        }
        if matches!(staged, Some(Staged::Absent)) {
            return Ok(Pre::NoIndex);
        }
        let Some(abs) = self.gate.resolve(&rp) else {
            return Ok(Pre::NoWork);
        };
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Pre::NoWork),
            Err(e) => return Err(format!("{}: {}", show(path), describe(&e))),
        };
        if !meta.is_file() {
            return Err(format!("{}: is not a regular file", show(path)));
        }
        if meta.len() > MAX_APPLY_BYTES as u64 {
            return Err(format!("{}: file is too large", show(path)));
        }
        let content = fs::read(&abs).map_err(|e| format!("{}: {}", show(path), describe(&e)))?;
        let exec = convert::exec_bit(&meta);
        let mut kind = if exec == Some(true) {
            Kind::Exec
        } else {
            Kind::File
        };
        if let Some(st) = staged {
            // With --index the work tree file must be what is staged.
            match st {
                Staged::Leaf { leaf, .. } => {
                    if blob_oid(&content) != leaf.git {
                        return Err(format!("{}: does not match index", show(path)));
                    }
                    if exec.is_none() {
                        kind = leaf.kind;
                    }
                }
                Staged::Absent => return Ok(Pre::NoIndex),
                Staged::Unmerged => return Err(format!("{}: does not match index", show(path))),
            }
        }
        Ok(Pre::Found(Slot { content, kind }))
    }

    /// Where a file that a patch creates already is: `Some("index")` or
    /// `Some("working directory")`.
    async fn exists(&self, path: &[u8]) -> std::result::Result<Option<&'static str>, String> {
        let here = |has: bool| {
            has.then_some(if self.s.index {
                "index"
            } else {
                "working directory"
            })
        };
        if let Some(slot) = self.overlay.get(path) {
            return Ok(here(slot.is_some()));
        }
        let rp = checked(path)?;
        if let Some(t) = &self.target
            && t.staged(self.ws, &rp)
                .await
                .map_err(|e| format!("{}: {e}", show(path)))?
                != Staged::Absent
        {
            return Ok(Some("index"));
        }
        if !self.s.cached
            && let Some(abs) = self.gate.resolve(&rp)
            && fs::symlink_metadata(&abs).is_ok()
        {
            return Ok(Some("working directory"));
        }
        Ok(None)
    }

    /// The blob a `--3way` merge starts from: the staged or HEAD entry of `path` whose id starts
    /// with the patch's `index` line.
    async fn base_blob(&self, path: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
        let rp = checked(path).ok()?;
        let target = self.target.as_ref()?;
        let prefix = std::str::from_utf8(prefix).ok()?.to_ascii_lowercase();
        let mut candidates: Vec<Leaf> = Vec::new();
        if let Ok(Staged::Leaf { leaf, .. }) = target.staged(self.ws, &rp).await {
            candidates.push(leaf);
        }
        if let Ok(Some(leaf)) = target.head_leaf(self.ws, &rp).await {
            candidates.push(leaf);
        }
        let leaf = candidates
            .into_iter()
            .find(|l| l.git.to_string().starts_with(&prefix))?;
        self.ws
            .engine
            .read_file(leaf.file, leaf.size)
            .await
            .ok()
            .map(|b| b.to_vec())
    }

    async fn check(&mut self, fp: &FilePatch) -> Result<bool> {
        let name = patch_name(fp);
        if self.s.verbose {
            eprintln!("Checking patch {name}...");
        }
        let fail = |text: String| {
            eprintln!("error: {text}");
            false
        };
        let (Some(dest), src) = (
            fp.new_path.as_deref().or(fp.old_path.as_deref()),
            fp.old_path.as_deref(),
        ) else {
            return Ok(false);
        };
        for path in [src, Some(dest)].into_iter().flatten() {
            if let Err(e) = checked(path) {
                return Ok(fail(e));
            }
        }
        let modes = [fp.old_mode, fp.new_mode];
        if modes
            .iter()
            .flatten()
            .any(|m| matches!(m & 0o170000, 0o120000 | 0o160000))
        {
            return Ok(fail(format!(
                "{}: symbolic link and submodule patches are not supported",
                show(dest)
            )));
        }
        if fp.binary_no_body {
            return Ok(fail(format!(
                "cannot apply binary patch to '{}' without full index line",
                show(dest)
            )));
        }
        // The source must exist; a created file or a rename target must not.
        let base = match src {
            Some(path) if !fp.is_new => match self.pre(path).await {
                Ok(Pre::Found(b)) => Some(b),
                Ok(other) => {
                    let text = self.missing(&other, path).unwrap_or_default();
                    return Ok(fail(text));
                }
                Err(e) => return Ok(fail(e)),
            },
            _ => None,
        };
        #[allow(clippy::collapsible_if)]
        if fp.is_new || fp.is_rename || fp.is_copy {
            if fp.is_new || src != Some(dest) {
                match self.exists(dest).await {
                    Ok(None) => {}
                    Ok(Some(where_)) => {
                        return Ok(fail(format!("{}: already exists in {where_}", show(dest))));
                    }
                    Err(e) => return Ok(fail(e)),
                }
            }
        }
        let opts = ApplyOptions {
            unidiff_zero: self.s.unidiff_zero,
        };
        let result: Option<Vec<u8>>;
        if let Some(bin) = &fp.binary {
            if fp.is_delete {
                result = None;
            } else {
                match apply::apply_binary(
                    base.as_ref().map(|b| &b.content[..]),
                    &bin.forward,
                    &inflate,
                ) {
                    Ok(v) => result = Some(v),
                    Err(_) => {
                        eprintln!("error: binary patch does not apply to '{}'", show(dest));
                        return Ok(fail(format!("{}: patch does not apply", show(dest))));
                    }
                }
            }
        } else {
            let pre = base.as_ref().map(|b| &b.content[..]);
            match apply::apply_file(pre, fp, opts) {
                Ok(r) => {
                    self.report(fp, &r.placements);
                    result = r.content;
                }
                Err(apply::Error::HunkFailed { path, line }) => {
                    if self.s.verbose
                        && let Some(h) = fp.hunks.iter().find(|h| h.old_start == line)
                    {
                        let mut text = String::from("error: while searching for:\n");
                        for l in h.lines.iter().filter(|l| l.op != apply::LineOp::Add) {
                            text.push_str(&String::from_utf8_lossy(&l.text));
                            if !l.text.ends_with(b"\n") {
                                text.push('\n');
                            }
                        }
                        eprintln!("{text}");
                    }
                    eprintln!("error: patch failed: {path}:{line}");
                    match self.three_way(fp, base.as_ref(), opts).await {
                        Some(merged) => result = Some(merged),
                        None => return Ok(fail(format!("{path}: patch does not apply"))),
                    }
                }
                Err(apply::Error::RemovalLeavesContent) => {
                    eprintln!("error: removal patch leaves file contents");
                    return Ok(fail(format!("{}: patch does not apply", show(dest))));
                }
                Err(e) => return Ok(fail(e.to_string())),
            }
        }
        let kind = match fp.new_mode {
            Some(0o100755) => Kind::Exec,
            Some(0o100644) => Kind::File,
            Some(m) => return Ok(fail(format!("{}: unsupported mode {m:o}", show(dest)))),
            None => base.as_ref().map_or(Kind::File, |b| b.kind),
        };
        if fp.is_rename
            && let Some(old) = src
        {
            self.overlay.insert(old.to_vec(), None);
            self.pending.insert(old.to_vec());
        }
        self.overlay
            .insert(dest.to_vec(), result.map(|content| Slot { content, kind }));
        self.pending.insert(dest.to_vec());
        self.applied.push(name);
        Ok(true)
    }

    /// `-v`: where hunks landed away from the line their header names.
    fn report(&self, fp: &FilePatch, placements: &[apply::Placement]) {
        if !self.s.verbose {
            return;
        }
        for p in placements {
            let Some(h) = fp.hunks.get(p.hunk - 1) else {
                continue;
            };
            let offset = (p.line as isize - 1) - h.new_start.saturating_sub(1) as isize;
            if offset != 0 {
                let plural = if offset.abs() == 1 { "" } else { "s" };
                eprintln!(
                    "Hunk #{} succeeded at {} (offset {offset} line{plural}).",
                    p.hunk, p.line
                );
            }
        }
    }

    /// `--3way`: merge the current file with the patch applied to the blob it was made against.
    async fn three_way(
        &self,
        fp: &FilePatch,
        ours: Option<&Slot>,
        opts: ApplyOptions,
    ) -> Option<Vec<u8>> {
        if !self.s.three_way {
            return None;
        }
        let ours = ours?;
        let path = fp.old_path.as_deref()?;
        let prefix = fp.old_oid.as_deref()?;
        let Some(base) = self.base_blob(path, prefix).await else {
            eprintln!("error: repository lacks the necessary blob to perform 3-way merge.");
            return None;
        };
        eprintln!("Falling back to three-way merge...");
        let theirs = match apply::apply_text(&base, fp, opts) {
            Ok(a) => a.content,
            Err(_) => {
                eprintln!("error: cannot apply the patch to the blob it was made against");
                return None;
            }
        };
        match apply::merge_clean(&base, &ours.content, &theirs) {
            Some(merged) => {
                eprintln!("Applied patch to '{}' cleanly.", show(path));
                Some(merged)
            }
            None => {
                eprintln!(
                    "error: {}: the three-way merge conflicts; asc does not leave conflict stages for apply",
                    show(path)
                );
                None
            }
        }
    }

    /// Writes the paths of the input just checked to the work tree: deletions first, then files.
    async fn write_worktree(&mut self) -> Result<()> {
        let pending = std::mem::take(&mut self.pending);
        if self.s.cached {
            return Ok(());
        }
        let fatal = |path: &[u8], e: &dyn std::fmt::Display| {
            CliError::Fatal(format!("{}: {e}", show(path)))
        };
        // Nothing is touched until every path is vetted.
        for path in &pending {
            checked(path).map_err(CliError::Fatal)?;
        }
        let mut verified = Verified::default();
        let filemode = self.ctx.config.bool("core.filemode").unwrap_or(cfg!(unix));
        for path in &pending {
            if matches!(self.overlay.get(path), Some(None)) {
                let rp = checked(path).map_err(CliError::Fatal)?;
                if self.gate.remove_file(&rp).map_err(|e| fatal(path, &e))? {
                    self.gate.remove_empty_parents(&rp);
                }
            }
        }
        for path in &pending {
            let Some(Some(slot)) = self.overlay.get(path) else {
                continue;
            };
            let rp = checked(path).map_err(CliError::Fatal)?;
            let abs = self
                .gate
                .resolve(&rp)
                .ok_or_else(|| fatal(path, &"beyond a symbolic link"))?;
            verified
                .ensure_parent(self.gate.root(), &rp, false)
                .map_err(|e| fatal(path, &e))?;
            // Never write through a link that sits at the path itself.
            let _ = self.gate.remove_file(&rp);
            fs::write(&abs, &slot.content).map_err(|e| fatal(path, &e))?;
            set_exec(&abs, slot.kind == Kind::Exec && filemode).map_err(|e| fatal(path, &e))?;
        }
        Ok(())
    }

    /// Writes the staged tree: every path the inputs changed.
    async fn write_index(&mut self) -> Result<()> {
        let fatal = |path: &[u8], e: &dyn std::fmt::Display| {
            CliError::Fatal(format!("{}: {e}", show(path)))
        };
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };
        let mut edits = Vec::new();
        let mut mapped = Vec::new();
        for (path, slot) in &self.overlay {
            let rp = checked(path).map_err(CliError::Fatal)?;
            match slot {
                None => {
                    if target.staged(self.ws, &rp).await? != Staged::Absent {
                        edits.push(Edit::Drop(rp));
                    }
                }
                Some(slot) => {
                    let got =
                        ingest(Cursor::new(&slot.content[..])).map_err(|e| fatal(path, &e))?;
                    add::store(self.ctx, self.ws, &got).await?;
                    mapped.push(GitMapRow {
                        git: got.git,
                        kind: ObjectKind::Blob,
                        asc: *got.file.as_bytes(),
                    });
                    let leaf = Leaf {
                        kind: slot.kind,
                        file: got.file,
                        size: got.size,
                        git: got.git,
                    };
                    edits.push(Edit::Put {
                        path: rp,
                        leaf,
                        ita: false,
                    });
                }
            }
        }
        if edits.is_empty() {
            return Ok(());
        }
        git_map::insert_many(&self.ws.db, self.ws.repo.key, &mapped).await?;
        if !self.s.cached || !target.is_default() {
            // The work tree holds what is staged: the synthetic index may trust its stat.
            return target.apply(self.ctx, edits).await;
        }
        // `--cached`: the work tree files differ from the staged content, so the rows are
        // written without the stat of the files (the index must not call them clean).
        let id = self.ws.workspace.ws.id;
        let mut upserts = Vec::new();
        for edit in &edits {
            let key = edit.path().as_bytes().to_vec();
            stage::remove(&self.ws.db, id, StageSet::Default, &key).await?;
            let head = target.head_leaf(self.ws, edit.path()).await?;
            match edit {
                Edit::Put { leaf, .. } if head.as_ref() != Some(leaf) => {
                    upserts.push(StageRow::from_leaf(key, Some(leaf)));
                }
                Edit::Drop(_) if head.is_some() => upserts.push(StageRow::from_leaf(key, None)),
                _ => {}
            }
        }
        stage::upsert(&self.ws.db, id, StageSet::Default, &upserts).await?;
        crate::ext::index_sink()
            .on_stage_changed(id, &[])
            .await
            .map_err(|e| CliError::Fatal(e.to_string()))
    }
}

#[cfg(unix)]
fn set_exec(abs: &std::path::Path, exec: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(abs)?.permissions().mode();
    let want = if exec { mode | 0o111 } else { mode & !0o111 };
    if want != mode {
        fs::set_permissions(abs, fs::Permissions::from_mode(want))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_exec(_: &std::path::Path, _: bool) -> std::io::Result<()> {
    Ok(())
}
