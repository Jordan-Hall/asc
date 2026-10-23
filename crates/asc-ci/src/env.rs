//! Writers for the Actions command files `$GITHUB_ENV` and `$GITHUB_PATH` (SPEC §12.3 steps 5
//! and 6).
//!
//! The values come from job inputs and a server, so they are untrusted: a value with a newline
//! could otherwise forge a second variable. A single-line value is written as `NAME=value`; a
//! multi-line one uses the heredoc form with a random delimiter that the value does not contain.

use std::io::Write;
use std::path::Path;

/// Longest value written to a command file.
pub const MAX_VALUE: usize = 64 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnvError {
    #[error("invalid environment variable name '{0}'")]
    Name(String),
    #[error("the value of {0} contains a NUL byte")]
    Nul(String),
    #[error("the value of {0} is longer than {MAX_VALUE} bytes")]
    TooLong(String),
    #[error("a PATH entry may not contain a line break or NUL")]
    PathEntry,
    #[error("{0}")]
    Io(String),
}

fn name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 256
}

/// The text appended to `$GITHUB_ENV` for `vars`.
pub fn render_env(vars: &[(String, String)]) -> Result<String, EnvError> {
    let mut out = String::new();
    for (name, value) in vars {
        if !name_ok(name) {
            return Err(EnvError::Name(name.clone()));
        }
        if value.contains('\0') {
            return Err(EnvError::Nul(name.clone()));
        }
        if value.len() > MAX_VALUE {
            return Err(EnvError::TooLong(name.clone()));
        }
        if value.contains(['\n', '\r']) {
            let delim = loop {
                let d = format!("ghadelimiter_{}", uuid::Uuid::new_v4().simple());
                if !value.contains(&d) {
                    break d;
                }
            };
            out.push_str(&format!("{name}<<{delim}\n{value}\n{delim}\n"));
        } else {
            out.push_str(&format!("{name}={value}\n"));
        }
    }
    Ok(out)
}

/// The text appended to `$GITHUB_PATH` for `dirs`.
pub fn render_path(dirs: &[String]) -> Result<String, EnvError> {
    let mut out = String::new();
    for d in dirs {
        if d.is_empty() || d.contains(['\n', '\r', '\0']) {
            return Err(EnvError::PathEntry);
        }
        out.push_str(d);
        out.push('\n');
    }
    Ok(out)
}

fn append(file: &Path, text: &str) -> Result<(), EnvError> {
    let io = |e: std::io::Error| EnvError::Io(format!("{}: {e}", file.display()));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .map_err(io)?;
    f.write_all(text.as_bytes()).map_err(io)
}

/// Appends `vars` to the `$GITHUB_ENV` file.
pub fn append_env(file: &Path, vars: &[(String, String)]) -> Result<(), EnvError> {
    append(file, &render_env(vars)?)
}

/// Appends `dirs` to the `$GITHUB_PATH` file (each is prepended to `PATH` for later steps).
pub fn append_path(file: &Path, dirs: &[String]) -> Result<(), EnvError> {
    append(file, &render_path(dirs)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(k: &str, x: &str) -> (String, String) {
        (k.to_owned(), x.to_owned())
    }

    #[test]
    fn single_line_values_are_plain() {
        assert_eq!(
            render_env(&[v("A", "1"), v("B_2", "x=y")]).unwrap(),
            "A=1\nB_2=x=y\n"
        );
    }

    #[test]
    fn multiline_values_cannot_forge_a_variable() {
        let text = render_env(&[v("A", "x\nEVIL=1")]).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("A<<ghadelimiter_"));
        let delim = lines[0].split("<<").nth(1).unwrap();
        assert_eq!(lines[1], "x");
        assert_eq!(lines[2], "EVIL=1");
        assert_eq!(lines[3], delim);
    }

    #[test]
    fn bad_names_and_values_are_refused() {
        assert_eq!(
            render_env(&[v("A B", "1")]),
            Err(EnvError::Name("A B".into()))
        );
        assert_eq!(
            render_env(&[v("1A", "1")]),
            Err(EnvError::Name("1A".into()))
        );
        assert_eq!(
            render_env(&[v("", "1")]),
            Err(EnvError::Name(String::new()))
        );
        assert_eq!(
            render_env(&[v("A", "a\0b")]),
            Err(EnvError::Nul("A".into()))
        );
        assert!(render_path(&["a\nb".to_owned()]).is_err());
        assert!(render_path(&[String::new()]).is_err());
    }

    #[test]
    fn files_are_appended_not_replaced() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("env");
        std::fs::write(&f, "OLD=1\n").unwrap();
        append_env(&f, &[v("NEW", "2")]).unwrap();
        append_path(&t.path().join("path"), &["/shim".to_owned()]).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "OLD=1\nNEW=2\n");
        assert_eq!(
            std::fs::read_to_string(t.path().join("path")).unwrap(),
            "/shim\n"
        );
    }
}
