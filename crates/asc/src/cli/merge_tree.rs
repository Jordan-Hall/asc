//! `merge-tree --write-tree` (SPEC §11.2): merges two commits without touching the work tree,
//! the index or any ref, over the same three-way merge `merge` uses. Prints the merged tree's id,
//! then (on conflict) the conflicted stage entries and the informational messages, git's format.
//! The old trivial-merge form and `--stdin` are not served.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, TreeId};
use asc_core::path::RepoPath;
use asc_engine::history::HistoryApi;
use asc_engine::merge::{ConflictKind, Labels, MergeOutcome};

use super::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use super::conflict::{self, Sides};
use super::plumbing::objects::{Object, Resolver};
use super::refs_plumbing::args::{Cursor, Word};
use super::restore::root_of;
use crate::native::NativeSpec;

const USAGE: &str = "usage: git merge-tree [--write-tree] [<options>] <branch1> <branch2>
   or: git merge-tree [--trivial-merge] <base-tree> <branch1> <branch2>

    --write-tree          do a real merge instead of a trivial merge
    --trivial-merge       do a trivial merge only
    --[no-]messages       also show informational/conflict messages
    --quiet               suppress all output; only exit status wanted
    -z                    separate paths with the NUL character
    --name-only           list filenames without modes/oids/stages
    --allow-unrelated-histories
                          allow merging unrelated histories
    --stdin               perform multiple merges, one per line of input
    --[no-]merge-base <tree-ish>
                          specify a merge-base for the merge
    -X, --[no-]strategy-option <option=value>
                          option for selected merge strategy
";

#[derive(clap::Parser, Debug)]
#[command(
    name = "merge-tree",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "merge-tree",
    flags: &[
        "--write-tree",
        "--messages",
        "--no-messages",
        "--quiet",
        "-z",
        "--name-only",
        "--allow-unrelated-histories",
        "--merge-base",
        "-X",
        "--strategy-option",
    ],
    placeholders: &[],
}];

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

