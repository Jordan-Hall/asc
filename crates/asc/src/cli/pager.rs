//! The pager, with git's rules: `GIT_PAGER`, then `core.pager`, then `PAGER`, then `less`;
//! only when stdout is a terminal; `cat` and the empty string mean no pager; `LESS=FRX` and
//! `LV=-c` are set when unset.
#![allow(dead_code)] // Paged commands arrive with their units.

use std::ffi::OsString;
use std::io::Write;
use std::process::{Child, Stdio};

use crate::env::{GitEnv, PagerChoice};

use super::common::{Ctx, Result};
use super::config::ConfigSnapshot;

/// What a command wants of the pager.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PagerWant {
    /// The command pages when the config allows (`log`, `diff`, `show`, …).
    Auto,
    /// The command never pages unless `-p` is given.
    Never,
}

/// The pager command line for `command`, or `None` for no pager.
pub fn resolve(
    env: &GitEnv,
    config: &ConfigSnapshot,
    command: &str,
    want: PagerWant,
    stdout_is_tty: bool,
) -> Option<String> {
    let forced = env.pager_choice == PagerChoice::Always;
    if env.pager_choice == PagerChoice::Never || (!stdout_is_tty && !forced) {
        return None;
    }
    // `pager.<command>`: false disables, true enables, anything else is a pager command.
    let per_command = config.get(&format!("pager.{command}"));
    let custom = match per_command.as_deref() {
        Some(v) => match crate::env::parse_git_bool(v) {
            Some(false) if !forced => return None,
            Some(_) => None,
            None => Some(v.to_owned()),
        },
        None => None,
    };
    if want == PagerWant::Never && !forced && custom.is_none() && per_command.is_none() {
        return None;
    }
    let text = |v: &Option<OsString>| v.as_ref().and_then(|s| s.to_str()).map(str::to_owned);
    let line = custom
        .or_else(|| text(&env.pager))
        .or_else(|| config.get("core.pager"))
        .or_else(|| text(&env.pager_fallback))
        .unwrap_or_else(|| "less".to_owned());
    let trimmed = line.trim();
    (!trimmed.is_empty() && trimmed != "cat").then(|| trimmed.to_owned())
}

/// A running pager; dropping it closes stdin and waits for the user to quit.
pub struct Pager {
    child: Child,
}

impl Pager {
    /// Starts `line` through the shell with stdin piped.
    pub fn spawn(ctx: &Ctx, line: &str) -> Result<Self> {
        let mut cmd = ctx.child(ctx.shell())?;
        cmd.arg("-c").arg(format!("{line} \"$@\"")).arg(line);
        if std::env::var_os("LESS").is_none() {
            cmd.env("LESS", "FRX");
        }
        if std::env::var_os("LV").is_none() {
            cmd.env("LV", "-c");
        }
        let child = cmd.stdin(Stdio::piped()).spawn()?;
        Ok(Self { child })
    }

    /// The pipe the command writes to.
    pub fn stdin(&mut self) -> &mut std::process::ChildStdin {
        self.child.stdin.as_mut().expect("stdin was piped")
    }

    /// Closes the pipe and waits for the pager to exit.
    pub fn finish(mut self) -> std::io::Result<()> {
        if let Some(mut stdin) = self.child.stdin.take() {
            // A pager the user quit early closes the pipe; that is not an error.
            let _ = stdin.flush();
        }
        self.child.wait().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> GitEnv {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        GitEnv::from_vars(&|k| map.get(k).cloned())
    }

    fn config(text: &str) -> ConfigSnapshot {
        ConfigSnapshot::from_text(text)
    }

    #[test]
    fn precedence_is_git_pager_core_pager_pager_less() {
        let none = config("");
        assert_eq!(
            resolve(&env(&[]), &none, "log", PagerWant::Auto, true).as_deref(),
            Some("less")
        );
        let e = env(&[("PAGER", "more")]);
        assert_eq!(
            resolve(&e, &none, "log", PagerWant::Auto, true).as_deref(),
            Some("more")
        );
        let c = config("[core]\n\tpager = most\n");
        assert_eq!(
            resolve(&e, &c, "log", PagerWant::Auto, true).as_deref(),
            Some("most")
        );
        let e = env(&[("PAGER", "more"), ("GIT_PAGER", "bat")]);
        assert_eq!(
            resolve(&e, &c, "log", PagerWant::Auto, true).as_deref(),
            Some("bat")
        );
    }

    #[test]
    fn no_pager_off_a_terminal_or_for_cat_and_empty() {
        let none = config("");
        assert!(resolve(&env(&[]), &none, "log", PagerWant::Auto, false).is_none());
        assert!(
            resolve(
                &env(&[("GIT_PAGER", "cat")]),
                &none,
                "log",
                PagerWant::Auto,
                true
            )
            .is_none()
        );
        assert!(
            resolve(
                &env(&[("GIT_PAGER", "")]),
                &none,
                "log",
                PagerWant::Auto,
                true
            )
            .is_none()
        );
    }

    #[test]
    fn flags_and_per_command_config() {
        let none = config("");
        let mut e = env(&[]);
        e.pager_choice = PagerChoice::Never;
        assert!(resolve(&e, &none, "log", PagerWant::Auto, true).is_none());
        e.pager_choice = PagerChoice::Always;
        assert!(resolve(&e, &none, "status", PagerWant::Never, false).is_some());
        let off = config("[pager]\n\tlog = false\n");
        assert!(resolve(&env(&[]), &off, "log", PagerWant::Auto, true).is_none());
        let custom = config("[pager]\n\tdiff = delta\n");
        assert_eq!(
            resolve(&env(&[]), &custom, "diff", PagerWant::Never, true).as_deref(),
            Some("delta")
        );
        assert!(resolve(&env(&[]), &none, "status", PagerWant::Never, true).is_none());
    }
}
