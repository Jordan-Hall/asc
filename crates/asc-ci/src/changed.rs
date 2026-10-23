//! `asc ci changed` (SPEC §12.2): the plan job's glob switches over one `Diff(NAMES,
//! merge_base)`, plus `semantic=<overall class>` when the server offers `Classify`. The rules
//! come from `.asc/ci.toml` at the merge base (`ReadFile`, no checkout) or a local file. A change
//! to the rules file itself turns every switch on, so a PR cannot narrow its own CI.
//!
//! The RPCs sit behind [`Backend`]; the CLI implements it over `asc api` and `asc affected`.

use std::io::Read;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::rules::{MAX_RULES_BYTES, Rules, RulesError, rules_changed};

/// A failed call to the server (or to the `asc` child that makes it).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct BackendError(pub String);

/// The server calls the plan job makes. Every revision is a text the server resolves.
#[async_trait]
pub trait Backend: Send + Sync {
    /// The merge base of `base` and `head`, as a revision text.
    async fn merge_base(&self, base: &str, head: &str) -> Result<String, BackendError>;
    /// `ReadFile` of `path` at `rev`; `None` when the file does not exist there.
    async fn read_file(&self, rev: &str, path: &str) -> Result<Option<Vec<u8>>, BackendError>;
    /// `Diff(NAMES, merge_base = true)`: every changed path; an error if the answer is cut.
    async fn changed_paths(&self, base: &str, head: &str) -> Result<Vec<String>, BackendError>;
    /// The overall class of `Classify`; `None` when the server does not offer it.
    async fn semantic(&self, base: &str, head: &str) -> Result<Option<String>, BackendError>;
    /// `asc affected --format gha`: the output lines of SPEC §12A.9.
    async fn affected_gha(&self, base: &str, head: &str) -> Result<String, BackendError>;
    /// Whether any project affected between `base` and `head` matches `select`.
    async fn affected_matches(
        &self,
        base: &str,
        head: &str,
        select: &[String],
    ) -> Result<bool, BackendError>;
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Rules(#[from] RulesError),
    #[error("cannot read the rules file: {0}")]
    RulesFile(String),
}

/// Where the rules come from.
#[derive(Clone, Debug, Default)]
pub struct Input {
    pub base: String,
    pub head: String,
    /// `--rules`: a repository path, read at the merge base.
    pub rules_path: Option<String>,
    /// `--rules-file`: a local file, read instead of the repository path.
    pub rules_file: Option<PathBuf>,
}

/// `key=value` lines for `$GITHUB_OUTPUT`, and warnings for stderr.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub lines: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn render(&self) -> String {
        let mut s = String::new();
        for (k, v) in &self.lines {
            s.push_str(k);
            s.push('=');
            s.push_str(v);
            s.push('\n');
        }
        s
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.lines
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Replaces the last `key` line, or appends one.
    pub fn set(&mut self, key: &str, value: &str) {
        match self.lines.iter_mut().rev().find(|(k, _)| k == key) {
            Some(l) => l.1 = value.to_owned(),
            None => self.lines.push((key.to_owned(), value.to_owned())),
        }
    }

    pub(crate) fn push_switches(&mut self, switches: Vec<(String, bool)>) {
        for (k, v) in switches {
            self.set(&k, if v { "true" } else { "false" });
        }
    }
}

/// Reads a local rules file, bounded.
pub fn read_local_rules(path: &std::path::Path) -> Result<Vec<u8>, PlanError> {
    let f = std::fs::File::open(path).map_err(|e| PlanError::RulesFile(e.to_string()))?;
    let mut buf = Vec::new();
    f.take(MAX_RULES_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| PlanError::RulesFile(e.to_string()))?;
    Ok(buf)
}

