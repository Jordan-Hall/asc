//! The native table (SPEC §11.5): which invocations asc serves. Every command file declares a
//! `NATIVE` const of [`NativeSpec`]; [`route`] collects them. An invocation outside the table
//! fails before doing anything, with git's exit code 129 (SPEC §11.2, §11.5). There is no
//! fall-through to real git in a workspace and no `asc.delegate`.

use std::ffi::OsString;

use crate::cli;
use crate::dispatch;
use crate::msg::Msg;

/// What one command file serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSpec {
    /// The git command name, e.g. `"status"`.
    pub cmd: &'static str,
    /// Every flag served, spelled as typed: `"-s"`, `"--porcelain"`. `"-<n>"` stands for `-3`
    /// style counts. `"-*"` stands for every option: a command that echoes the options it does
    /// not know, as git's `rev-parse` does (docs/git-quirks.md Q-2), serves them all.
    pub flags: &'static [&'static str],
    /// The format placeholders and ref-filter atoms served, e.g. `"%H"`, `"%(refname)"`. Empty
    /// means the command takes no format.
    pub placeholders: &'static [&'static str],
}

/// The outcome of routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Native,
    Unsupported(Msg),
}

/// Whether `argv` (`[command, args…]`) is in the native table.
pub fn route(argv: &[OsString]) -> Route {
    route_in(argv, &cli::native_specs().collect::<Vec<_>>())
}

/// [`route`] over an explicit table (the tests use small ones).
pub fn route_in(argv: &[OsString], table: &[&NativeSpec]) -> Route {
    let Some(cmd) = argv.first().and_then(|a| a.to_str()) else {
        return Route::Unsupported(Msg::AscNotYet { cmd: String::new() });
    };
    let specs: Vec<&&NativeSpec> = table.iter().filter(|s| s.cmd == cmd).collect();
    if specs.is_empty() {
        return Route::Unsupported(not_served(cmd));
    }
    let mut ended = false;
    for arg in &argv[1..] {
        let Some(text) = arg.to_str() else { continue };
        if ended {
            continue;
        }
        if text == "--" {
            ended = true;
            continue;
        }
        let Some(rest) = text.strip_prefix('-') else {
            continue;
        };
        if rest.is_empty() {
            continue; // `-` alone is a path or a revision (`switch -`).
        }
        // Every letter of a short cluster must be served: flags are never ignored.
        let unserved = flags_of(text)
            .into_iter()
            .find(|f| !specs.iter().any(|s| serves(s, f)));
        if let Some(flag) = unserved {
            return Route::Unsupported(Msg::AscOptionUnsupported { flag });
        }
        if let Some(format) = format_of(text) {
            for token in tokens_of(format) {
                if !specs.iter().any(|s| serves_token(s, &token)) {
                    return Route::Unsupported(Msg::AscOptionUnsupported { flag: token });
                }
            }
        }
    }
    Route::Native
}

/// "Not supported yet" for a command whose unit has not landed; "not supported in an asc
/// workspace" for a git command that no unit will serve.
pub fn not_served(cmd: &str) -> Msg {
    let planned = cli::is_registered(cmd) || dispatch::ASC_ONLY_VERBS.contains(&cmd);
    if planned && !dispatch::is_unavailable(cmd) {
        Msg::AscNotYet {
            cmd: cmd.to_owned(),
        }
    } else {
        Msg::AscNotInWorkspace {
            cmd: cmd.to_owned(),
        }
    }
}

fn serves(spec: &NativeSpec, flag: &str) -> bool {
    spec.flags.contains(&flag)
        || spec.flags.contains(&"-*")
        || (flag.len() > 1
            && flag[1..].chars().all(|c| c.is_ascii_digit())
            && spec.flags.contains(&"-<n>"))
}

/// The flags in one argument: `--name` for `--name=value`, each letter for a `-abc` cluster of
/// short flags (stopping at a digit, which starts a value such as `-U3`).
fn flags_of(arg: &str) -> Vec<String> {
    if let Some(long) = arg.strip_prefix("--") {
        let name = long.split_once('=').map_or(long, |(n, _)| n);
        return vec![format!("--{name}")];
    }
    let body = &arg[1..];
    if body.chars().all(|c| c.is_ascii_digit()) {
        return vec![arg.to_owned()];
    }
    body.chars()
        .take_while(|c| !c.is_ascii_digit())
        .map(|c| format!("-{c}"))
        .collect()
}

/// The format string of `--format=…`, `--pretty=format:…`, `--pretty=tformat:…`.
fn format_of(arg: &str) -> Option<&str> {
    if let Some(f) = arg.strip_prefix("--format=") {
        return Some(f);
    }
    let pretty = arg.strip_prefix("--pretty=")?;
    pretty
        .strip_prefix("tformat:")
        .or_else(|| pretty.strip_prefix("format:"))
}

