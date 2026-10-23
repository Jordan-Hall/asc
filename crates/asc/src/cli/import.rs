//! `asc import --git <path|url> [--repo <name>] [--incremental]` (SPEC §14.1): imports a git
//! repository into the object stores of the current workspace's repository.
//!
//! The import is the git-remote machinery of U39 pointed at a private tracking namespace: a
//! hardened `git fetch` into the repository's bare scratch repo (`transport_url` refuses remote
//! helpers, every git child comes from `RealGit`), then an incremental import in topological
//! order whose every changeset must export back to the original commit bytes (SPEC §14.4). The
//! git map decides what is new, so every run is incremental and `--incremental` is accepted for
//! scripts that spell it out.
//!
//! Local refs: branches land in `refs/remotes/<name>/…` (`<name>` is `--repo`, default
//! `import`), so an import never moves a branch the workspace may have checked out; tags land in
//! `refs/tags/…` unless the tag already names something else, which is left alone with a
//! warning. Generations of the imported history are written to `cs_gen` (SPEC §14.1 step 5).

use std::ffi::OsString;
use std::process::ExitCode;

use asc_engine::sync::Remote as _;
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs as _};

use super::common::{CliError, Ctx, Result, StagingAfterMove};
use super::fsck::{rederive_gen, tip_of};
use super::refs_plumbing::args::{Cursor, Word};
use super::restore::actor;
use super::switch::ref_name;
use crate::native::NativeSpec;
use crate::remote::git::GitRemote;

const USAGE: &str = "usage: asc import --git <path|url> [--repo <name>] [--incremental]";

/// Longest `--repo` name.
const MAX_NAME: usize = 64;

#[derive(clap::Parser, Debug)]
#[command(name = "import", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "import",
    flags: &["--git", "--repo", "--incremental"],
    placeholders: &[],
}];

#[derive(Debug, PartialEq, Eq)]
struct Opts {
    source: String,
    name: String,
}

fn parse(rest: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(rest, USAGE);
    let (mut source, mut name) = (None, None);
    while let Some(word) = cur.next() {
        match word {
            Word::Long(n, inline) => match n.as_str() {
                "git" => source = Some(cur.value("git", inline)?),
                "repo" => name = Some(cur.value("repo", inline)?),
                "incremental" => {}
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
            Word::End | Word::Operand(_) | Word::Short(_) => return Err(cur.usage_only()),
        }
    }
    let Some(source) = source else {
        return Err(cur.usage_only());
    };
    let name = name.unwrap_or_else(|| "import".to_owned());
    if name.is_empty()
        || name.len() > MAX_NAME
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        || name.starts_with(['.', '-'])
        || name.contains("..")
        || name.ends_with(".lock")
    {
        return Err(CliError::Fatal(format!(
            "asc: '{name}' is not a usable import name"
        )));
    }
    Ok(Opts { source, name })
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let o = parse(&args.rest)?;
    let ws = ctx.ws()?;
    // A name of its own: the scratch repo's tracking namespace of a configured remote is never
    // shared with an import.
    let remote = GitRemote::new(ctx, ws, &format!("import-{}", o.name), &o.source)?;
    let entries = remote
        .list_refs()
        .await
        .map_err(|e| CliError::Fatal(format!("asc: import failed: {e}")))?;

    let refs = ws.db.refs();
    let mut updates = Vec::new();
    let mut tips = Vec::new();
    let (mut branches, mut tags, mut kept) = (0usize, 0usize, 0usize);
    for entry in &entries {
        let full = String::from_utf8_lossy(entry.name.as_bytes()).into_owned();
        if let Some(tip) = tip_of(ws, entry.target).await? {
            tips.push(tip);
        }
        let (local, expect) = if let Some(b) = full.strip_prefix("refs/heads/") {
            branches += 1;
            (format!("refs/remotes/{}/{b}", o.name), Expect::Any)
        } else if full.starts_with("refs/tags/") {
            tags += 1;
            (full.clone(), Expect::Absent)
        } else {
            continue;
        };
        let name = ref_name(&local)?;
        if let Some(existing) = refs.get(ws.repo.key, &name).await? {
            if existing.target == entry.target {
                continue;
            }
            if matches!(expect, Expect::Absent) {
                eprintln!(
                    "warning: asc: kept tag '{}', which names something else",
                    full
                );
                kept += 1;
                continue;
            }
        }
        updates.push(RefUpdate {
            name,
            expect,
            new: Some(entry.target),
            expires_at: None,
        });
    }
    let moved = updates.len();
    if !updates.is_empty() {
        let txn = RefTxn {
            updates,
            actor: actor(),
            reason: String::new(),
            op_id: uuid::Uuid::now_v7(),
        };
        ctx.update_local_refs(
            txn,
            &format!("import: from {}", super::url_display::anonymize(&o.source)),
            StagingAfterMove::Keep,
        )
        .await?;
    }
    tips.sort();
    tips.dedup();
    let gen_report = rederive_gen(ws, &tips).await?;
    for line in gen_report.unreadable.iter().chain(&gen_report.mismatches) {
        eprintln!("warning: asc: {line}");
    }
    println!(
        "imported {branches} branches and {tags} tags from {}: {moved} refs updated, {kept} kept, {} changesets indexed",
        super::url_display::anonymize(&o.source),
        gen_report.inserted
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn needs_a_source() {
        assert!(parse(&os(&[])).is_err());
        assert!(parse(&os(&["--repo", "x"])).is_err());
        assert!(parse(&os(&["somewhere"])).is_err());
    }

    #[test]
    fn source_and_name() {
        let o = parse(&os(&["--git", "../x", "--incremental"])).unwrap();
        assert_eq!(o.source, "../x");
        assert_eq!(o.name, "import");
        let o = parse(&os(&["--git=/p", "--repo=asc"])).unwrap();
        assert_eq!((o.source.as_str(), o.name.as_str()), ("/p", "asc"));
    }

    #[test]
    fn bad_names_are_refused() {
        for bad in ["", "a/b", "..", ".x", "-x", "a b", "x.lock", "a..b"] {
            assert!(parse(&os(&["--git", "p", "--repo", bad])).is_err(), "{bad}");
        }
    }
}
