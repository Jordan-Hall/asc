//! `asc stack info|parent|children|trunk` (SPEC §20.7): relationship and configuration reads.
//!
//! `stack` is one verb word shared by many units, so it registers once here and dispatches
//! on its first argument through the [`SUBS`] slice; a later unit adds `stack rename`,
//! `stack get`, ... with [`sub!`] in its own file and never touches this one. A word with no
//! entry answers "not supported yet". The `NATIVE` flag list covers every `stack` sub-verb of
//! SPEC §20.7, so a flag of a sub-verb that is not served yet reaches that answer, not a
//! flag error.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use asc_core::change::ChangePrefix;
use asc_core::id::ChangesetId;
use asc_engine::change::{ChangeRow, ChangeRows};
use clap::Parser;
use linkme::distributed_slice;
use serde::Serialize;

use super::sl::{Local, LStacks, change_out, clean, fatal, stack_err};
use crate::cli::BoxFut;
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::config;
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::msg::Msg;
use crate::native::NativeSpec;
use crate::stack::json::{ChangeOut, Trunk, VERSION, line};

/// One `asc stack <name>` sub-verb.
pub struct Sub {
    pub name: &'static str,
    /// Gets `[name, args…]`.
    pub run: for<'a> fn(&'a Ctx, Vec<OsString>) -> BoxFut<'a>,
}

#[allow(unsafe_code)] // linkme declares the slice with a link section.
#[distributed_slice]
pub static SUBS: [Sub];

/// Registers a `stack` sub-verb: `sub!(SLOT, "word", Args, run)`, where `Args` is a clap
/// parser named like the word and `run` is `async fn(&Ctx, Args) -> Result<ExitCode>`.
/// `sub!(SLOT, "word", Args, run, json: Done::IfLogged)` also gives it a `--json` form that
/// prints an `OpResult` (see `json_op`).
macro_rules! sub {
    ($slot:ident, $name:literal, $args:ty, $run:ident) => {
        $crate::stack::cli::info::sub!(@go $slot, $name, $args, $run,
            ::std::option::Option::<$crate::stack::cli::json_op::Done>::None);
    };
    ($slot:ident, $name:literal, $args:ty, $run:ident, json: $done:expr) => {
        $crate::stack::cli::info::sub!(@go $slot, $name, $args, $run, ::std::option::Option::Some($done));
    };
    (@go $slot:ident, $name:literal, $args:ty, $run:ident, $json:expr) => {
        #[allow(unsafe_code)] // linkme places the entry in a link section.
        #[::linkme::distributed_slice($crate::stack::cli::info::SUBS)]
        static $slot: $crate::stack::cli::info::Sub = $crate::stack::cli::info::Sub {
            name: $name,
            run: {
                fn go<'a>(
                    ctx: &'a $crate::cli::common::Ctx,
                    argv: ::std::vec::Vec<::std::ffi::OsString>,
                ) -> $crate::cli::BoxFut<'a> {
                    ::std::boxed::Box::pin(async move {
                        if let ::std::option::Option::Some(done) = $json
                            && let ::std::option::Option::Some(plain) = $crate::stack::cli::json_op::strip(&argv)
                        {
                            return $crate::stack::cli::json_op::run(ctx, &["stack"], plain, done).await;
                        }
                        let args = <$args as ::clap::Parser>::try_parse_from(argv).map_err(|e| {
                            $crate::cli::common::CliError::Usage(e.render().to_string().trim_end().to_owned())
                        })?;
                        $run(ctx, args).await
                    })
                }
                go
            },
        };
    };
}
#[allow(unused_imports)]
pub(crate) use sub;

#[derive(clap::Parser, Debug)]
#[command(name = "stack", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "stack",
    flags: &[
        "--json", "--add", "--remove", "--upstack", "--downstack", "-c", "-d", "-u", "--unfrozen",
        "--worktree", "--now", "-n", "--dry-run", "--revset", "--delete", "--close", "-s", "--stack", "--draft",
        "--publish", "--reviewer", "--update-only", "--auto-merge", "--force", "--no-fetch", "--no-restack",
        "--all-trunks", "--downstack", "--only", "-m", "--message",
    ],
    placeholders: &[],
}];

super::verb!(STACK, "stack");

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let Some(word) = a.rest.first().and_then(|w| w.to_str()) else {
        return Err(CliError::Usage(
            "usage: asc stack <rename|delete|pop|get|freeze|unfreeze|trunk|info|parent|children|merge|exec|submit|sync|restack|share> [<args>]"
                .into(),
        ));
    };
    match SUBS.iter().find(|s| s.name == word) {
        Some(s) => (s.run)(ctx, a.rest.clone()).await,
        None => Err(CliError::Msg(Msg::AscNotYet { cmd: format!("stack {}", clean(word)) })),
    }
}

