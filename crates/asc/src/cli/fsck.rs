//! `asc fsck` (SPEC §3.5, §11.2): verifies what asc keeps, offline.
//!
//! * **Objects.** Every object in `store/` and `cache/` is read back through a read-only tier
//!   (nothing is deleted or repaired) and checked against its key: the codec frame, the size cap
//!   and the BLAKE3 or git hash. A corrupt object in `cache/` is recoverable (`asc gc` or the next
//!   read evicts it and a remote refills it); one in `store/` may be the only copy.
//! * **Refs and generations.** Inside a workspace, every ref must name a readable changeset and
//!   the `cs_gen` rows of the changesets reachable from the refs are re-derived from their
//!   parents: a missing row is written, a row whose parents or generation disagree with the
//!   changeset is reported. History beyond the fetch window (missing parents) is a boundary,
//!   not an error.
//! * **Workspaces.** A registered workspace whose directory is gone is a warning.
//!
//! Output follows `git fsck`: silent when clean, `error: …` lines on stderr and exit 1 when
//! something is wrong. `-v` adds a summary on stdout. Every walk is bounded.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use asc_core::codec::decode_envelope;
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::object::TagRecord;
use asc_engine::commit::Error as CommitError;
use asc_state::AscHome;
use asc_state::state::tables::cs_gen::{self, CsGen};
use asc_state::state::workspaces;
use asc_store::cachedir::CacheDir;
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::Objects;
use asc_store::refs::Refs as _;
use futures::StreamExt;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::refs_plumbing::args::{Cursor, Word};
use crate::native::NativeSpec;

const USAGE: &str = "usage: git fsck [<options>] [<object>...]

    -v, --[no-]verbose    be verbose
    --[no-]dangling       show dangling objects
    --[no-]progress       show progress
    --full                also check the object stores the workspace borrows
    --strict              enable more strict checking
";

/// Most changesets the generation walk holds in memory.
const MAX_NODES: usize = 4_000_000;

/// Concurrent object reads while verifying a tier.
const READS: usize = 8;

/// Rows written per transaction when generations are re-derived.
const ROW_BATCH: usize = 1_000;

/// Most error lines printed per category before the rest are only counted.
const MAX_LISTED: usize = 100;

#[derive(clap::Parser, Debug)]
#[command(name = "fsck", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "fsck",
    flags: &[
        "-v",
        "--verbose",
        "--no-verbose",
        "--dangling",
        "--no-dangling",
        "--progress",
        "--no-progress",
        "--full",
        "--strict",
    ],
    placeholders: &[],
}];

// ------------------------------------------------------------------ generations

/// What a generation re-derivation found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct GenReport {
    /// Changesets read.
    pub visited: usize,
    /// Rows written because none existed.
    pub inserted: usize,
    /// Changesets whose generation cannot be derived (an ancestor is not local).
    pub unresolved: usize,
    /// Rows that disagree with their changeset, as report lines.
    pub mismatches: Vec<String>,
    /// Changesets that exist but cannot be read, as report lines.
    pub unreadable: Vec<String>,
}

