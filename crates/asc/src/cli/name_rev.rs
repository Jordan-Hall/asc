//! `name-rev` (SPEC §11.2): names commits by the nearest ref that reaches them, `tip`, `tip~3`,
//! `tip~3^2`, with git's preference: a tag over any other ref, then the shorter name, then the
//! older tag. Commits dated before the oldest commit named (less a day) are not walked, as git
//! does, so a skewed history can be named differently than its shape suggests. `--name-only`, `--tags`,
//! `--refs=<pattern>`, `--exclude=<pattern>`, `--always`, `--[no-]undefined` and
//! `--annotate-stdin` are served; `--all` is not.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, Read as _, Write as _};
use std::process::ExitCode;
use std::sync::Arc;

use asc_core::id::{ChangesetId, TagId};

use super::common::{CliError, Ctx, Result, WsCtx};
use super::conflict;
use super::log::refs::{RefTip, tips};
use super::plumbing::objects::{Object, Resolver};
use super::refs_plumbing::args::{Cursor, Word, parse_oid};
use crate::native::NativeSpec;

/// A second parent costs this much extra distance (git's `MERGE_TRAVERSAL_WEIGHT`).
const MERGE_TRAVERSAL_WEIGHT: u64 = 65535;
/// Commits older than the oldest target by more than this are not walked past (git's slop).
const CUTOFF_SLOP: i64 = 86400;
/// Most name updates one run makes.
const MAX_STEPS: usize = 200_000_000;
/// Longest line read in `--annotate-stdin`.
const MAX_LINE: usize = 16 << 20;

const USAGE: &str = "usage: git name-rev [<options>] <commit>...
   or: git name-rev [<options>] --all
   or: git name-rev [<options>] --annotate-stdin

    --[no-]name-only      print only ref-based names (no object names)
    --[no-]tags           only use tags to name the commits
    --[no-]refs <pattern> only use refs matching <pattern>
    --[no-]exclude <pattern>
                          ignore refs matching <pattern>

    --[no-]all            list all commits reachable from all refs
    --[no-]annotate-stdin annotate text from stdin
    --[no-]undefined      allow to print `undefined` names (default)
    --[no-]always         show abbreviated commit object as fallback
";

#[derive(clap::Parser, Debug)]
#[command(
    name = "name-rev",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "name-rev",
    flags: &[
        "--name-only",
        "--no-name-only",
        "--tags",
        "--no-tags",
        "--refs",
        "--no-refs",
        "--exclude",
        "--no-exclude",
        "--annotate-stdin",
        "--no-annotate-stdin",
        "--undefined",
        "--no-undefined",
        "--always",
        "--no-always",
    ],
    placeholders: &[],
}];

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

#[derive(Default)]
struct Opts {
    name_only: bool,
    tags: bool,
    refs: Vec<String>,
    excludes: Vec<String>,
    annotate_stdin: bool,
    no_undefined: bool,
    /// Accepted; git prints `undefined` for an unnamed commit either way.
    #[allow(dead_code)]
    always: bool,
    operands: Vec<String>,
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, USAGE);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.operands.extend(cur.rest()),
            Word::Operand(p) => o.operands.push(p),
            Word::Short(f) => {
                return Err(cur.usage_error(&format!("unknown switch `{}'", &f[..1])));
            }
            Word::Long(name, inline) => match name.as_str() {
                "name-only" => o.name_only = true,
                "no-name-only" => o.name_only = false,
                "tags" => o.tags = true,
                "no-tags" => o.tags = false,
                "refs" => o.refs.push(cur.value("refs", inline)?),
                "no-refs" => o.refs.clear(),
                "exclude" => o.excludes.push(cur.value("exclude", inline)?),
                "no-exclude" => o.excludes.clear(),
                "annotate-stdin" => o.annotate_stdin = true,
                "no-annotate-stdin" => o.annotate_stdin = false,
                "undefined" => o.no_undefined = false,
                "no-undefined" => o.no_undefined = true,
                "always" => o.always = true,
                "no-always" => o.always = false,
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

/// A wildmatch of `pat` against `text` (`*` and `?` cross `/`).
fn glob(pat: &[u8], text: &[u8]) -> bool {
    match pat.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|i| glob(rest, &text[i..])),
        Some((b'?', rest)) => text.split_first().is_some_and(|(_, t)| glob(rest, t)),
        Some((c, rest)) => text
            .split_first()
            .is_some_and(|(t, tail)| t == c && glob(rest, tail)),
    }
}

