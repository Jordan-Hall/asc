//! Lookups over the verb slice. Only verbs `precedence::routable` accepts are visible, so a
//! registration can never shadow a git command or take a name `dispatch` does not route with
//! D5 precedence.

use super::VERBS;
use crate::cli::Command;
use crate::stack::precedence::routable;

/// The registered verb called `name`.
pub fn find(name: &str) -> Option<&'static Command> {
    registered().find(|c| c.name == name)
}

/// Every reachable registration.
pub fn registered() -> impl Iterator<Item = &'static Command> {
    VERBS.iter().filter(|c| routable(c.name))
}

/// Problems in the registry: a name registered twice, or one that cannot be reached.
pub fn problems() -> Vec<String> {
    let mut out = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for c in VERBS.iter() {
        if !routable(c.name) {
            out.push(format!("{}: not a routable asc-only verb", c.name));
        } else if seen.contains(&c.name) {
            out.push(format!("{}: registered twice", c.name));
        }
        seen.push(c.name);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_is_consistent() {
        assert_eq!(problems(), Vec::<String>::new());
        for c in registered() {
            assert!(find(c.name).is_some());
        }
        assert!(find("status").is_none());
        assert!(find("nosuch").is_none());
    }
}

/// A verb registered only in the test binary, to prove the macro, the slice and the lookups.
#[cfg(test)]
mod probe {
    use std::process::ExitCode;

    use crate::cli::common::{Ctx, Result};
    use crate::native::NativeSpec;

    #[derive(clap::Parser, Debug)]
    #[command(name = "share", disable_help_flag = true)]
    pub struct Args {
        #[arg(long)]
        pub json: bool,
    }

    pub const NATIVE: &[NativeSpec] = &[NativeSpec { cmd: "share", flags: &["--json"], placeholders: &[] }];

    pub async fn run(_ctx: &Ctx, _args: Args) -> Result<ExitCode> {
        Ok(ExitCode::SUCCESS)
    }

    super::super::verb!(PROBE_SHARE, "share");

    #[test]
    fn a_registered_verb_is_found_and_listed_once() {
        let c = crate::stack::cli::find("share").expect("registered");
        assert_eq!(c.native[0].cmd, "share");
        assert_eq!(crate::stack::cli::registered().filter(|c| c.name == "share").count(), 1);
        assert!(crate::cli::find("share").is_some());
        assert!(crate::cli::native_specs().any(|s| s.cmd == "share"));
        assert!(crate::stack::cli::problems().is_empty());
    }
}
