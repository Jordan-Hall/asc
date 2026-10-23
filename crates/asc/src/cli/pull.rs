//! `pull [<remote> [<branch>]]` (SPEC §11.2): fetch, then merge or rebase the remote-tracking
//! branch per `--rebase`/`--no-rebase`/`--ff-only` and `pull.rebase`/`pull.ff`.
//!
//! The integration step is the `merge` or `rebase` command itself, so `post-merge`, the
//! `RewriteSink` calls of a rebase (SPEC §20.3) and every ref update behave exactly as when the
//! user runs those commands.

use std::ffi::OsString;
use std::process::ExitCode;

use asc_engine::history::HistoryApi;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::fetch::{Opts as FetchOpts, current_branch, default_remote, fatal, fetch_one, tip_of};
use crate::native::NativeSpec;
use crate::remote::url::display_from;

#[derive(clap::Parser, Debug)]
#[command(name = "pull", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "pull",
    flags: &[
        "-q", "--quiet", "-v", "--verbose", "-r", "--rebase", "--no-rebase", "--ff", "--ff-only", "--no-ff", "-p",
        "--prune", "-t", "--tags", "-n", "--no-tags", "--no-verify", "--verify", "--no-edit", "--edit", "-e",
        "--progress", "--no-progress", "--no-recurse-submodules", "--autostash", "--no-autostash",
    ],
    placeholders: &[],
}];

const USAGE: &str = "usage: git pull [<options>] [<repository> [<refspec>...]]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rebase {
    No,
    Yes,
    Merges,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ff {
    Allow,
    Only,
    Never,
}

#[derive(Debug, Default)]
struct Opts {
    quiet: bool,
    verbose: bool,
    rebase: Option<Rebase>,
    ff: Option<Ff>,
    prune: bool,
    tags: Option<bool>,
    no_verify: bool,
    words: Vec<String>,
}

fn rebase_value(v: &str) -> Result<Rebase> {
    match v {
        "true" | "yes" | "on" | "1" => Ok(Rebase::Yes),
        "false" | "no" | "off" | "0" | "" => Ok(Rebase::No),
        "merges" | "m" => Ok(Rebase::Merges),
        "interactive" | "i" => Err(CliError::Fatal("asc: interactive pull --rebase is not supported".into())),
        other => Err(CliError::Fatal(format!("invalid value for 'pull.rebase': '{other}'"))),
    }
}

fn parse(argv: &[OsString]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut ended = false;
    for a in argv.iter().map(|a| a.to_string_lossy().into_owned()) {
        if ended || !a.starts_with('-') || a == "-" {
            o.words.push(a);
            continue;
        }
        match a.as_str() {
            "--" => ended = true,
            "-q" | "--quiet" => o.quiet = true,
            "-v" | "--verbose" => o.verbose = true,
            "-r" | "--rebase" => o.rebase = Some(Rebase::Yes),
            "--no-rebase" => o.rebase = Some(Rebase::No),
            "--ff" => o.ff = Some(Ff::Allow),
            "--ff-only" => o.ff = Some(Ff::Only),
            "--no-ff" => o.ff = Some(Ff::Never),
            "-p" | "--prune" => o.prune = true,
            "-t" | "--tags" => o.tags = Some(true),
            "-n" | "--no-tags" => o.tags = Some(false),
            "--no-verify" => o.no_verify = true,
            "--verify" | "--no-edit" | "--edit" | "-e" | "--progress" | "--no-progress" | "--no-recurse-submodules"
            | "--no-autostash" => {}
            "--autostash" => return Err(CliError::Fatal("asc: pull --autostash is not supported yet".into())),
            w if w.starts_with("--rebase=") => o.rebase = Some(rebase_value(&w["--rebase=".len()..])?),
            _ => return Err(CliError::Usage(USAGE.to_owned())),
        }
    }
    Ok(o)
}

fn no_tracking(ctx: &Ctx, branch: Option<&str>, remote: &str) -> CliError {
    let prog = ctx.prog.name();
    let text = match branch {
        None => format!(
            "You are not currently on a branch.\nPlease specify which branch you want to merge with.\nSee git-pull(1) for details.\n\n    {prog} pull <remote> <branch>\n"
        ),
        Some(b) => format!(
            "There is no tracking information for the current branch.\nPlease specify which branch you want to merge with.\nSee git-pull(1) for details.\n\n    {prog} pull <remote> <branch>\n\nIf you wish to set tracking information for this branch you can do so with:\n\n    {prog} branch --set-upstream-to={remote}/<branch> {b}\n"
        ),
    };
    CliError::Raw(text, 1)
}

/// The remote-tracking ref a pull integrates, as `(full ref, branch name on the remote)`.
fn target_ref(ctx: &Ctx, ws: &WsCtx, remote: &str, words: &[String]) -> Result<(String, String)> {
    if let Some(b) = words.first() {
        let name = b.strip_prefix("refs/heads/").unwrap_or(b).to_owned();
        return Ok((format!("refs/remotes/{remote}/{name}"), name));
    }
    let branch = current_branch(ws)?;
    let Some(b) = branch.as_deref() else {
        return Err(no_tracking(ctx, None, remote));
    };
    let configured = ctx.config.get(&format!("branch.{b}.remote"));
    let merge = ctx.config.get(&format!("branch.{b}.merge"));
    match (configured.as_deref(), merge) {
        (Some(r), Some(m)) if r == remote => {
            let name = m.strip_prefix("refs/heads/").unwrap_or(&m).to_owned();
            Ok((format!("refs/remotes/{remote}/{name}"), name))
        }
        _ => Err(no_tracking(ctx, Some(b), remote)),
    }
}