/// Whether `pattern` matches `refname` or the part after any `/` of it (git's
/// `subpath_matches`).
fn subpath_matches(refname: &str, pattern: &str) -> bool {
    let mut at = 0;
    loop {
        if glob(pattern.as_bytes(), &refname.as_bytes()[at..]) {
            return true;
        }
        match refname[at..].find('/') {
            Some(i) => at += i + 1,
            None => return false,
        }
    }
}

/// A ref that names commits.
struct Tip {
    /// `master`, `tags/v1`, `remotes/origin/dev`.
    name: String,
    commit: ChangesetId,
    tag: Option<TagId>,
    from_tag: bool,
    taggerdate: i64,
}

async fn collect_tips(ws: &WsCtx, o: &Opts) -> Result<Vec<Tip>> {
    let raw: Vec<RefTip> = tips(ws, "refs/").await?;
    let mut out = Vec::new();
    for t in raw.iter().cloned() {
        if o.tags && !t.name.starts_with("refs/tags/") {
            continue;
        }
        if !o.refs.is_empty() && !o.refs.iter().any(|p| subpath_matches(&t.name, p)) {
            continue;
        }
        if o.excludes.iter().any(|p| subpath_matches(&t.name, p)) {
            continue;
        }
        let from_tag = t.name.starts_with("refs/tags/");
        // Tag names lose `tags/` only for `--tags --name-only`, and only when no branch
        // would then share the name.
        let shorten = o.tags && o.name_only;
        let name = if let Some(rest) = t.name.strip_prefix("refs/heads/") {
            rest.to_owned()
        } else if let Some(rest) = t.name.strip_prefix("refs/tags/")
            && shorten
            && !raw.iter().any(|r| r.name == format!("refs/heads/{rest}"))
        {
            rest.to_owned()
        } else {
            t.name.strip_prefix("refs/").unwrap_or(&t.name).to_owned()
        };
        let taggerdate = match t.tag {
            Some(id) => super::log::refs::tag_record(ws, id)
                .await?
                .tagger
                .map_or(i64::MAX, |s| s.time()),
            None if from_tag => ws.engine.get_changeset(t.commit).await?.committer.time(),
            None => i64::MAX,
        };
        out.push(Tip {
            name,
            commit: t.commit,
            tag: t.tag,
            from_tag,
            taggerdate,
        });
    }
    // Tags first, oldest first; the sort is stable so equal tips keep ref order.
    out.sort_by(|a, b| {
        b.from_tag
            .cmp(&a.from_tag)
            .then(a.taggerdate.cmp(&b.taggerdate))
    });
    Ok(out)
}

#[derive(Clone)]
struct Name {
    tip: Arc<str>,
    taggerdate: i64,
    generation: u64,
    distance: u64,
    from_tag: bool,
}

fn effective(distance: u64, generation: u64) -> u64 {
    distance
        + if generation > 0 {
            MERGE_TRAVERSAL_WEIGHT
        } else {
            0
        }
}

/// Whether the candidate beats the name a commit already has.
fn is_better(old: &Name, taggerdate: i64, generation: u64, distance: u64, from_tag: bool) -> bool {
    let old_d = effective(old.distance, old.generation);
    let new_d = effective(distance, generation);
    if old.from_tag != from_tag {
        // A tag always wins over another kind of ref, however far it is.
        return from_tag;
    }
    if new_d != old_d {
        return new_d < old_d;
    }
    old.taggerdate > taggerdate
}

