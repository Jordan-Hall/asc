//! `asc ci affected` (SPEC §12.2): `asc affected --format gha` plus the project-selector switches
//! of `.asc/ci.toml`. If the server fails it writes `all=true`, sets every switch true and
//! warns, so a broken plan job runs more CI, never less.

use crate::changed::{Backend, Input, Plan, PlanError, load_rules};
use crate::rules::{Rules, rules_changed};

/// The keys `asc affected --format gha` writes. Anything else in its output is dropped, so the
/// child can never add a key of its own (or of the rules) to `$GITHUB_OUTPUT`.
const GHA_KEYS: &[&str] = &[
    "affected",
    "count",
    "all",
    "hidden",
    "rust",
    "python",
    "dotnet",
    "node",
    "turbo_filter",
    "nx_projects",
    "matrix",
];

/// Largest `--format gha` text accepted.
const MAX_GHA_BYTES: usize = 8 * 1024 * 1024;

/// The accepted `key=value` lines of the gha output.
pub fn parse_gha(text: &str) -> Vec<(String, String)> {
    if text.len() > MAX_GHA_BYTES {
        return Vec::new();
    }
    text.lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, v)| GHA_KEYS.contains(k) && !v.chars().any(|c| c == '\r' || c == '\0'))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Everything on: the failure fallback.
fn everything(rules: Option<&Rules>) -> Plan {
    let mut plan = Plan::default();
    plan.set("all", "true");
    for eco in ["rust", "python", "dotnet", "node"] {
        plan.set(eco, "true");
    }
    plan.set("turbo_filter", "--filter=*");
    plan.set("nx_projects", "*");
    if let Some(r) = rules {
        plan.push_switches(r.all_true());
    }
    plan
}

