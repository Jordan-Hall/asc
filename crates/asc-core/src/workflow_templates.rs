//! Built-in workflow templates: starting files for `.asc/workflows/<name>.toml` that
//! `asc workflow new` writes. Each one is an ordinary workflow file, checked by the same strict
//! parser as any other (`workflow::Workflow::parse`), so a template that does not parse fails the
//! tests here, never a user's run.

/// One template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Template {
    /// The template's name, also the default workflow name.
    pub name: &'static str,
    /// One line for a listing.
    pub summary: &'static str,
    /// The file as written.
    pub text: &'static str,
}

/// Every template, by name.
pub const TEMPLATES: &[Template] = &[Template {
    name: "tsrx-check",
    summary: "Prettier, ESLint and tsrx-tsc over the TSRX files of a change",
    text: include_str!("../templates/workflows/tsrx-check.toml"),
}];

/// The template called `name`.
pub fn template(name: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{StepKind, TriggerKind, Workflow, valid_name};

    #[test]
    fn every_template_parses_and_is_named_validly() {
        let mut names: Vec<&str> = TEMPLATES.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TEMPLATES.len(), "template names are unique");
        for t in TEMPLATES {
            assert!(valid_name(t.name), "{}", t.name);
            assert!(!t.summary.is_empty());
            let w = Workflow::parse(t.name, t.text.as_bytes())
                .unwrap_or_else(|e| panic!("{}: {e}", t.name));
            assert!(!w.steps.is_empty(), "{}", t.name);
        }
    }

    #[test]
    fn tsrx_check_runs_the_three_tools_on_tsrx_changes() {
        let t = template("tsrx-check").unwrap();
        let w = Workflow::parse(t.name, t.text.as_bytes()).unwrap();
        assert_eq!(w.order(), ["format", "lint", "types"]);
        for s in &w.steps {
            assert_eq!(s.kind, StepKind::Command);
            assert!(s.needs.is_empty(), "the checks run side by side");
            assert!(
                s.secrets.is_empty(),
                "no secrets, so no trust gate on the change"
            );
            let line = s.command.last().unwrap();
            assert!(line.starts_with("npm ci --ignore-scripts "), "{line}");
        }
        let line = |id: &str| w.step(id).unwrap().command.join(" ");
        assert!(line("format").contains("prettier --check '**/*.tsrx'"));
        assert!(line("lint").contains("eslint --max-warnings 0"));
        assert!(line("types").contains("tsrx-tsc --noEmit"));
        let kinds: Vec<TriggerKind> = w.triggers.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&TriggerKind::ChangeOpened));
        assert!(kinds.contains(&TriggerKind::ChangeUpdated));
        assert!(kinds.contains(&TriggerKind::Manual));
        for t in w.triggers.iter().filter(|t| t.kind != TriggerKind::Manual) {
            let hit = |path: &str| t.paths.iter().any(|g| g.matches(path.as_bytes()));
            assert!(hit("src/App.tsrx") && hit("App.tsrx") && hit("package-lock.json"));
            assert!(!hit("src/App.tsx") && !hit("README.md"));
        }
    }

    #[test]
    fn an_unknown_template_is_none() {
        assert!(template("nope").is_none());
        assert!(template("").is_none());
    }
}
