//! `asc cache setup <tool>` (docs/designs/build-cache.md §5): writes the tool's configuration so
//! its builds use the build cache, prints what changed, and takes it out again with `--undo`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use toml_edit::{DocumentMut, Item, Table, value};

use crate::cli::common::{CliError, Ctx, Result};

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// `build.rustc-wrapper` in cargo's configuration.
    Cargo,
}

#[derive(clap::Args, Debug)]
pub struct SetupArgs {
    tool: Tool,
    /// The user's configuration (the default outside a work tree).
    #[arg(long, conflicts_with = "repo")]
    user: bool,
    /// The work tree's configuration, to commit (the default in a work tree).
    #[arg(long)]
    repo: bool,
    /// Cargo: turn incremental compilation off for the dev profile, so workspace crates are
    /// cached too.
    #[arg(long = "no-incremental")]
    no_incremental: bool,
    /// Takes out what setup wrote.
    #[arg(long)]
    undo: bool,
}

/// What setup did to one setting.
#[derive(Debug, PartialEq, Eq)]
enum Change {
    Set(&'static str, String),
    Removed(&'static str),
}

pub fn run(ctx: &Ctx, a: &SetupArgs, out: &mut impl Write) -> Result<ExitCode> {
    match a.tool {
        Tool::Cargo => cargo(ctx, a, out),
    }
}

fn cargo_home() -> Result<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".cargo")))
        .ok_or_else(|| CliError::Fatal("cannot find cargo's home (set CARGO_HOME)".into()))
}

/// The configuration file and the wrapper to name in it: in a work tree `asc` (found on `PATH`
/// by each person who builds), in the user's file this executable's path.
fn cargo_target(ctx: &Ctx, a: &SetupArgs) -> Result<(PathBuf, String)> {
    let repo = match (a.user, a.repo) {
        (true, _) => false,
        (_, true) => true,
        _ => ctx.ws_opt().is_some(),
    };
    if repo {
        let root = ctx.ws()?.projection.toplevel();
        return Ok((root.join(".cargo").join("config.toml"), "asc".into()));
    }
    let home = cargo_home()?;
    // Cargo reads `config` when `config.toml` is absent.
    let file = if !home.join("config.toml").exists() && home.join("config").exists() {
        home.join("config")
    } else {
        home.join("config.toml")
    };
    let exe = std::env::current_exe()?;
    Ok((file, exe.to_string_lossy().replace('\\', "/")))
}

/// Whether `wrapper` names asc.
fn is_asc(wrapper: &str) -> bool {
    Path::new(wrapper)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("asc"))
}

fn table<'a>(doc: &'a mut Table, name: &str) -> Result<&'a mut Table> {
    let item = doc.entry(name).or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    item.as_table_mut()
        .ok_or_else(|| CliError::Fatal(format!("'{name}' in cargo's configuration is not a table")))
}

/// The edits to `doc`, applied.
fn edit_cargo(
    doc: &mut DocumentMut,
    wrapper: &str,
    no_incremental: bool,
    undo: bool,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    let root = doc.as_table_mut();
    let current = root
        .get("build")
        .and_then(|b| b.get("rustc-wrapper"))
        .and_then(Item::as_str)
        .map(str::to_owned);
    if undo {
        if current.as_deref().is_some_and(is_asc)
            && let Some(build) = root.get_mut("build").and_then(Item::as_table_mut)
        {
            build.remove("rustc-wrapper");
            changes.push(Change::Removed("build.rustc-wrapper"));
            if build.is_empty() {
                root.remove("build");
            }
        }
        let dev = root
            .get_mut("profile")
            .and_then(Item::as_table_mut)
            .and_then(|p| p.get_mut("dev"))
            .and_then(Item::as_table_mut);
        if let Some(dev) = dev
            && dev.get("incremental").and_then(Item::as_bool) == Some(false)
        {
            dev.remove("incremental");
            changes.push(Change::Removed("profile.dev.incremental"));
            let empty = dev.is_empty();
            if let Some(profile) = root.get_mut("profile").and_then(Item::as_table_mut) {
                if empty {
                    profile.remove("dev");
                }
                if profile.is_empty() {
                    root.remove("profile");
                }
            }
        }
        return Ok(changes);
    }
    match current {
        Some(w) if w == wrapper => {}
        Some(w) if !is_asc(&w) => {
            return Err(CliError::Fatal(format!(
                "build.rustc-wrapper is already '{w}'; remove it first"
            )));
        }
        _ => {
            table(root, "build")?.insert("rustc-wrapper", value(wrapper));
            changes.push(Change::Set("build.rustc-wrapper", format!("\"{wrapper}\"")));
        }
    }
    if no_incremental {
        let profile = table(root, "profile")?;
        let dev = table(profile, "dev")?;
        if dev.get("incremental").and_then(Item::as_bool) != Some(false) {
            dev.insert("incremental", value(false));
            changes.push(Change::Set("profile.dev.incremental", "false".into()));
        }
    }
    Ok(changes)
}