/// Re-derives `cs_gen` for the changesets reachable from `tips` (SPEC §6.3): `gen` is 1 for a
/// root and 1 + the largest parent generation otherwise. A parent's generation is its stored row
/// when there is one, else the value derived here, so history behind the fetch window (whose
/// changesets are not local) still checks against the rows `fetch` wrote.
pub(crate) async fn rederive_gen(ws: &WsCtx, tips: &[ChangesetId]) -> Result<GenReport> {
    let (db, repo) = (&ws.db, ws.repo.key);
    let mut report = GenReport::default();
    let mut parents_of: HashMap<ChangesetId, Vec<ChangesetId>> = HashMap::new();
    let mut seen: HashSet<ChangesetId> = HashSet::new();
    let mut post: Vec<ChangesetId> = Vec::new();
    let mut stack: Vec<(ChangesetId, bool)> = tips.iter().rev().map(|t| (*t, false)).collect();
    while let Some((cs, expanded)) = stack.pop() {
        if expanded {
            post.push(cs);
            continue;
        }
        if !seen.insert(cs) {
            continue;
        }
        if seen.len() > MAX_NODES {
            return Err(CliError::Fatal(format!(
                "asc: more than {MAX_NODES} changesets are reachable; generations are not re-derived"
            )));
        }
        match ws.engine.get_changeset(cs).await {
            Ok(c) => {
                stack.push((cs, true));
                for p in c.parents.iter().rev() {
                    if !seen.contains(p) {
                        stack.push((*p, false));
                    }
                }
                parents_of.insert(cs, c.parents.clone());
            }
            Err(CommitError::MissingChangeset(_)) => {}
            Err(e) => report.unreadable.push(format!("changeset {cs}: {e}")),
        }
    }
    report.visited = parents_of.len();

    let mut value: HashMap<ChangesetId, u64> = HashMap::new();
    let mut pending: Vec<CsGen> = Vec::new();
    for cs in post {
        let Some(parents) = parents_of.get(&cs) else {
            continue;
        };
        let mut expected = Some(0u64);
        for p in parents {
            let known = match value.get(p) {
                Some(v) => Some(*v),
                None => cs_gen::get(db, repo, *p).await?.map(|r| r.gen_no),
            };
            expected = match (expected, known) {
                (Some(m), Some(v)) => Some(m.max(v)),
                _ => None,
            };
        }
        let expected = expected.map(|m| m + 1);
        match cs_gen::get(db, repo, cs).await? {
            Some(row) => {
                if row.parents != *parents {
                    report
                        .mismatches
                        .push(format!("cs_gen {cs}: parents differ from the changeset"));
                } else if expected.is_some_and(|e| e != row.gen_no) {
                    report.mismatches.push(format!(
                        "cs_gen {cs}: generation {} but its parents give {}",
                        row.gen_no,
                        expected.unwrap_or_default()
                    ));
                }
                value.insert(cs, row.gen_no);
            }
            None => match expected {
                Some(gen_no) => {
                    value.insert(cs, gen_no);
                    pending.push(CsGen {
                        cs,
                        gen_no,
                        parents: parents.clone(),
                    });
                    if pending.len() >= ROW_BATCH {
                        report.inserted += pending.len();
                        cs_gen::insert_many(db, repo, &std::mem::take(&mut pending)).await?;
                    }
                }
                None => report.unresolved += 1,
            },
        }
    }
    if !pending.is_empty() {
        report.inserted += pending.len();
        cs_gen::insert_many(db, repo, &pending).await?;
    }
    Ok(report)
}

/// The changeset a ref target names (an annotated tag is peeled), `None` when the tag record is
/// not local.
pub(crate) async fn tip_of(ws: &WsCtx, target: RefTarget) -> Result<Option<ChangesetId>> {
    Ok(match target {
        RefTarget::Changeset(c) => Some(c),
        RefTarget::Tag(t) => match ws.objects.get(&ObjKey::Tag(t)).await {
            Ok(Some(data)) => decode_envelope::<TagRecord>(&data).ok().map(|r| r.target),
            _ => None,
        },
    })
}

// ------------------------------------------------------------------ objects

/// What verifying one tier found.
#[derive(Debug, Default)]
struct TierReport {
    checked: u64,
    bad: Vec<String>,
}

/// Reads every object of the tier at `root` back and checks it against its key. The tier is
/// opened read-only, so a corrupt file is reported and left in place.
async fn verify_tier(root: &std::path::Path) -> Result<TierReport> {
    let mut out = TierReport::default();
    if !root.is_dir() {
        return Ok(out);
    }
    let tier = CacheDir::open_read_only(root).map_err(CliError::Objects)?;
    let mut checks = tier
        .list(KeyPrefix::all())
        .map(|listed| {
            let tier = &tier;
            async move {
                let (key, _) = listed?;
                let read = tier.get(&key).await;
                Ok::<_, asc_store::objects::Error>((key, read))
            }
        })
        .buffer_unordered(READS);
    while let Some(next) = checks.next().await {
        let (key, read) = next.map_err(CliError::Objects)?;
        out.checked += 1;
        match read {
            Ok(_) => {}
            Err(e) => out.bad.push(format!("{}: {e}", key.path())),
        }
    }
    Ok(out)
}

