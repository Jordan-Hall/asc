//! `check-ref-format` (SPEC §11.2): git's `check_refname_format`, silent with exit 1 when the
//! name is invalid.

use std::ffi::OsString;
use std::process::ExitCode;

use crate::cli::common::{CliError, Result};

const USAGE: &str = super::usage::CHECK_REF_FORMAT;

/// Which names `check_refname_format` admits.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rules {
    pub allow_onelevel: bool,
    /// One `*` is allowed (a refspec side).
    pub refspec_pattern: bool,
}

/// The length of the component at the start of `name`; 0 when it is empty, `None` when invalid.
fn component(name: &[u8], pattern: &mut bool) -> Option<usize> {
    let mut last = 0u8;
    let mut end = name.len();
    for (i, &ch) in name.iter().enumerate() {
        match ch {
            b'/' => {
                end = i;
                break;
            }
            b'.' if last == b'.' => return None,
            b'{' if last == b'@' => return None,
            0..=0x20 | 0x7f | b'~' | b'^' | b':' | b'?' | b'[' | b'\\' => return None,
            b'*' => {
                if !*pattern {
                    return None;
                }
                *pattern = false;
            }
            _ => {}
        }
        last = ch;
    }
    if end == 0 {
        return Some(0);
    }
    if name[0] == b'.' || name[..end].ends_with(b".lock") {
        return None;
    }
    Some(end)
}

/// Whether `name` is a valid ref name under `rules`.
pub fn valid(name: &str, rules: Rules) -> bool {
    if name == "@" {
        return false;
    }
    let mut rest = name.as_bytes();
    let (mut count, mut pattern) = (0, rules.refspec_pattern);
    let last_len = loop {
        let Some(len) = component(rest, &mut pattern) else { return false };
        if len == 0 {
            return false;
        }
        count += 1;
        if len == rest.len() {
            break len;
        }
        rest = &rest[len + 1..];
    };
    if rest[last_len - 1] == b'.' {
        return false;
    }
    rules.allow_onelevel || count >= 2
}

/// `collapse_slashes`: no leading slash, no doubled slashes.
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.trim_start_matches('/').chars() {
        if c != '/' || !out.ends_with('/') {
            out.push(c);
        }
    }
    out
}

pub fn run(args: &[OsString]) -> Result<ExitCode> {
    let usage = || CliError::Usage(USAGE.to_owned());
    let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    if args.len() == 2 && args[0] == "--branch" {
        let name = &args[1];
        let ok = !name.starts_with('-') && valid(&format!("refs/heads/{name}"), Rules::default()) && name != "HEAD";
        if !ok {
            return Err(CliError::Fatal(format!("'{name}' is not a valid branch name")));
        }
        println!("{name}");
        return Ok(ExitCode::SUCCESS);
    }
    let (mut rules, mut print) = (Rules::default(), false);
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        match args[i].as_str() {
            "--normalize" | "--print" => print = true,
            "--allow-onelevel" => rules.allow_onelevel = true,
            "--no-allow-onelevel" => rules.allow_onelevel = false,
            "--refspec-pattern" => rules.refspec_pattern = true,
            _ => return Err(usage()),
        }
        i += 1;
    }
    if i + 1 != args.len() {
        return Err(usage());
    }
    let name = if print { normalize(&args[i]) } else { args[i].clone() };
    if !valid(&name, rules) {
        return Ok(ExitCode::from(1));
    }
    if print {
        println!("{name}");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(name: &str) -> bool {
        valid(name, Rules::default())
    }

    #[test]
    fn git_ref_name_rules() {
        assert!(ok("refs/heads/master"));
        assert!(!ok("master"));
        assert!(valid("master", Rules { allow_onelevel: true, ..Rules::default() }));
        assert!(!ok("refs/heads/.hidden"));
        assert!(!ok("refs/heads/a..b"));
        assert!(!ok("refs/heads/a.lock"));
        assert!(!ok("refs/heads/a b"));
        assert!(!ok("refs/heads/a~1"));
        assert!(!ok("refs/heads/a@{1}"));
        assert!(!ok("refs/heads/"));
        assert!(!ok("/refs/heads/a"));
        assert!(!ok("refs//heads/a"));
        assert!(!ok("refs/heads/a."));
        assert!(!ok("@"));
        assert!(!ok("refs/heads/a*"));
        assert!(valid("refs/heads/*", Rules { refspec_pattern: true, ..Rules::default() }));
        assert!(!valid("refs/*/*", Rules { refspec_pattern: true, ..Rules::default() }));
        assert_eq!(normalize("//a///b/"), "a/b/");
    }
}