/// Runs `asc ci affected`. Only a malformed rules file is an error.
pub async fn affected(b: &dyn Backend, input: &Input) -> Result<Plan, PlanError> {
    let rules = match load_rules(b, input).await? {
        Ok(r) => r,
        Err(m) => {
            let mut p = everything(None);
            p.warnings
                .push(format!("asc ci affected: {m}; every switch is on"));
            return Ok(p);
        }
    };
    let fallback = |why: String, rules: &Option<Rules>| {
        let mut p = everything(rules.as_ref());
        p.warnings
            .push(format!("asc ci affected: {why}; every switch is on"));
        p
    };
    let gha = match b.affected_gha(&input.base, &input.head).await {
        Ok(t) => parse_gha(&t),
        Err(e) => return Ok(fallback(format!("affected failed: {e}"), &rules)),
    };
    if gha.is_empty() {
        return Ok(fallback("affected answered nothing usable".into(), &rules));
    }
    let mut plan = Plan {
        lines: gha,
        warnings: Vec::new(),
    };
    // A PR that edits the rules cannot narrow its own CI through a project switch either.
    let mut all = plan.get("all") == Some("true");
    if !all && let Some(path) = &input.rules_path {
        match b.changed_paths(&input.base, &input.head).await {
            Ok(paths) => all = rules_changed(path, &paths),
            Err(e) => return Ok(fallback(format!("diff failed: {e}"), &rules)),
        }
    }
    if all {
        plan.set("all", "true");
    }
    if let Some(r) = &rules {
        for (output, select) in r.project_switches() {
            let on = all
                || match b.affected_matches(&input.base, &input.head, select).await {
                    Ok(m) => m,
                    Err(e) => {
                        return Ok(fallback(format!("affected {output} failed: {e}"), &rules));
                    }
                };
            plan.set(output, if on { "true" } else { "false" });
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changed::fake::Fake;

    const RULES: &str = "[switches]\nimages = { projects = [\"tag:image\"] }\nlibs = { projects = [\"eco:rust\", \"path:libs\"] }\nrs = [\"**/*.rs\"]\n";
    const GHA: &str = "affected=[\"node:a\"]\ncount=1\nall=false\nhidden=0\nrust=false\npython=false\ndotnet=false\nnode=true\nturbo_filter=--filter=a\nnx_projects=a\nmatrix={}\n";

    fn input() -> Input {
        Input {
            base: "master".into(),
            head: "HEAD".into(),
            rules_path: Some(".asc/ci.toml".into()),
            rules_file: None,
        }
    }

    fn fake() -> Fake {
        let mut f = Fake {
            gha: GHA.into(),
            ..Fake::default()
        };
        f.files.insert(".asc/ci.toml".into(), RULES.into());
        f
    }

    #[tokio::test]
    async fn selector_switches_follow_the_affected_projects() {
        let mut f = fake();
        f.matching = vec!["eco:rust,path:libs".into()];
        let p = affected(&f, &input()).await.unwrap();
        assert_eq!(p.get("images"), Some("false"));
        assert_eq!(p.get("libs"), Some("true"));
        assert_eq!(p.get("rs"), None, "glob switches belong to `changed`");
        assert_eq!(p.get("node"), Some("true"));
        assert_eq!(p.get("count"), Some("1"));
        assert_eq!(f.count("affected"), 1);
    }

    #[tokio::test]
    async fn failure_falls_back_to_everything() {
        let mut f = fake();
        f.fail_all = true;
        let p = affected(&f, &input()).await.unwrap();
        assert_eq!(p.get("all"), Some("true"));
        assert_eq!(p.warnings.len(), 1);

        // Rules readable, affected itself fails: every switch is on.
        let mut f = fake();
        f.gha = String::new();
        let p = affected(&f, &input()).await.unwrap();
        for k in ["all", "images", "libs", "rs", "rust", "node"] {
            assert_eq!(p.get(k), Some("true"), "{k}");
        }
        assert_eq!(p.get("turbo_filter"), Some("--filter=*"));
    }

    #[tokio::test]
    async fn a_failing_selector_call_falls_back() {
        struct Half(Fake);
        #[async_trait::async_trait]
        impl Backend for Half {
            async fn merge_base(
                &self,
                a: &str,
                b: &str,
            ) -> Result<String, crate::changed::BackendError> {
                self.0.merge_base(a, b).await
            }
            async fn read_file(
                &self,
                r: &str,
                p: &str,
            ) -> Result<Option<Vec<u8>>, crate::changed::BackendError> {
                self.0.read_file(r, p).await
            }
            async fn changed_paths(
                &self,
                a: &str,
                b: &str,
            ) -> Result<Vec<String>, crate::changed::BackendError> {
                self.0.changed_paths(a, b).await
            }
            async fn semantic(
                &self,
                a: &str,
                b: &str,
            ) -> Result<Option<String>, crate::changed::BackendError> {
                self.0.semantic(a, b).await
            }
            async fn affected_gha(
                &self,
                a: &str,
                b: &str,
            ) -> Result<String, crate::changed::BackendError> {
                self.0.affected_gha(a, b).await
            }
            async fn affected_matches(
                &self,
                _: &str,
                _: &str,
                _: &[String],
            ) -> Result<bool, crate::changed::BackendError> {
                Err(crate::changed::BackendError("boom".into()))
            }
        }
        let p = affected(&Half(fake()), &input()).await.unwrap();
        assert_eq!(p.get("all"), Some("true"));
        assert_eq!(p.get("images"), Some("true"));
    }

    #[tokio::test]
    async fn editing_the_rules_turns_project_switches_on() {
        let mut f = fake();
        f.paths = vec![".asc/ci.toml".into()];
        let p = affected(&f, &input()).await.unwrap();
        assert_eq!(p.get("all"), Some("true"));
        assert_eq!(p.get("images"), Some("true"));
        assert_eq!(p.get("libs"), Some("true"));
        assert_eq!(f.count("matches"), 0);
    }

    #[tokio::test]
    async fn the_child_cannot_inject_keys_into_the_output() {
        let mut f = fake();
        f.gha = format!("{GHA}evil=1\nrust=true\x00\nimages=true\n");
        let p = affected(&f, &input()).await.unwrap();
        assert_eq!(p.get("evil"), None);
        assert_eq!(p.get("images"), Some("false"));
        assert!(parse_gha("all=tr\rue\n").is_empty());
    }
}