/// Resolves `word` to a draft change: a `k`-`z` change prefix of a draft first, else a
/// revision. `None` is the current HEAD. Exit 1 when the result is not a draft change.
pub(crate) async fn resolve_change(
    ctx: &Ctx,
    l: &Local<'_>,
    st: &LStacks<'_, '_>,
    word: Option<&str>,
) -> Result<(ChangesetId, ChangeRow)> {
    let cs = match word {
        None => st.current_head().ok_or_else(|| CliError::Raw("asc: HEAD is not on a change".into(), 1))?,
        Some(w) => match by_prefix(l, st, w).await? {
            Some(cs) => cs,
            None => crate::cli::restore::resolve_commit(ctx, w)
                .await?
                .ok_or_else(|| CliError::Fatal(format!("bad revision '{}'", clean(w))))?,
        },
    };
    match st.draft_row(cs).await.map_err(stack_err)? {
        Some(row) => Ok((cs, row)),
        None => Err(CliError::Raw("asc: not a draft change".into(), 1)),
    }
}

async fn by_prefix(l: &Local<'_>, st: &LStacks<'_, '_>, word: &str) -> Result<Option<ChangesetId>> {
    let Ok(prefix) = ChangePrefix::parse(word) else { return Ok(None) };
    let mut hits: Vec<(ChangesetId, ChangeRow)> = Vec::new();
    for cs in st.draft_set().await.map_err(stack_err)? {
        if let Some(row) = l.rows.get(cs).await.map_err(fatal)?
            && prefix.matches(&row.change)
        {
            hits.push((cs, row));
        }
    }
    let Some(first) = hits.first().map(|h| h.1.change) else { return Ok(None) };
    if hits.iter().any(|h| h.1.change != first) {
        return Err(CliError::Fatal(format!("ambiguous change prefix '{}'", clean(word))));
    }
    for (cs, _) in &hits {
        if !st.is_obsolete(*cs).await.map_err(stack_err)? {
            return Ok(Some(*cs));
        }
    }
    Ok(hits.first().map(|h| h.0))
}

/// The draft children of `cs`: draft changes whose first parent is `cs`.
async fn children_of(l: &Local<'_>, st: &LStacks<'_, '_>, cs: ChangesetId) -> Result<Vec<ChangesetId>> {
    let mut out = Vec::new();
    for d in st.draft_set().await.map_err(stack_err)? {
        if l.graph.node(d).await.map_err(fatal)?.parents.first() == Some(&cs) {
            out.push(d);
        }
    }
    Ok(out)
}

#[derive(clap::Parser, Debug)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct RelArgs {
    /// A change prefix or revision; default is HEAD.
    pub change: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct Info {
    version: u32,
    change: ChangeOut,
    /// Changes directly above this one.
    children: Vec<String>,
    /// 1-based place from the stack bottom.
    position: usize,
    trunk: String,
}

#[derive(Serialize)]
struct Related {
    version: u32,
    changes: Vec<ChangeOut>,
}

fn emit(ctx: &Ctx, text: &str) -> Result<()> {
    let mut out = Output::open(ctx, "stack", PagerWant::Never)?;
    out.write_all(text.as_bytes())?;
    out.finish()?;
    Ok(())
}

fn json_text<T: Serialize>(v: &T) -> Result<String> {
    line(v).map_err(fatal)
}

async fn info(ctx: &Ctx, a: RelArgs) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let st = l.stacks().await?;
    let (cs, row) = resolve_change(ctx, &l, &st, a.change.as_deref()).await?;
    let me = change_out(&l, &st, cs, &row).await?;
    let mut kids = Vec::new();
    for k in children_of(&l, &st, cs).await? {
        if let Some(r) = st.draft_row(k).await.map_err(stack_err)? {
            kids.push(r.change.to_string());
        }
    }
    let stack = st.stack_of(cs).await.map_err(stack_err)?;
    let trunk = l
        .env
        .trunk_info()
        .into_iter()
        .find(|t| Some(t.tip) == stack.trunk)
        .map(|t| t.name)
        .unwrap_or_default();
    let info = Info { version: VERSION, position: stack.changes.len(), trunk, children: kids, change: me };
    let text = if a.json {
        json_text(&info)?
    } else {
        let c = &info.change;
        let mut t = format!("change:   {}\ncommit:   {}\nsubject:  {}\n", c.change, c.commit, clean(&c.subject));
        t.push_str(&format!("parent:   {}\n", c.parent_change.as_deref().unwrap_or("none")));
        t.push_str(&format!("children: {}\n", info.children.len()));
        t.push_str(&format!("stack:    {} from the bottom, on {}\n", info.position, clean(&info.trunk)));
        t.push_str(&format!("state:    {:?}\n", c.state).to_lowercase());
        if c.needs_restack {
            t.push_str("restack:  needed\n");
        }
        t
    };
    emit(ctx, &text)?;
    Ok(ExitCode::SUCCESS)
}