/// The cache directory: `asc.cache.dir` (with `~/`), else `<home>/cache`.
fn cache_root(ctx: &Ctx, home: &AscHome) -> PathBuf {
    match ctx.config.get("asc.cache.dir") {
        Some(p) => match (p.strip_prefix("~/"), std::env::home_dir()) {
            (Some(rest), Some(h)) => h.join(rest),
            _ => PathBuf::from(p),
        },
        None => home.cache(),
    }
}

// ------------------------------------------------------------------ command

#[derive(Debug, Default, PartialEq, Eq)]
struct Opts {
    verbose: bool,
}

fn parse(rest: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(rest, USAGE);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => {
                if cur.remaining() > 0 {
                    return Err(cur.usage_only());
                }
            }
            // git takes object names; asc checks everything it keeps and has none to select.
            Word::Operand(_) => return Err(cur.usage_only()),
            Word::Short(flags) => {
                for c in flags.chars() {
                    match c {
                        'v' => o.verbose = true,
                        other => {
                            return Err(cur.usage_error(&format!("unknown switch `{other}'")));
                        }
                    }
                }
            }
            Word::Long(name, _) => match name.as_str() {
                "verbose" => o.verbose = true,
                "no-verbose" => o.verbose = false,
                "dangling" | "no-dangling" | "progress" | "no-progress" | "full" | "strict" => {}
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

/// Prints `lines` as `error: …`, at most [`MAX_LISTED`] of them.
fn print_errors(lines: &[String]) {
    for l in lines.iter().take(MAX_LISTED) {
        eprintln!("error: {l}");
    }
    if lines.len() > MAX_LISTED {
        eprintln!("error: … and {} more", lines.len() - MAX_LISTED);
    }
}

async fn check_workspace(ws: &WsCtx, errors: &mut Vec<String>, o: &Opts) -> Result<()> {
    let refs = ws.db.refs();
    let listed = refs.list(ws.repo.key, "refs/").await?;
    let mut tips = Vec::new();
    for entry in &listed {
        match tip_of(ws, entry.target).await? {
            Some(cs) => {
                tips.push(cs);
            }
            None => errors.push(format!(
                "{}: the tag it names is not in the object stores",
                entry.name
            )),
        }
    }
    tips.sort();
    tips.dedup();
    let report = rederive_gen(ws, &tips).await?;
    for tip in &tips {
        if ws.engine.get_changeset(*tip).await.is_err() {
            errors.push(format!("ref target {tip}: the changeset cannot be read"));
        }
    }
    errors.extend(report.unreadable.iter().cloned());
    errors.extend(report.mismatches.iter().cloned());
    if o.verbose {
        println!(
            "checked {} changesets: {} generation rows written, {} not derivable (history is not all local)",
            report.visited, report.inserted, report.unresolved
        );
    }
    for w in workspaces::list(&ws.db, Some(ws.repo.key)).await? {
        if !std::path::Path::new(&w.ws.path).is_dir() {
            eprintln!("warning: workspace {} does not exist", w.ws.path);
        }
    }
    Ok(())
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let o = parse(&args.rest)?;
    let home = match ctx.ws_opt() {
        Some(ws) => ws.home.clone(),
        None => AscHome::locate()?,
    };
    let mut errors: Vec<String> = Vec::new();
    for (name, root) in [("store", home.store()), ("cache", cache_root(ctx, &home))] {
        let tier = verify_tier(&root).await?;
        if o.verbose {
            println!("checked {} objects in {name}/", tier.checked);
        }
        errors.extend(tier.bad.iter().map(|b| format!("{name}/{b}")));
    }
    if let Some(ws) = ctx.ws_opt() {
        check_workspace(ws, &mut errors, &o).await?;
    }
    print_errors(&errors);
    Ok(if errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn flags() {
        assert_eq!(parse(&os(&[])).unwrap(), Opts::default());
        assert!(
            parse(&os(&["-v", "--full", "--no-dangling"]))
                .unwrap()
                .verbose
        );
        assert!(parse(&os(&["--lost-found"])).is_err());
        assert!(parse(&os(&["HEAD"])).is_err());
    }
}