/// Names every commit reachable from the tips, down to the cutoff.
async fn name_all(
    ws: &WsCtx,
    tips: &[Tip],
    cutoff: Option<i64>,
) -> Result<HashMap<ChangesetId, Name>> {
    let mut names: HashMap<ChangesetId, Name> = HashMap::new();
    let mut steps = 0usize;
    for tip in tips {
        let tip_name: Arc<str> = if tip.tag.is_some() {
            format!("{}^0", tip.name).into()
        } else {
            tip.name.as_str().into()
        };
        // (commit, tip name, generation, distance)
        let mut stack: Vec<(ChangesetId, Arc<str>, u64, u64)> = vec![(tip.commit, tip_name, 0, 0)];
        while let Some((id, name, generation, distance)) = stack.pop() {
            steps += 1;
            if steps > MAX_STEPS {
                return Err(fatal("asc: name-rev: history is too large to name"));
            }
            let cs = ws.engine.get_changeset(id).await?;
            if let Some(cut) = cutoff
                && cs.committer.time() < cut - CUTOFF_SLOP
            {
                continue;
            }
            if let Some(old) = names.get(&id)
                && !is_better(old, tip.taggerdate, generation, distance, tip.from_tag)
            {
                continue;
            }
            names.insert(
                id,
                Name {
                    tip: name.clone(),
                    taggerdate: tip.taggerdate,
                    generation,
                    distance,
                    from_tag: tip.from_tag,
                },
            );
            let base = name.strip_suffix("^0").unwrap_or(&name);
            let mut pushes: Vec<(ChangesetId, Arc<str>, u64, u64)> = Vec::new();
            for (i, parent) in cs.parents.iter().enumerate() {
                if i == 0 {
                    pushes.push((*parent, name.clone(), generation + 1, distance + 1));
                } else {
                    let derived = if generation > 0 {
                        format!("{base}~{generation}^{}", i + 1)
                    } else {
                        format!("{base}^{}", i + 1)
                    };
                    pushes.push((
                        *parent,
                        derived.into(),
                        0,
                        distance + MERGE_TRAVERSAL_WEIGHT,
                    ));
                }
            }
            // First parent first.
            stack.extend(pushes.into_iter().rev());
        }
    }
    Ok(names)
}

fn rendered(name: &Name) -> String {
    if name.generation == 0 {
        name.tip.to_string()
    } else {
        let base = name.tip.strip_suffix("^0").unwrap_or(&name.tip);
        format!("{base}~{}", name.generation)
    }
}