async fn related(ctx: &Ctx, a: RelArgs, up: bool) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let st = l.stacks().await?;
    let (cs, _) = resolve_change(ctx, &l, &st, a.change.as_deref()).await?;
    let ids = if up {
        children_of(&l, &st, cs).await?
    } else {
        let parent = l.graph.node(cs).await.map_err(fatal)?.parents.first().copied();
        match parent {
            Some(p) if st.draft_row(p).await.map_err(stack_err)?.is_some() => vec![p],
            _ => Vec::new(),
        }
    };
    let mut changes = Vec::new();
    for id in ids {
        if let Some(r) = st.draft_row(id).await.map_err(stack_err)? {
            changes.push(change_out(&l, &st, id, &r).await?);
        }
    }
    let text = if a.json {
        json_text(&Related { version: VERSION, changes: changes.clone() })?
    } else {
        changes.iter().map(|c| format!("{}\n", c.change)).collect()
    };
    emit(ctx, &text)?;
    // A bottom change has no parent: nothing printed, exit 1. No children is a plain empty list.
    Ok(if changes.is_empty() && !up { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

async fn parent(ctx: &Ctx, a: RelArgs) -> Result<ExitCode> {
    related(ctx, a, false).await
}

async fn children(ctx: &Ctx, a: RelArgs) -> Result<ExitCode> {
    related(ctx, a, true).await
}

sub!(STACK_INFO, "info", RelArgs, info);
sub!(STACK_PARENT, "parent", RelArgs, parent);
sub!(STACK_CHILDREN, "children", RelArgs, children);

#[derive(clap::Parser, Debug)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct TrunkArgs {
    #[arg(long, conflicts_with = "remove")]
    pub add: Option<String>,
    #[arg(long)]
    pub remove: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct Trunks {
    version: u32,
    trunks: Vec<Trunk>,
}

/// A branch name safe to store as a trunk: git's ref-name rules for the characters that
/// matter, and nothing a shell, a regex or a config file could misread.
fn valid_trunk(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && !name.chars().any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\\"".contains(c))
}

async fn config_run(ctx: &Ctx, args: &[&str]) -> Result<()> {
    let mut argv = vec!["config"];
    argv.extend_from_slice(args);
    let parsed = config::Args::try_parse_from(argv)
        .map_err(|e| CliError::Usage(e.render().to_string().trim_end().to_owned()))?;
    config::run(ctx, parsed).await.map(|_| ())
}

async fn trunk(ctx: &Ctx, a: TrunkArgs) -> Result<ExitCode> {
    const KEY: &str = "asc.stack.trunk";
    if let Some(b) = &a.add {
        if !valid_trunk(b) {
            return Err(CliError::Fatal(format!("asc: '{}' is not a usable trunk name", clean(b))));
        }
        let have = ctx.config.get_all(KEY);
        if !have.iter().any(|h| h == b) {
            // The implicit default stops applying once a trunk is configured: keep it.
            if have.is_empty() && b != "master" {
                config_run(ctx, &["--local", "--add", KEY, "master"]).await?;
            }
            config_run(ctx, &["--local", "--add", KEY, b]).await?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(b) = &a.remove {
        if !ctx.config.get_all(KEY).iter().any(|h| h == b) {
            return Err(CliError::Fatal(format!("asc: '{}' is not a configured trunk", clean(b))));
        }
        let pattern = format!("^{}$", regex::escape(b));
        config_run(ctx, &["--local", "--unset", KEY, &pattern]).await?;
        return Ok(ExitCode::SUCCESS);
    }
    let l = Local::open(ctx).await?;
    let mut names = Vec::new();
    let mut rows = Vec::new();
    for t in l.env.trunk_info() {
        names.push(t.name.clone());
        rows.push(Trunk { name: t.name, tip: l.git_id(t.tip).await?, remote: t.remote });
    }
    let text = if a.json {
        json_text(&Trunks { version: VERSION, trunks: rows })?
    } else {
        // Configured trunks that do not exist yet still print, so `--remove` can name them.
        l.env.trunk_names().iter().map(|n| format!("{}\n", clean(n))).collect()
    };
    emit(ctx, &text)?;
    Ok(ExitCode::SUCCESS)
}

sub!(STACK_TRUNK, "trunk", TrunkArgs, trunk);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trunk_names_are_checked() {
        for ok in ["master", "release/1.x", "a-b_c"] {
            assert!(valid_trunk(ok), "{ok}");
        }
        for bad in ["", "-x", "/x", "a..b", "a b", "a\u{1b}", "a~", "a:b", "x/", ".x", "a@{b", "a\"b"] {
            assert!(!valid_trunk(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_stack_sub_verbs_are_registered() {
        for w in ["info", "parent", "children", "trunk"] {
            assert!(SUBS.iter().any(|s| s.name == w), "{w}");
        }
    }
}
