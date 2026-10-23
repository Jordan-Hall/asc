//! `ls-files` (SPEC §9.5): the index (HEAD's tree overlaid with the stage rows) as git prints its
//! own, and with `-o` the untracked files of the work tree.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use asc_wc::ignore::{IgnoreSources, IgnoreStack};

use super::index::Index;
use super::quote::{Names, relative};
use super::usage;
use crate::cli::add::spec::Spec;
use crate::cli::add::cwd_prefix;
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::cli::refs_plumbing::args::{Cursor, Word};

#[derive(Default)]
struct Opts {
    cached: bool,
    stage: bool,
    others: bool,
    exclude_standard: bool,
    nul: bool,
    full_name: bool,
    error_unmatch: bool,
    pathspecs: Vec<OsString>,
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, usage::LS_FILES);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.pathspecs.extend(cur.rest().into_iter().map(OsString::from)),
            Word::Operand(p) => o.pathspecs.push(p.into()),
            Word::Short(flags) => {
                for c in flags.chars() {
                    match c {
                        'c' => o.cached = true,
                        's' => o.stage = true,
                        'o' => o.others = true,
                        'z' => o.nul = true,
                        other => return Err(cur.usage_error(&format!("unknown switch `{other}'"))),
                    }
                }
            }
            Word::Long(name, _) => match name.as_str() {
                "cached" => o.cached = true,
                "stage" => o.stage = true,
                "others" => o.others = true,
                "exclude-standard" => o.exclude_standard = true,
                "full-name" => o.full_name = true,
                "error-unmatch" => o.error_unmatch = true,
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

/// Where `-o` takes its ignore rules from (the same sources `status` reads).
pub(super) fn ignore_sources(ctx: &Ctx, ws: &WsCtx) -> IgnoreSources {
    let home = std::env::home_dir();
    let xdg = |sub: &str| {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        base.map(|b| b.join(sub))
    };
    let root = ws.projection.toplevel();
    IgnoreSources {
        asc_excludes: ctx
            .config
            .path("asc.excludesFile")
            .or_else(|| xdg("asc/ignore"))
            .unwrap_or_else(|| root.join(".asc/no-such-global-ignore")),
        core_excludes: ctx.config.path("core.excludesFile").or_else(|| xdg("git/ignore")),
        local_exclude: root.join(".asc/info/exclude"),
        common_exclude: ws.projection.common_dir().join("info/exclude"),
    }
}

/// The untracked entries below `dir`, appended to `out` in path order. A directory that is a
/// repository of its own prints as `name/`.
struct Others<'a> {
    root: &'a Path,
    spec: &'a Spec,
    tracked: &'a HashSet<Vec<u8>>,
    tracked_dirs: &'a HashSet<Vec<u8>>,
    ignore: Option<IgnoreStack>,
    out: Vec<Vec<u8>>,
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = dir.to_vec();
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

fn name_bytes(n: &std::ffi::OsStr) -> Vec<u8> {
    n.to_string_lossy().into_owned().into_bytes()
}

impl Others<'_> {
    fn walk(&mut self, rel: &[u8], abs: &Path) -> std::io::Result<()> {
        if let Some(stack) = &mut self.ignore {
            stack.push_dir(abs)?;
        }
        let result = self.entries(rel, abs);
        if let Some(stack) = &mut self.ignore {
            stack.pop_dir();
        }
        result
    }

    fn entries(&mut self, rel: &[u8], abs: &Path) -> std::io::Result<()> {
        // Real directories only: `file_type` does not follow a link.
        let listing = std::fs::read_dir(abs)?; // lint:allow-worktree-fs
        let mut items: Vec<(Vec<u8>, bool, std::path::PathBuf)> = Vec::new();
        for entry in listing {
            let entry = entry?;
            let name = name_bytes(&entry.file_name());
            if name == b".git" {
                continue;
            }
            let is_dir = entry.file_type()?.is_dir() && !asc_wc::safe_path::is_link(&entry.metadata()?);
            items.push((name, is_dir, entry.path()));
        }
        // git's order: a directory sorts as its name followed by `/`.
        items.sort_by(|a, b| {
            let key = |n: &Vec<u8>, d: bool| {
                let mut k = n.clone();
                if d {
                    k.push(b'/');
                }
                k
            };
            key(&a.0, a.1).cmp(&key(&b.0, b.1))
        });
        for (name, is_dir, child_abs) in items {
            let path = join(rel, &name);
            if self.ignore.as_ref().is_some_and(|s| s.is_ignored(&child_abs, is_dir)) {
                continue;
            }
            if !is_dir {
                if !self.tracked.contains(&path) && self.spec.matches(&path) {
                    self.out.push(path);
                }
                continue;
            }
            if !self.spec.may_contain_dir(&path) {
                continue;
            }
            if !self.tracked_dirs.contains(&path) && child_abs.join(".git").exists() {
                let mut dir = path;
                dir.push(b'/');
                if self.spec.matches(&dir[..dir.len() - 1]) {
                    self.out.push(dir);
                }
                continue;
            }
            self.walk(&path, &child_abs)?;
        }
        Ok(())
    }
}