/// The operands as commits: the word as typed, the commit, and the tag object when the word
/// names one.
async fn operands(
    ctx: &Ctx,
    ws: &WsCtx,
    words: &[String],
) -> Result<Vec<(String, Option<(ChangesetId, Option<TagId>)>)>> {
    let res = Resolver::new(ctx, ws);
    let mut out = Vec::new();
    for w in words {
        let found = match res.resolve(w).await {
            Ok(Some(Object::Commit { id, .. })) => Some((id, None)),
            Ok(Some(obj @ Object::Tag { id, .. })) => {
                match res.peel(obj, asc_core::git::ObjectKind::Commit).await? {
                    Some(Object::Commit { id: c, .. }) => Some((c, Some(id))),
                    _ => None,
                }
            }
            _ => None,
        };
        out.push((w.clone(), found));
    }
    Ok(out)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = parse(&a.rest)?;
    if o.annotate_stdin && !o.operands.is_empty() {
        return Err(cur_usage());
    }
    if !o.annotate_stdin && o.operands.is_empty() {
        // git names nothing and succeeds.
        return Ok(ExitCode::SUCCESS);
    }
    let ws = ctx.ws()?;
    let tips = collect_tips(ws, &o).await?;
    let mut out = std::io::stdout().lock();

    if o.annotate_stdin {
        let names = name_all(ws, &tips, None).await?;
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = (&mut reader)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            if line.len() > MAX_LINE {
                return Err(fatal("asc: name-rev: input line is too long"));
            }
            let text = annotate_line(ws, &line, &names, &o).await?;
            out.write_all(&text)?;
        }
        out.flush()?;
        return Ok(ExitCode::SUCCESS);
    }

    let targets = operands(ctx, ws, &o.operands).await?;
    let mut cutoff: Option<i64> = None;
    for (_, found) in &targets {
        if let Some((id, _)) = found {
            let t = ws.engine.get_changeset(*id).await?.committer.time();
            cutoff = Some(cutoff.map_or(t, |c| c.min(t)));
        }
    }
    let names = name_all(ws, &tips, cutoff).await?;
    for (word, found) in targets {
        let Some((id, tag)) = found else {
            eprintln!("Could not get sha1 for {word}. Skipping.");
            continue;
        };
        // A tag object is named by the ref that holds it, with no `^0`.
        let exact = tag.and_then(|t| {
            tips.iter()
                .find(|x| x.tag == Some(t))
                .map(|x| x.name.clone())
        });
        let name = exact.or_else(|| names.get(&id).map(rendered));
        let line = match name {
            Some(n) if o.name_only => n,
            Some(n) => format!("{word} {n}"),
            None if o.no_undefined => {
                // git has printed the word by the time it finds there is no name.
                if !o.name_only {
                    write!(out, "{word} ")?;
                }
                out.flush()?;
                let full = conflict::git_id(ws, id).await?;
                return Err(fatal(format!("cannot describe '{full}'")));
            }
            None if o.name_only => "undefined".to_owned(),
            None => format!("{word} undefined"),
        };
        writeln!(out, "{line}")?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn cur_usage() -> CliError {
    CliError::Usage(USAGE.trim_end().to_owned())
}

/// One input line with every full commit id followed by its name (or replaced by it).
async fn annotate_line(
    ws: &WsCtx,
    line: &[u8],
    names: &HashMap<ChangesetId, Name>,
    o: &Opts,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(line.len() + 32);
    let mut i = 0;
    while i < line.len() {
        if line[i].is_ascii_hexdigit() {
            let start = i;
            while i < line.len() && line[i].is_ascii_hexdigit() {
                i += 1;
            }
            let run = &line[start..i];
            let named = if run.len() == 40
                && let Ok(text) = std::str::from_utf8(run)
                && let Some(oid) = parse_oid(text)
                && let Ok(id) = conflict::changeset_of(ws, oid).await
            {
                names.get(&id).map(rendered)
            } else {
                None
            };
            match named {
                Some(n) if o.name_only => out.extend_from_slice(n.as_bytes()),
                Some(n) => {
                    out.extend_from_slice(run);
                    out.extend_from_slice(format!(" ({n})").as_bytes());
                }
                None => out.extend_from_slice(run),
            }
        } else {
            out.push(line[i]);
            i += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(tip: &str, generation: u64, distance: u64, from_tag: bool, date: i64) -> Name {
        Name {
            tip: tip.into(),
            taggerdate: date,
            generation,
            distance,
            from_tag,
        }
    }

    #[test]
    fn subpaths_match_like_git() {
        assert!(subpath_matches("refs/heads/master", "master"));
        assert!(subpath_matches("refs/heads/master", "heads/*"));
        assert!(subpath_matches("refs/tags/v1", "refs/tags/v*"));
        assert!(!subpath_matches("refs/heads/master", "aster"));
    }

    #[test]
    fn a_tag_beats_a_nearer_branch() {
        let br = name("br", 0, 0, false, i64::MAX);
        assert!(is_better(&br, 5, 5, 5, true));
        let tag = name("t", 5, 5, true, 5);
        assert!(!is_better(&tag, i64::MAX, 0, 0, false));
    }

    #[test]
    fn shorter_then_older() {
        let a = name("a", 3, 3, true, 10);
        assert!(is_better(&a, 99, 1, 1, true));
        assert!(is_better(&a, 5, 3, 3, true));
        assert!(!is_better(&a, 20, 3, 3, true));
    }

    #[test]
    fn names_render_with_generations() {
        assert_eq!(rendered(&name("tags/v2^0", 0, 0, true, 0)), "tags/v2^0");
        assert_eq!(rendered(&name("tags/v2^0", 2, 2, true, 0)), "tags/v2~2");
        assert_eq!(rendered(&name("feat^2", 1, 1, false, 0)), "feat^2~1");
    }
}