/// Loads the rules for `input`. `Ok(None)` when no rules were named or the repository has none
/// at the merge base. A server failure is `Err(Err(message))` so the caller can fall back.
pub(crate) async fn load_rules(
    b: &dyn Backend,
    input: &Input,
) -> Result<Result<Option<Rules>, String>, PlanError> {
    if let Some(file) = &input.rules_file {
        let bytes = read_local_rules(file)?;
        return Ok(Ok(Some(Rules::parse(&bytes)?)));
    }
    let Some(path) = &input.rules_path else {
        return Ok(Ok(None));
    };
    let mb = match b.merge_base(&input.base, &input.head).await {
        Ok(m) => m,
        Err(e) => return Ok(Err(format!("merge base: {e}"))),
    };
    match b.read_file(&mb, path).await {
        Ok(Some(bytes)) => Ok(Ok(Some(Rules::parse(&bytes)?))),
        Ok(None) => Ok(Ok(None)),
        Err(e) => Ok(Err(format!("reading {path}: {e}"))),
    }
}

/// `semantic` values are lower-case class names; anything else is dropped.
fn semantic_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|c| c.is_ascii_lowercase() || c == b'_')
}

/// Runs `asc ci changed`. A server failure is not fatal: `all=true` and every switch on, with a
/// warning. A malformed rules file is.
pub async fn changed(b: &dyn Backend, input: &Input) -> Result<Plan, PlanError> {
    let mut plan = Plan::default();
    let rules = match load_rules(b, input).await? {
        Ok(r) => r,
        Err(m) => {
            plan.warnings
                .push(format!("asc ci changed: {m}; every switch is on"));
            plan.set("all", "true");
            return Ok(plan);
        }
    };
    let paths = match b.changed_paths(&input.base, &input.head).await {
        Ok(p) => p,
        Err(e) => {
            plan.warnings.push(format!(
                "asc ci changed: diff failed: {e}; every switch is on"
            ));
            plan.set("all", "true");
            if let Some(r) = &rules {
                plan.push_switches(r.all_true());
            }
            return Ok(plan);
        }
    };
    let all = input
        .rules_path
        .as_deref()
        .is_some_and(|p| rules_changed(p, &paths));
    plan.set("all", if all { "true" } else { "false" });
    if let Some(r) = &rules {
        plan.push_switches(r.eval_paths(&paths, all));
    }
    match b.semantic(&input.base, &input.head).await {
        Ok(Some(s)) if semantic_ok(&s) => plan.set("semantic", &s),
        Ok(_) => {}
        Err(e) => plan.warnings.push(format!(
            "asc ci changed: classify failed: {e}; no semantic output"
        )),
    }
    Ok(plan)
}