pub async fn run(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let o = parse(&args)?;
    let ws = ctx.ws()?;
    let prefix = cwd_prefix(ctx, ws);
    let root = ws.projection.toplevel();
    let mut items = o.pathspecs.clone();
    if items.is_empty() && !prefix.is_empty() {
        items.push(OsString::from("."));
    }
    let spec = Spec::parse(&items, &prefix, root)?;
    let spec_ref = (!items.is_empty()).then_some(&spec);
    let mut out = Output::open(ctx, "ls-files", PagerWant::Never)?;
    let names = Names {
        terminator: if o.nul { 0 } else { b'\n' },
        quote_high: ctx.config.bool("core.quotePath").unwrap_or(true),
    };
    let strip = if prefix.is_empty() || o.full_name { Vec::new() } else { format!("{prefix}/").into_bytes() };
    let show = |out: &mut Output, path: &[u8]| -> std::io::Result<()> {
        names.write_bare(out, &if strip.is_empty() { path.to_vec() } else { relative(path, &strip) })
    };
    let show_cached = o.cached || o.stage || !o.others;
    let mut matched = vec![false; spec.includes().len()];
    let mut tracked: HashSet<Vec<u8>> = HashSet::new();
    let mut tracked_dirs: HashSet<Vec<u8>> = HashSet::new();
    let index = Index::open(ctx).await?;
    // git lists the untracked files before the index entries, whatever the option order.
    if o.others {
        let mut entries = index.entries(ws, None).await?;
        while let Some(e) = entries.next().await? {
            for (at, b) in e.path.iter().enumerate() {
                if *b == b'/' {
                    tracked_dirs.insert(e.path[..at].to_vec());
                }
            }
            tracked.insert(e.path.clone());
        }
        let ignore = o.exclude_standard.then(|| IgnoreStack::new(root, &ignore_sources(ctx, ws)));
        let mut walker = Others {
            root,
            spec: &spec,
            tracked: &tracked,
            tracked_dirs: &tracked_dirs,
            ignore,
            out: Vec::new(),
        };
        let top = root.to_owned();
        walker.walk(b"", &top).map_err(CliError::Io)?;
        for path in std::mem::take(&mut walker.out) {
            show(&mut out, &path)?;
            out.write_all(&[names.terminator])?;
        }
    }
    if show_cached {
        let mut entries = index.entries(ws, spec_ref).await?;
        while let Some(e) = entries.next().await? {
            spec.mark(&e.path, &mut matched);
            let result: std::io::Result<()> = (|| {
                if o.stage {
                    write!(out, "{:06o} {} {}	", e.mode(), e.oid, e.stage)?;
                }
                show(&mut out, &e.path)?;
                out.write_all(&[names.terminator])
            })();
            result?;
        }
    }
    out.finish()?;
    if o.error_unmatch {
        let mut missing = false;
        for (item, hit) in spec.includes().iter().zip(&matched) {
            if !hit {
                eprintln!("error: pathspec '{}' did not match any file(s) known to git", item.raw());
                missing = true;
            }
        }
        if missing {
            eprintln!("Did you forget to 'git add'?");
            return Ok(ExitCode::from(1));
        }
    }
    Ok(ExitCode::SUCCESS)
}