fn divergent_hint(prog: &str) -> CliError {
    CliError::Raw(
        format!(
            "hint: You have divergent branches and need to specify how to reconcile them.\nhint: You can do so by running one of the following commands sometime before\nhint: your next pull:\nhint:\nhint:   {prog} config pull.rebase false  # merge\nhint:   {prog} config pull.rebase true   # rebase\nhint:   {prog} config pull.ff only       # fast-forward only\nhint:\nhint: You can replace \"{prog} config\" with \"{prog} config --global\" to set a default\nhint: preference for all repositories. You can also pass --rebase, --no-rebase,\nhint: or --ff-only on the command line to override the configured default per\nhint: invocation.\nfatal: Need to specify how to reconcile divergent branches."
        ),
        128,
    )
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let o = parse(&a.rest)?;
    let (remote_arg, words) = match o.words.split_first() {
        Some((first, rest)) => (first.clone(), rest.to_vec()),
        None => (default_remote(ctx, ws)?, Vec::new()),
    };
    let fetch_opts = FetchOpts {
        quiet: o.quiet,
        verbose: o.verbose,
        prune: o.prune.then_some(true),
        tags: o.tags,
        ..FetchOpts::default()
    };
    let fetched_words: Vec<String> = words.iter().map(|w| w.strip_prefix("refs/heads/").unwrap_or(w).to_owned()).collect();
    let fetched = fetch_one(ctx, ws, &remote_arg, &fetch_opts, &fetched_words).await?;
    let remote_name = fetched.opened.name.clone();
    let (tracking, branch_name) = target_ref(ctx, ws, &remote_name, &words)?;
    let Some(theirs) = tip_of(ws, &tracking).await? else {
        return Err(CliError::Raw(
            format!("Your configuration specifies to merge with the ref 'refs/heads/{branch_name}'\nfrom the remote, but no such ref was fetched."),
            1,
        ));
    };
    let head = ws.head_commit().await?;
    let branch = current_branch(ws)?;

    let configured_rebase = match branch.as_deref().and_then(|b| ctx.config.get(&format!("branch.{b}.rebase"))) {
        Some(v) => Some(rebase_value(&v)?),
        None => match ctx.config.get("pull.rebase") {
            Some(v) => Some(rebase_value(&v)?),
            None => None,
        },
    };
    let configured_ff = match ctx.config.get("pull.ff").as_deref() {
        Some("only") => Some(Ff::Only),
        Some("false") => Some(Ff::Never),
        Some("true") => Some(Ff::Allow),
        _ => None,
    };
    let rebase = o.rebase.or(configured_rebase);
    let ff = o.ff.or(configured_ff);

    // Divergence without a stated way to reconcile it is an error, as in git.
    if let Some(h) = head
        && rebase.is_none()
        && ff.is_none()
        && h != theirs
    {
        let contained = HistoryApi::is_ancestor(&ws.engine, theirs, h).await.map_err(fatal)?;
        let can_ff = HistoryApi::is_ancestor(&ws.engine, h, theirs).await.map_err(fatal)?;
        if !contained && !can_ff {
            return Err(divergent_hint(ctx.prog.name()));
        }
    }

    let mut argv: Vec<OsString> = Vec::new();
    if matches!(rebase, Some(Rebase::Yes | Rebase::Merges)) {
        argv.push("rebase".into());
        if rebase == Some(Rebase::Merges) {
            argv.push("--rebase-merges".into());
        }
        if o.quiet {
            argv.push("-q".into());
        }
        if o.no_verify {
            argv.push("--no-verify".into());
        }
        argv.push(tracking.clone().into());
    } else {
        argv.push("merge".into());
        match ff {
            Some(Ff::Only) => argv.push("--ff-only".into()),
            Some(Ff::Never) => argv.push("--no-ff".into()),
            _ => {}
        }
        if o.quiet {
            argv.push("-q".into());
        }
        if o.verbose {
            argv.push("-v".into());
        }
        if o.no_verify {
            argv.push("--no-verify".into());
        }
        argv.push("-m".into());
        argv.push(format!("Merge branch '{branch_name}' of {}", display_from(&fetched.opened.url)).into());
        argv.push(tracking.clone().into());
    }
    super::run_command(ctx, argv).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebase_values() {
        assert_eq!(rebase_value("true").unwrap(), Rebase::Yes);
        assert_eq!(rebase_value("false").unwrap(), Rebase::No);
        assert_eq!(rebase_value("merges").unwrap(), Rebase::Merges);
        assert!(rebase_value("interactive").is_err());
        assert!(rebase_value("sideways").is_err());
    }

    #[test]
    fn options_parse() {
        let argv: Vec<OsString> = ["--rebase", "-q", "origin", "master"].iter().map(Into::into).collect();
        let o = parse(&argv).unwrap();
        assert_eq!(o.rebase, Some(Rebase::Yes));
        assert!(o.quiet);
        assert_eq!(o.words, ["origin", "master"]);
        assert!(parse(&[OsString::from("--bogus")]).is_err());
    }
}
