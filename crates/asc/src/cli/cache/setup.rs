//! `asc cache setup <tool>` (docs/designs/build-cache.md §5): writes the tool's configuration so
//! its builds use the build cache, prints what changed, and takes it out again with `--undo`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use toml_edit::{DocumentMut, Item, Table, value};

use crate::cli::common::{CliError, Ctx, Result};

pub(crate) mod fronts;

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// `build.rustc-wrapper` in cargo's configuration.
    Cargo,
    /// `GOCACHEPROG` in the user's go env file.
    Go,
    /// turbo's remote cache: the front, through `asc cache env`.
    Turbo,
    /// Nx's self-hosted remote cache: the front, through `asc cache env`.
    Nx,
    /// `--remote_cache` in the user's `.bazelrc`.
    Bazel,
    /// An init script in the Gradle user home that names the front as the remote build cache.
    Gradle,
    /// The token as a server in the user's Maven settings; `asc cache env` gives `MAVEN_ARGS`.
    Maven,
    /// ccache's `remote_storage`.
    Ccache,
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
    /// Go: add `-trimpath` to `GOFLAGS`, so packages in different work trees share entries
    /// (with `--undo`: take it out).
    #[arg(long)]
    trimpath: bool,
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
        Tool::Go => go(a, out),
        _ => fronts::run(ctx, a, out),
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
    print(out, &shown, &changes)?;
    if changes.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&file, doc.to_string())?;
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

fn print(out: &mut impl Write, shown: &str, changes: &[Change]) -> Result<()> {
    if changes.is_empty() {
        writeln!(out, "{shown}: nothing to change")?;
    }
    for c in changes {
        match c {
            Change::Set(k, v) => writeln!(out, "{shown}: set {k} = {v}")?,
            Change::Removed(k) => writeln!(out, "{shown}: removed {k}")?,
        }
    }
    Ok(())
}

/// Whether `prog` (a `GOCACHEPROG` value) runs asc's go cache.
fn is_asc_go(prog: &str) -> bool {
    let first = prog
        .strip_prefix('"')
        .and_then(|p| p.split_once('"'))
        .map_or_else(|| prog.split(' ').next().unwrap_or_default(), |(p, _)| p);
    is_asc(first) && prog.ends_with(" cache go")
}

/// The edits to the go env file: each change with the variable's new value (`None`: unset).
fn edit_go(
    prog: Option<&str>,
    flags: Option<&str>,
    wanted: &str,
    trimpath: bool,
    undo: bool,
) -> Result<Vec<(Change, &'static str, Option<String>)>> {
    let mut edits = Vec::new();
    let flags: Vec<&str> = flags.unwrap_or_default().split_whitespace().collect();
    let has_trim = flags
        .iter()
        .any(|f| *f == "-trimpath" || *f == "-trimpath=true");
    if undo {
        if prog.is_some_and(is_asc_go) {
            edits.push((Change::Removed("GOCACHEPROG"), "GOCACHEPROG", None));
        }
        if trimpath && has_trim {
            let rest: Vec<&str> = flags
                .iter()
                .copied()
                .filter(|f| *f != "-trimpath" && *f != "-trimpath=true")
                .collect();
            let value = (!rest.is_empty()).then(|| rest.join(" "));
            edits.push((Change::Removed("-trimpath from GOFLAGS"), "GOFLAGS", value));
        }
        return Ok(edits);
    }
    match prog.filter(|p| !p.is_empty()) {
        Some(p) if p == wanted => {}
        Some(p) if !is_asc_go(p) => {
            return Err(CliError::Fatal(format!(
                "GOCACHEPROG is already '{p}'; remove it first"
            )));
        }
        _ => edits.push((
            Change::Set("GOCACHEPROG", wanted.to_owned()),
            "GOCACHEPROG",
            Some(wanted.to_owned()),
        )),
    }
    if trimpath && !has_trim {
        let mut all = flags.clone();
        all.push("-trimpath");
        let value = all.join(" ");
        edits.push((
            Change::Set("GOFLAGS", value.clone()),
            "GOFLAGS",
            Some(value),
        ));
    }
    Ok(edits)
}

fn go_cmd() -> std::process::Command {
    std::process::Command::new(super::run::program(&"go".into()))
}