/// The `%(atom)` and `%X…` tokens of a format string. `%%` and `%n` are literals and are not
/// tokens; a `%X…` token runs to the next `%` so that a placeholder is matched by prefix.
fn tokens_of(format: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = format;
    while let Some(at) = rest.find('%') {
        rest = &rest[at + 1..];
        if let Some(inner) = rest.strip_prefix('(') {
            let end = inner.find(')').unwrap_or(inner.len());
            out.push(format!("%({})", &inner[..end]));
            rest = &inner[(end + 1).min(inner.len())..];
        } else if let Some(after) = rest.strip_prefix('%').or_else(|| rest.strip_prefix('n')) {
            rest = after;
        } else {
            let end = rest.find('%').unwrap_or(rest.len());
            out.push(format!("%{}", &rest[..end]));
            rest = &rest[end..];
        }
    }
    out
}

/// Whether a token is served: `%%`/`%n` always; otherwise some entry of the table is a prefix
/// of it (`%(upstream:short)` is served by `%(upstream`; `%aNx` by `%aN`). A command with an
/// empty placeholder list serves any format.
fn serves_token(spec: &NativeSpec, token: &str) -> bool {
    if spec.placeholders.is_empty() {
        return true;
    }
    if let Some(atom) = token.strip_prefix("%(") {
        let name = atom.split([':', ')']).next().unwrap_or(atom);
        return spec.placeholders.iter().any(|p| {
            p.strip_prefix("%(")
                .is_some_and(|a| a.trim_end_matches(')') == name)
        });
    }
    spec.placeholders
        .iter()
        .any(|p| !p.starts_with("%(") && token.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: NativeSpec = NativeSpec {
        cmd: "log",
        flags: &["--oneline", "-n", "--format", "--pretty", "-<n>", "-p"],
        placeholders: &["%H", "%aN", "%(refname)"],
    };

    fn argv(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    fn route_log(items: &[&str]) -> Route {
        route_in(&argv(items), &[&LOG])
    }

    #[test]
    fn served_invocations_are_native() {
        assert_eq!(
            route_log(&["log", "--oneline", "-n", "3", "HEAD", "--", "-x"]),
            Route::Native
        );
        assert_eq!(route_log(&["log", "-3"]), Route::Native);
        assert_eq!(route_log(&["log", "--format=%H %aN%n%%"]), Route::Native);
    }

    #[test]
    fn the_wildcard_serves_every_option() {
        const ECHO: NativeSpec = NativeSpec {
            cmd: "rev-parse",
            flags: &["--git-dir", "-*"],
            placeholders: &[],
        };
        let r = |items: &[&str]| route_in(&argv(items), &[&ECHO]);
        assert_eq!(
            r(&["rev-parse", "--no-such-flag", "-x", "HEAD"]),
            Route::Native
        );
        assert_eq!(
            route_log(&["log", "--bogus"]),
            Route::Unsupported(Msg::AscOptionUnsupported {
                flag: "--bogus".into()
            })
        );
    }

    #[test]
    fn unknown_flag_is_named() {
        assert_eq!(
            route_log(&["log", "--bogus"]),
            Route::Unsupported(Msg::AscOptionUnsupported {
                flag: "--bogus".into()
            })
        );
        assert_eq!(
            route_log(&["log", "--bogus=1"]),
            Route::Unsupported(Msg::AscOptionUnsupported {
                flag: "--bogus".into()
            })
        );
        assert_eq!(
            route_log(&["log", "-pz"]),
            Route::Unsupported(Msg::AscOptionUnsupported { flag: "-z".into() })
        );
    }

    #[test]
    fn unknown_placeholder_is_named() {
        assert_eq!(
            route_log(&["log", "--format=%H%x00"]),
            Route::Unsupported(Msg::AscOptionUnsupported {
                flag: "%x00".into()
            })
        );
        assert_eq!(
            route_log(&["log", "--format=%(refname:short)"]),
            Route::Native
        );
        assert_eq!(
            route_log(&["log", "--pretty=format:%(objectname)"]),
            Route::Unsupported(Msg::AscOptionUnsupported {
                flag: "%(objectname)".into()
            })
        );
    }

    #[test]
    fn command_outside_the_table_is_not_served() {
        assert!(matches!(
            route_in(&argv(&["status"]), &[&LOG]),
            Route::Unsupported(Msg::AscNotYet { .. })
        ));
    }

    #[test]
    fn flags_after_double_dash_and_lone_dash_are_operands() {
        assert_eq!(route_log(&["log", "-", "--", "--weird"]), Route::Native);
    }
}