/// A scripted [`Backend`] for tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    pub struct Fake {
        pub paths: Vec<String>,
        pub fail_diff: bool,
        pub fail_all: bool,
        /// Files at any revision.
        pub files: BTreeMap<String, Vec<u8>>,
        pub semantic: Option<String>,
        pub gha: String,
        /// Selectors (joined by `,`) that match an affected project.
        pub matching: Vec<String>,
        pub calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn log(&self, s: &str) {
            self.calls.lock().unwrap().push(s.to_owned());
        }
        pub fn count(&self, prefix: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.starts_with(prefix))
                .count()
        }
    }

    #[async_trait]
    impl Backend for Fake {
        async fn merge_base(&self, _: &str, _: &str) -> Result<String, BackendError> {
            self.log("merge_base");
            if self.fail_all {
                return Err(BackendError("down".into()));
            }
            Ok("mb".into())
        }
        async fn read_file(&self, rev: &str, path: &str) -> Result<Option<Vec<u8>>, BackendError> {
            self.log(&format!("read_file {rev}:{path}"));
            Ok(self.files.get(path).cloned())
        }
        async fn changed_paths(&self, _: &str, _: &str) -> Result<Vec<String>, BackendError> {
            self.log("diff");
            if self.fail_diff || self.fail_all {
                return Err(BackendError("down".into()));
            }
            Ok(self.paths.clone())
        }
        async fn semantic(&self, _: &str, _: &str) -> Result<Option<String>, BackendError> {
            Ok(self.semantic.clone())
        }
        async fn affected_gha(&self, _: &str, _: &str) -> Result<String, BackendError> {
            self.log("affected");
            if self.fail_all {
                return Err(BackendError("down".into()));
            }
            Ok(self.gha.clone())
        }
        async fn affected_matches(
            &self,
            _: &str,
            _: &str,
            select: &[String],
        ) -> Result<bool, BackendError> {
            self.log("matches");
            if self.fail_all {
                return Err(BackendError("down".into()));
            }
            Ok(self.matching.contains(&select.join(",")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Fake;
    use super::*;

    const RULES: &str = "[switches]\nrs = [\"**/*.rs\"]\ndocs_only = [\"docs/**\"]\nimages = { projects = [\"tag:image\"] }\n";

    fn input() -> Input {
        Input {
            base: "master".into(),
            head: "HEAD".into(),
            rules_path: Some(".asc/ci.toml".into()),
            rules_file: None,
        }
    }

    fn fake(paths: &[&str]) -> Fake {
        let mut f = Fake {
            paths: paths.iter().map(|s| (*s).to_owned()).collect(),
            ..Fake::default()
        };
        f.files.insert(".asc/ci.toml".into(), RULES.into());
        f
    }

    #[tokio::test]
    async fn rules_are_read_at_the_merge_base_without_a_checkout() {
        let f = fake(&["src/a.rs"]);
        let p = changed(&f, &input()).await.unwrap();
        assert_eq!(p.render(), "all=false\ndocs_only=false\nrs=true\n");
        assert_eq!(f.count("merge_base"), 1);
        assert_eq!(f.count("read_file mb:.asc/ci.toml"), 1);
        assert_eq!(f.count("diff"), 1);
    }

    #[tokio::test]
    async fn a_pr_that_edits_the_rules_gets_every_switch_true() {
        let f = fake(&[".asc/ci.toml"]);
        let p = changed(&f, &input()).await.unwrap();
        assert_eq!(p.get("all"), Some("true"));
        assert_eq!(p.get("rs"), Some("true"));
        assert_eq!(p.get("docs_only"), Some("true"));
        assert_eq!(p.get("images"), Some("true"));
    }

    #[tokio::test]
    async fn semantic_is_added_when_offered_and_sanitised() {
        let mut f = fake(&["docs/a.md"]);
        f.semantic = Some("docs".into());
        let p = changed(&f, &input()).await.unwrap();
        assert_eq!(p.get("semantic"), Some("docs"));
        f.semantic = Some("x\nall=false".into());
        let p = changed(&f, &input()).await.unwrap();
        assert_eq!(p.get("semantic"), None);
    }

    #[tokio::test]
    async fn local_rules_file_needs_no_server_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ci.toml");
        std::fs::write(&file, RULES).unwrap();
        let f = fake(&["docs/x"]);
        let mut i = input();
        i.rules_path = None;
        i.rules_file = Some(file);
        let p = changed(&f, &i).await.unwrap();
        assert_eq!(p.get("docs_only"), Some("true"));
        assert_eq!(f.count("read_file"), 0);
        assert_eq!(f.count("merge_base"), 0);
    }

    #[tokio::test]
    async fn a_server_failure_turns_everything_on() {
        let mut f = fake(&[]);
        f.fail_diff = true;
        let p = changed(&f, &input()).await.unwrap();
        assert_eq!(p.get("all"), Some("true"));
        assert_eq!(p.get("rs"), Some("true"));
        assert_eq!(p.warnings.len(), 1);
        f.fail_all = true;
        let p = changed(&f, &input()).await.unwrap();
        assert_eq!(p.render(), "all=true\n");
    }

    #[tokio::test]
    async fn a_malformed_rules_file_is_an_error() {
        let mut f = fake(&[]);
        f.files.insert(".asc/ci.toml".into(), b"turbo = 1".to_vec());
        assert!(changed(&f, &input()).await.is_err());
    }

    #[tokio::test]
    async fn no_rules_still_reports_all() {
        let mut f = fake(&["a"]);
        f.files.clear();
        let p = changed(&f, &input()).await.unwrap();
        assert_eq!(p.render(), "all=false\n");
    }
}