#[derive(Default)]
struct Opts {
    messages: Option<bool>,
    quiet: bool,
    nul: bool,
    name_only: bool,
    unrelated: bool,
    merge_base: Option<String>,
    xopts: Vec<String>,
    operands: Vec<String>,
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, USAGE);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.operands.extend(cur.rest()),
            Word::Operand(p) => o.operands.push(p),
            Word::Short(f) => match f.split_at(1) {
                ("z", "") => o.nul = true,
                ("X", rest) => {
                    let inline = (!rest.is_empty()).then(|| rest.to_owned());
                    o.xopts.push(cur.value("X", inline)?);
                }
                (c, _) => return Err(cur.usage_error(&format!("unknown switch `{c}'"))),
            },
            Word::Long(name, inline) => match name.as_str() {
                "write-tree" => {}
                "messages" => o.messages = Some(true),
                "no-messages" => o.messages = Some(false),
                "quiet" => o.quiet = true,
                "name-only" => o.name_only = true,
                "allow-unrelated-histories" => o.unrelated = true,
                "merge-base" => o.merge_base = Some(cur.value("merge-base", inline)?),
                "no-merge-base" => o.merge_base = None,
                "strategy-option" => o.xopts.push(cur.value("strategy-option", inline)?),
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

/// The commit a branch argument names.
async fn commit_named(res: &Resolver<'_>, word: &str) -> Result<Option<ChangesetId>> {
    let Some(obj) = res.resolve(word).await? else {
        return Ok(None);
    };
    Ok(match res.peel(obj, ObjectKind::Commit).await? {
        Some(Object::Commit { id, .. }) => Some(id),
        _ => None,
    })
}

/// The tree to merge over: one base's tree, the empty tree when there is none, or the virtual
/// base of several (their merge with markers kept, as git does).
async fn base_tree(
    ws: &WsCtx,
    bases: &[ChangesetId],
    opts: &asc_engine::merge::MergeOptions,
) -> Result<TreeId> {
    match bases {
        [] => Ok(CliEngine::empty_tree_id()),
        [one] => root_of(ws, Some(*one)).await,
        [first, rest @ ..] => {
            let mut acc = root_of(ws, Some(*first)).await?;
            let mut acc_commit = *first;
            for &next in rest {
                let common = ws
                    .engine
                    .merge_base(acc_commit, &[next], false)
                    .await
                    .map_err(conflict::fatal)?;
                let under = match common.first() {
                    Some(&c) => root_of(ws, Some(c)).await?,
                    None => CliEngine::empty_tree_id(),
                };
                let theirs = root_of(ws, Some(next)).await?;
                acc = ws
                    .engine
                    .merge_trees(under, acc, theirs, opts)
                    .await
                    .map_err(conflict::fatal)?
                    .root;
                acc_commit = next;
            }
            Ok(acc)
        }
    }
}

/// The type string and path list of a message in the `-z` form.
fn typed_messages(
    outcome: &MergeOutcome,
    differ: &BTreeSet<RepoPath>,
) -> Vec<(RepoPath, String, String)> {
    let mut out: Vec<(RepoPath, String, String)> = Vec::new();
    let mut merged = differ.clone();
    for c in &outcome.conflicts {
        if matches!(
            c.kind,
            ConflictKind::Content | ConflictKind::AddAdd | ConflictKind::Binary
        ) {
            merged.insert(c.path.clone());
        }
    }
    for p in &merged {
        out.push((
            p.clone(),
            "Auto-merging".to_owned(),
            format!("Auto-merging {p}\n"),
        ));
    }
    out
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = parse(&a.rest)?;
    if o.operands.len() != 2 {
        return Err(CliError::Usage(USAGE.to_owned()));
    }
    let ws = ctx.ws()?;
    let res = Resolver::new(ctx, ws);
    let mut sides_ids = Vec::new();
    for word in &o.operands {
        match commit_named(&res, word).await? {
            Some(id) => sides_ids.push(id),
            None => {
                return Err(CliError::Raw(
                    format!("merge-tree: {word} - not something we can merge"),
                    1,
                ));
            }
        }
    }
    let (ours, theirs) = (sides_ids[0], sides_ids[1]);
    let favor = conflict::favor_of(&o.xopts)?;

    let (bases, base_label): (Vec<ChangesetId>, String) = match &o.merge_base {
        Some(word) => {
            let Some(obj) = res.resolve(word).await? else {
                return Err(fatal(format!("could not parse object '{word}'")));
            };
            match res.peel(obj, ObjectKind::Commit).await? {
                Some(Object::Commit { id, oid }) => (vec![id], oid.to_string()[..7].to_owned()),
                _ => (Vec::new(), "base".to_owned()),
            }
        }
        None => {
            let bases = ws
                .engine
                .merge_base(ours, &[theirs], true)
                .await
                .map_err(conflict::fatal)?;
            if bases.is_empty() && !o.unrelated {
                return Err(fatal("refusing to merge unrelated histories"));
            }
            let label = match bases.as_slice() {
                [one] => conflict::git_id(ws, *one).await?.to_string()[..7].to_owned(),
                _ => "merged common ancestors".to_owned(),
            };
            (bases, label)
        }
    };
    let (ours_root, theirs_root) = (
        root_of(ws, Some(ours)).await?,
        root_of(ws, Some(theirs)).await?,
    );
    let (base_root, labels) = if let Some(word) = &o.merge_base
        && bases.is_empty()
    {
        // A tree-ish that is not a commit.
        let root = match res.resolve(word).await? {
            Some(obj) => match res.peel(obj, ObjectKind::Tree).await? {
                Some(Object::Tree { id, .. }) => id,
                _ => return Err(fatal(format!("could not parse object '{word}'"))),
            },
            None => return Err(fatal(format!("could not parse object '{word}'"))),
        };
        (
            root,
            Labels {
                base: base_label,
                ours: o.operands[0].clone(),
                theirs: o.operands[1].clone(),
            },
        )
    } else {
        let labels = Labels {
            base: base_label,
            ours: o.operands[0].clone(),
            theirs: o.operands[1].clone(),
        };
        let plain = conflict::merge_options(
            ctx,
            ws,
            &Sides {
                base: ours_root,
                ours: ours_root,
                theirs: ours_root,
            },
            favor,
            labels.clone(),
        )
        .await?
        .0;
        (base_tree(ws, &bases, &plain).await?, labels)
    };
    let sides = Sides {
        base: base_root,
        ours: ours_root,
        theirs: theirs_root,
    };
    let (opts, differ) = conflict::merge_options(ctx, ws, &sides, favor, labels).await?;
    let outcome = ws
        .engine
        .merge_trees(base_root, ours_root, theirs_root, &opts)
        .await
        .map_err(conflict::fatal)?;
    conflict::refuse_unsupported(&outcome)?;
    let tree = res.tree(outcome.root).await?;
    res.flush().await?;
    let clean = outcome.is_clean();
    if o.quiet {
        return Ok(ExitCode::from(u8::from(!clean)));
    }

    let term: &[u8] = if o.nul { b"\0" } else { b"\n" };
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(tree.oid().to_string().as_bytes());
    out.extend_from_slice(term);
    if !clean {
        let mut conflicts: Vec<&asc_engine::merge::Conflict> = outcome.conflicts.iter().collect();
        conflicts.sort_by(|a, b| a.path.cmp(&b.path));
        if o.name_only {
            let mut paths: Vec<&RepoPath> = conflicts.iter().map(|c| &c.path).collect();
            paths.dedup();
            for p in paths {
                out.extend_from_slice(&maybe_quote(p, o.nul));
                out.extend_from_slice(term);
            }
        } else {
            for c in conflicts {
                for (stage, entry) in [
                    (1, &c.stages.base),
                    (2, &c.stages.ours),
                    (3, &c.stages.theirs),
                ] {
                    let Some(e) = entry else { continue };
                    out.extend_from_slice(
                        format!("{:o} {} {stage}\t", asc_engine::diff::mode(e.kind), e.git)
                            .as_bytes(),
                    );
                    out.extend_from_slice(&maybe_quote(&c.path, o.nul));
                    out.extend_from_slice(term);
                }
            }
        }
    }
    if o.messages.unwrap_or(!clean) {
        out.extend_from_slice(term);
        if o.nul {
            write_nul_messages(&mut out, &outcome, &differ, &o.operands);
        } else {
            for line in conflict::messages(&outcome, &differ, &o.operands[0], &o.operands[1]) {
                out.extend_from_slice(line.as_bytes());
                out.push(b'\n');
            }
        }
    }
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&out)?;
    stdout.flush()?;
    Ok(ExitCode::from(u8::from(!clean)))
}