fn cargo(ctx: &Ctx, a: &SetupArgs, out: &mut impl Write) -> Result<ExitCode> {
    let (file, wrapper) = cargo_target(ctx, a)?;
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| CliError::Fatal(format!("{}: {e}", file.display())))?;
    let changes = edit_cargo(&mut doc, &wrapper, a.no_incremental, a.undo)?;
    let shown = file.display().to_string().replace('\\', "/");
    if changes.is_empty() {
        writeln!(out, "{shown}: nothing to change")?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&file, doc.to_string())?;
    for c in &changes {
        match c {
            Change::Set(k, v) => writeln!(out, "{shown}: set {k} = {v}")?,
            Change::Removed(k) => writeln!(out, "{shown}: removed {k}")?,
        }
    }
    if changes.contains(&Change::Set("profile.dev.incremental", "false".into())) {
        writeln!(
            out,
            "incremental compilation is off for dev builds: an edit recompiles the whole crate, \
             and in return every crate is cached across work trees and machines"
        )?;
    } else if !a.undo && !a.no_incremental {
        writeln!(
            out,
            "note: cargo compiles workspace crates incrementally in dev builds, and those are not \
             cached; --no-incremental turns that off"
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(text: &str, no_inc: bool, undo: bool) -> (String, Vec<Change>) {
        let mut doc: DocumentMut = text.parse().unwrap();
        let c = edit_cargo(&mut doc, "asc", no_inc, undo).unwrap();
        (doc.to_string(), c)
    }

    #[test]
    fn setup_sets_and_undo_takes_out_only_its_own_settings() {
        let start = "# mine\n[build]\njobs = 4\n\n[alias]\nb = \"build\"\n";
        let (set, c) = edit(start, true, false);
        assert_eq!(c.len(), 2);
        assert!(set.contains("rustc-wrapper = \"asc\""), "{set}");
        assert!(set.contains("incremental = false"), "{set}");
        assert!(set.contains("# mine") && set.contains("jobs = 4"), "{set}");
        assert!(edit(&set, true, false).1.is_empty(), "idempotent");
        let (undone, c) = edit(&set, false, true);
        assert_eq!(c.len(), 2);
        assert_eq!(undone, start);
        assert!(edit(&undone, false, true).1.is_empty());
    }

    #[test]
    fn another_wrapper_is_not_replaced() {
        let mut doc: DocumentMut = "[build]\nrustc-wrapper = \"sccache\"\n".parse().unwrap();
        assert!(edit_cargo(&mut doc, "asc", false, false).is_err());
        assert!(edit_cargo(&mut doc, "asc", false, true).unwrap().is_empty());
        // The user's file may name this executable by path.
        let mut doc: DocumentMut = "[build]\nrustc-wrapper = \"/opt/bin/asc\"\n"
            .parse()
            .unwrap();
        assert_eq!(
            edit_cargo(&mut doc, "/usr/bin/asc", false, false)
                .unwrap()
                .len(),
            1
        );
    }
}
