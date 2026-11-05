//! `asc workflow templates | new`: the built-in workflow templates (`asc_core::workflow_templates`)
//! and writing one to `.asc/workflows/<name>.toml` in the workspace.
//!
//! This is local: nothing reaches the server until the file lands on trunk. The file is created,
//! never followed: a symbolic link at `.asc`, `.asc/workflows` or the file itself is refused, and
//! an existing file is kept unless `--force` is given.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_core::workflow_templates::{TEMPLATES, Template, template};

use super::{bad, name_ok};
use crate::cli::codeintel::with_stdout;
use crate::cli::common::{CliError, Ctx, Result};

#[derive(clap::Args, Debug)]
pub struct TemplatesArgs {
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct NewArgs {
    /// The template (`asc workflow templates` lists them).
    template: String,
    /// The workflow's name; the template's name when absent.
    #[arg(long)]
    name: Option<String>,
    /// Replace an existing file of that name.
    #[arg(long)]
    force: bool,
    /// Print the file instead of writing it.
    #[arg(long)]
    stdout: bool,
}

pub fn list(a: TemplatesArgs) -> Result<ExitCode> {
    with_stdout(|out| {
        for t in TEMPLATES {
            if a.json {
                let v = serde_json::json!({ "name": t.name, "summary": t.summary });
                writeln!(out, "{v}")?;
            } else {
                writeln!(out, "{:<16} {}", t.name, t.summary)?;
            }
        }
        Ok(ExitCode::SUCCESS)
    })
}

pub fn new(ctx: &Ctx, a: NewArgs) -> Result<ExitCode> {
    let t = template(&a.template).ok_or_else(|| {
        let names: Vec<&str> = TEMPLATES.iter().map(|t| t.name).collect();
        bad(format!(
            "no template {:?}; there are: {}",
            a.template,
            names.join(", ")
        ))
    })?;
    let name = a.name.as_deref().unwrap_or(t.name);
    name_ok("the workflow name", name)?;
    if a.stdout {
        return with_stdout(|out| {
            out.write_all(t.text.as_bytes())?;
            Ok(ExitCode::SUCCESS)
        });
    }
    let root = match ctx.ws_opt() {
        Some(ws) => PathBuf::from(&ws.workspace.ws.path),
        None => ctx.cwd.clone(),
    };
    let path = write(&root, t, name, a.force)?;
    let mut err = std::io::stderr().lock();
    let _ = writeln!(
        err,
        "wrote {}; it runs once it lands on trunk",
        path.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn fatal(text: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc workflow: {text}"))
}

/// A directory under `root` that is a real directory, made when missing.
fn real_dir(dir: &Path) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_dir() => Ok(()),
        Ok(_) => Err(fatal(format!(
            "{} is not a directory (links are not followed)",
            dir.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(dir)
            .map_err(|e| fatal(format!("cannot create {}: {}", dir.display(), e.kind()))),
        Err(e) => Err(fatal(format!(
            "cannot read {}: {}",
            dir.display(),
            e.kind()
        ))),
    }
}

/// Writes `t` as `<root>/.asc/workflows/<name>.toml` and returns the path.
fn write(root: &Path, t: &Template, name: &str, force: bool) -> Result<PathBuf> {
    let asc = root.join(".asc");
    real_dir(&asc)?;
    let dir = asc.join("workflows");
    real_dir(&dir)?;
    let path = dir.join(format!("{name}.toml"));
    match fs::symlink_metadata(&path) {
        Ok(m) if !m.file_type().is_file() => {
            return Err(fatal(format!(
                "{} exists and is not a file",
                path.display()
            )));
        }
        Ok(_) if !force => {
            return Err(fatal(format!(
                "{} exists; --force replaces it",
                path.display()
            )));
        }
        _ => {}
    }
    // A sibling temporary file then a rename, so a reader never sees half a file.
    let tmp = dir.join(format!(".{name}.toml.tmp"));
    let res = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(t.text.as_bytes())?;
            f.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, &path));
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(fatal(format!(
            "cannot write {}: {}",
            path.display(),
            e.kind()
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_core::workflow::Workflow;
    use clap::Parser as _;

    fn tsrx() -> &'static Template {
        template("tsrx-check").unwrap()
    }

    #[test]
    fn new_writes_a_file_that_parses_and_keeps_an_existing_one() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), tsrx(), "tsrx-check", false).unwrap();
        assert_eq!(p, d.path().join(".asc/workflows/tsrx-check.toml"));
        let text = fs::read(&p).unwrap();
        assert_eq!(text, tsrx().text.as_bytes());
        Workflow::parse("tsrx-check", &text).unwrap();

        fs::write(&p, "mine").unwrap();
        assert!(write(d.path(), tsrx(), "tsrx-check", false).is_err());
        assert_eq!(fs::read(&p).unwrap(), b"mine");
        write(d.path(), tsrx(), "tsrx-check", true).unwrap();
        assert_eq!(fs::read(&p).unwrap(), tsrx().text.as_bytes());

        write(d.path(), tsrx(), "lint", false).unwrap();
        assert!(d.path().join(".asc/workflows/lint.toml").is_file());
        let left: Vec<_> = fs::read_dir(d.path().join(".asc/workflows"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 2, "no temporary file is left: {left:?}");
    }

    #[test]
    fn a_file_where_a_directory_belongs_is_refused() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(".asc"), "").unwrap();
        assert!(write(d.path(), tsrx(), "x", false).is_err());
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".asc/workflows/x.toml")).unwrap();
        assert!(write(d.path(), tsrx(), "x", true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), d.path().join(".asc")).unwrap();
        assert!(write(d.path(), tsrx(), "x", true).is_err());
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_commands_parse() {
        let p = |args: &[&str]| {
            let mut argv = vec!["workflow"];
            argv.extend_from_slice(args);
            super::super::Args::try_parse_from(argv)
        };
        let a = p(&["new", "tsrx-check", "--name", "lint", "--force"]).unwrap();
        let super::super::Sub::New(a) = a.cmd else {
            panic!("new")
        };
        assert_eq!(
            (a.template.as_str(), a.name.as_deref(), a.force),
            ("tsrx-check", Some("lint"), true)
        );
        assert!(p(&["templates", "--json"]).is_ok());
        assert!(p(&["new"]).is_err());
    }
}
