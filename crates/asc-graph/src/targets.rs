//! Per-project target commands (SPEC §24.8.2 A4, PLAN P57).
//!
//! A `[[project]]` of `.asc/projects.toml` may carry `argv = { test = ["bun", "run", "test"] }`.
//! asc stores and serves the table (`AffectedProject.targets`) and never runs it: an IDE "Run
//! target" task runs it with the project root as the working directory. The table is untrusted
//! text, so every part of it is bounded here, once, when the file is parsed.

use std::collections::BTreeMap;

use crate::config::{ProjectEntry, ProjectTarget};
use crate::model::{Ecosystem, Project, ProjectGraph};

/// Most targets one project may define.
pub const MAX_TARGETS: usize = 64;
/// Most words in one command.
pub const MAX_ARGS: usize = 256;
/// Longest target name, in bytes.
pub const MAX_NAME: usize = 128;
/// Longest single word, in bytes.
pub const MAX_WORD: usize = 4096;
/// Most bytes in one command, all words together.
pub const MAX_COMMAND: usize = 32 * 1024;

/// A target name or command that the table cannot hold.
pub type Message = String;

/// Checks one `argv` entry: a plain target name and a non-empty command whose words hold no NUL.
pub fn check_argv(target: &str, argv: &[String]) -> Result<(), Message> {
    if target.is_empty()
        || target.len() > MAX_NAME
        || target.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(format!(
            "invalid argv target name {target:?} (1 to {MAX_NAME} bytes, no whitespace or control characters)"
        ));
    }
    let first = argv
        .first()
        .ok_or_else(|| format!("argv for {target:?} is empty"))?;
    if first.is_empty() {
        return Err(format!(
            "argv for {target:?} starts with an empty program name"
        ));
    }
    if argv.len() > MAX_ARGS {
        return Err(format!(
            "argv for {target:?} has more than {MAX_ARGS} words"
        ));
    }
    let mut total = 0usize;
    for w in argv {
        if w.len() > MAX_WORD {
            return Err(format!(
                "a word of argv for {target:?} is over {MAX_WORD} bytes"
            ));
        }
        if w.contains('\0') {
            return Err(format!("a word of argv for {target:?} holds a NUL byte"));
        }
        total += w.len();
    }
    if total > MAX_COMMAND {
        return Err(format!("argv for {target:?} is over {MAX_COMMAND} bytes"));
    }
    Ok(())
}

/// Checks the whole table of one project.
pub fn check_table(table: &BTreeMap<String, Vec<String>>) -> Result<(), Message> {
    if table.len() > MAX_TARGETS {
        return Err(format!("argv defines more than {MAX_TARGETS} targets"));
    }
    Ok(())
}

fn applies(e: &ProjectEntry, p: &Project) -> bool {
    match &e.target {
        ProjectTarget::Select(sel) => sel.matches(p),
        ProjectTarget::Declared { root, .. } => {
            p.key.eco == Ecosystem::Declared && p.key.manifest == *root
        }
    }
}

/// The commands defined for `project`: the `argv` tables of every `[[project]]` entry that
/// applies to it, a later entry replacing an earlier one's command for the same target.
pub fn commands_for(graph: &ProjectGraph, project: &Project) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for e in graph.config.projects.iter().filter(|e| applies(e, project)) {
        for (t, argv) in &e.argv {
            out.insert(t.clone(), argv.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_plain_command_passes() {
        assert!(check_argv("test", &words(&["bun", "run", "test"])).is_ok());
        assert!(check_argv("build:prod", &words(&["make", ""])).is_ok());
    }

    #[test]
    fn bad_names_and_commands_are_refused() {
        assert!(check_argv("", &words(&["x"])).is_err());
        assert!(check_argv("a b", &words(&["x"])).is_err());
        assert!(check_argv("a\nb", &words(&["x"])).is_err());
        assert!(check_argv(&"n".repeat(MAX_NAME + 1), &words(&["x"])).is_err());
        assert!(check_argv("t", &[]).is_err());
        assert!(check_argv("t", &words(&[""])).is_err());
        assert!(check_argv("t", &words(&["a\0b"])).is_err());
        assert!(check_argv("t", &vec!["x".to_owned(); MAX_ARGS + 1]).is_err());
        assert!(check_argv("t", &["x".repeat(MAX_WORD + 1)]).is_err());
        let wide = vec!["x".repeat(MAX_WORD); MAX_COMMAND / MAX_WORD + 1];
        assert!(check_argv("t", &wide).is_err());
    }

    #[test]
    fn the_table_is_bounded() {
        let t: BTreeMap<_, _> = (0..=MAX_TARGETS)
            .map(|i| (format!("t{i}"), words(&["x"])))
            .collect();
        assert!(check_table(&t).is_err());
        assert!(check_table(&BTreeMap::new()).is_ok());
    }

    #[test]
    fn parse_reads_argv_and_refuses_bad_tables() {
        let c = config::parse(
            "[[project]]\nselect = \"a\"\nargv = { test = [\"bun\", \"run\", \"test\"], lint = [\"x\"] }\n",
        )
        .unwrap();
        assert_eq!(c.projects[0].argv["test"], ["bun", "run", "test"]);
        assert_eq!(c.projects[0].argv.len(), 2);
        let e = config::parse("[[project]]\nselect = \"a\"\nargv = { test = [] }\n").unwrap_err();
        assert_eq!(e.line, 3);
        assert!(config::parse("[[project]]\nselect = \"a\"\nargv = { test = [1] }\n").is_err());
        assert!(config::parse("[[project]]\nselect = \"a\"\nargv = [\"x\"]\n").is_err());
    }
}