/// A path as git prints it: C-quoted unless `-z`.
fn maybe_quote(path: &RepoPath, nul: bool) -> Vec<u8> {
    let bytes = path.as_bytes();
    if nul
        || !bytes
            .iter()
            .any(|&b| b < 0x20 || b == b'"' || b == b'\\' || b >= 0x80)
    {
        return bytes.to_vec();
    }
    let mut out = vec![b'"'];
    for &b in bytes {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b if !(0x20..0x80).contains(&b) => {
                out.extend_from_slice(format!("\\{b:03o}").as_bytes())
            }
            b => out.push(b),
        }
    }
    out.push(b'"');
    out
}

/// The messages in the `-z` form: per message, the path count, the paths, the type and the text,
/// each ended by NUL.
fn write_nul_messages(
    out: &mut Vec<u8>,
    outcome: &MergeOutcome,
    differ: &BTreeSet<RepoPath>,
    names: &[String],
) {
    let mut entries: Vec<(RepoPath, usize, String, String)> = Vec::new();
    for (path, kind, text) in typed_messages(outcome, differ) {
        entries.push((path, 0, kind, text));
    }
    for c in &outcome.conflicts {
        let (kind, text) = match c.kind {
            ConflictKind::Content | ConflictKind::Binary => (
                "CONFLICT (contents)",
                format!("CONFLICT (content): Merge conflict in {}\n", c.path),
            ),
            ConflictKind::AddAdd => (
                "CONFLICT (contents)",
                format!("CONFLICT (add/add): Merge conflict in {}\n", c.path),
            ),
            ConflictKind::ModifyDelete => {
                let (deleted, modified) = if c.stages.ours.is_none() {
                    (&names[0], &names[1])
                } else {
                    (&names[1], &names[0])
                };
                (
                    "CONFLICT (modify/delete)",
                    format!(
                        "CONFLICT (modify/delete): {p} deleted in {deleted} and modified in {modified}.  Version {modified} of {p} left in tree.\n",
                        p = c.path
                    ),
                )
            }
            ConflictKind::FileDir | ConflictKind::CaseFold => continue,
        };
        entries.push((c.path.clone(), 1, kind.to_owned(), text));
    }
    entries.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    for (path, _, kind, text) in entries {
        out.extend_from_slice(b"1\0");
        out.extend_from_slice(path.as_bytes());
        out.push(0);
        out.extend_from_slice(kind.as_bytes());
        out.push(0);
        out.extend_from_slice(text.as_bytes());
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_follows_git() {
        let plain = RepoPath::new(b"a/b".to_vec()).unwrap();
        assert_eq!(maybe_quote(&plain, false), b"a/b");
        let odd = RepoPath::new(b"a\tb".to_vec()).unwrap();
        assert_eq!(maybe_quote(&odd, false), b"\"a\\tb\"");
        assert_eq!(maybe_quote(&odd, true), b"a\tb");
    }

    #[test]
    fn options_parse() {
        let args: Vec<OsString> = ["--write-tree", "-z", "--merge-base=x", "a", "b"]
            .iter()
            .map(OsString::from)
            .collect();
        let o = parse(&args).unwrap();
        assert!(o.nul);
        assert_eq!(o.merge_base.as_deref(), Some("x"));
        assert_eq!(o.operands, ["a", "b"]);
    }
}