/// `go env <args>`, its output trimmed.
fn go_env(args: &[&str]) -> Result<String> {
    let out = go_cmd()
        .arg("env")
        .args(args)
        .output()
        .map_err(|e| CliError::Fatal(format!("cannot run 'go': {e}")))?;
    if !out.status.success() {
        return Err(CliError::Fatal(format!(
            "go env {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The values the go env file itself holds (not the environment's).
fn go_file_values(file: &Path) -> Result<(Option<String>, Option<String>)> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let get = |name: &str| {
        text.lines()
            .filter_map(|l| l.split_once('='))
            .filter(|(k, _)| k.trim() == name)
            .map(|(_, v)| v.trim().to_owned())
            .next_back()
    };
    Ok((get("GOCACHEPROG"), get("GOFLAGS")))
}

fn go(a: &SetupArgs, out: &mut impl Write) -> Result<ExitCode> {
    if a.repo {
        return Err(CliError::Usage(
            "error: go reads GOCACHEPROG from the environment or the user's go env file, not \
             from the repository; leave out --repo"
                .into(),
        ));
    }
    let file = go_env(&["GOENV"])?;
    if file.is_empty() || file == "off" {
        return Err(CliError::Fatal(
            "the go env file is off (GOENV=off); set GOCACHEPROG in the environment instead".into(),
        ));
    }
    let file = PathBuf::from(file);
    let exe = std::env::current_exe()?
        .to_string_lossy()
        .replace('\\', "/");
    let exe = if exe.contains(' ') {
        format!("\"{exe}\"")
    } else {
        exe
    };
    let wanted = format!("{exe} cache go");
    let (prog, flags) = go_file_values(&file)?;
    let edits = edit_go(
        prog.as_deref(),
        flags.as_deref(),
        &wanted,
        a.trimpath,
        a.undo,
    )?;
    let mut set = Vec::new();
    let mut unset = Vec::new();
    for (_, name, value) in &edits {
        match value {
            Some(v) => set.push(format!("{name}={v}")),
            None => unset.push(*name),
        }
    }
    if !set.is_empty() {
        let mut args = vec!["-w"];
        args.extend(set.iter().map(String::as_str));
        go_env(&args)?;
    }
    if !unset.is_empty() {
        let mut args = vec!["-u"];
        args.extend(unset.iter().copied());
        go_env(&args)?;
    }
    let changes: Vec<Change> = edits.into_iter().map(|(c, _, _)| c).collect();
    print(
        out,
        &file.display().to_string().replace('\\', "/"),
        &changes,
    )?;
    if !a.undo && !a.trimpath && !flags.unwrap_or_default().contains("-trimpath") {
        writeln!(
            out,
            "note: go keys a package by its directory unless it builds with -trimpath, so work \
             trees share entries only with --trimpath"
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

    #[test]
    fn go_setup_sets_and_undoes_only_its_own_values() {
        let want = "/opt/bin/asc cache go";
        let e = edit_go(None, Some("-mod=mod"), want, true, false).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].2.as_deref(), Some(want));
        assert_eq!(e[1].2.as_deref(), Some("-mod=mod -trimpath"));
        assert!(
            edit_go(Some(want), Some("-mod=mod -trimpath"), want, true, false)
                .unwrap()
                .is_empty()
        );
        // Another path to asc is replaced; another program is not.
        assert_eq!(
            edit_go(
                Some("\"C:/x y/asc.exe\" cache go"),
                None,
                want,
                false,
                false
            )
            .unwrap()
            .len(),
            1
        );
        assert!(edit_go(Some("gobuildcache"), None, want, false, false).is_err());
        let u = edit_go(Some(want), Some("-mod=mod -trimpath"), want, true, true).unwrap();
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].2, None);
        assert_eq!(u[1].2.as_deref(), Some("-mod=mod"));
        // Without --trimpath, undo leaves GOFLAGS alone.
        assert_eq!(
            edit_go(Some(want), Some("-trimpath"), want, false, true)
                .unwrap()
                .len(),
            1
        );
        assert!(
            edit_go(Some("gobuildcache"), None, want, false, true)
                .unwrap()
                .is_empty()
        );
    }
}
