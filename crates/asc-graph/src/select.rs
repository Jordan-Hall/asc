//! Project selectors (SPEC §12A.1): CLI, RPC and config.

use asc_core::path::RepoPath;
use serde::{Deserialize, Serialize};

use crate::model::{Ecosystem, Project, ProjectKey, display_ids};

/// Why a selector failed to parse or resolve.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    #[error("empty selector")]
    Empty,
    #[error("unknown ecosystem {0:?}")]
    UnknownEcosystem(String),
    #[error("invalid path selector {0:?}")]
    BadPath(String),
    #[error("selector {0:?} is ambiguous: {names}", names = .1.join(", "))]
    Ambiguous(String, Vec<String>),
    #[error("selector {0:?} matches no project")]
    NoMatch(String),
}

/// What a selector matches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Term {
    /// `<eco>:<name>` with an optional `@<root>` suffix (`.` is the repository root).
    Id {
        eco: Ecosystem,
        name: String,
        root: Option<String>,
    },
    /// A bare name; it must be unique.
    Name(String),
    /// `path:<root>`: projects whose root is exactly this directory (`None` is the repository root).
    Path(Option<RepoPath>),
    /// `tag:<t>`.
    Tag(String),
    /// `eco:<eco>`.
    Eco(Ecosystem),
}

/// A selector; `!` in front excludes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selector {
    pub exclude: bool,
    pub term: Term,
}

impl Selector {
    /// Parses `[!]<display id> | <name> | path:<root> | tag:<t> | eco:<eco>`.
    pub fn parse(s: &str) -> Result<Self, SelectError> {
        let (exclude, s) = match s.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        if s.is_empty() {
            return Err(SelectError::Empty);
        }
        let term = if let Some(p) = s.strip_prefix("path:") {
            Term::Path(match p {
                "." | "" => None,
                p => Some(
                    RepoPath::new(p.trim_end_matches('/'))
                        .map_err(|_| SelectError::BadPath(p.to_owned()))?,
                ),
            })
        } else if let Some(t) = s.strip_prefix("tag:") {
            non_empty(t).map(|t| Term::Tag(t.to_owned()))?
        } else if let Some(e) = s.strip_prefix("eco:") {
            Term::Eco(
                Ecosystem::parse(e).ok_or_else(|| SelectError::UnknownEcosystem(e.to_owned()))?,
            )
        } else if let Some((eco, rest)) = s
            .split_once(':')
            .and_then(|(e, r)| Some((Ecosystem::parse(e)?, r)))
        {
            let (name, root) = split_root(non_empty(rest)?);
            Term::Id {
                eco,
                name: name.to_owned(),
                root: root.map(str::to_owned),
            }
        } else {
            Term::Name(s.to_owned())
        };
        Ok(Self { exclude, term })
    }

    /// True when `p` matches the term, ignoring `exclude`.
    pub fn matches(&self, p: &Project) -> bool {
        match &self.term {
            Term::Id { eco, name, root } => {
                *eco == p.key.eco
                    && p.name == name.as_bytes()
                    && root.as_ref().is_none_or(|r| *r == p.root_lossy())
            }
            Term::Name(n) => p.name == n.as_bytes(),
            Term::Path(r) => p.root == *r,
            Term::Tag(t) => p.tags.iter().any(|x| x == t.as_bytes()),
            Term::Eco(e) => p.key.eco == *e,
        }
    }
}

fn non_empty(s: &str) -> Result<&str, SelectError> {
    if s.is_empty() {
        Err(SelectError::Empty)
    } else {
        Ok(s)
    }
}

/// Splits a collision suffix `name@root`. A leading `@` is an npm scope, not a suffix.
fn split_root(s: &str) -> (&str, Option<&str>) {
    match s.char_indices().skip(1).find(|&(_, c)| c == '@') {
        Some((i, _)) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    }
}

/// The projects (as indexes into `projects`, in order) chosen by `selectors`.
///
/// Positive selectors union; with none, every project starts selected. Exclusions
/// then remove. A bare name that matches several projects is an error.
pub fn select(projects: &[Project], selectors: &[Selector]) -> Result<Vec<usize>, SelectError> {
    let ids = display_ids(projects);
    let mut keep = vec![!selectors.iter().any(|s| !s.exclude); projects.len()];
    // Exclusions apply after every inclusion, whatever their order.
    for exclude in [false, true] {
        for s in selectors.iter().filter(|s| s.exclude == exclude) {
            let hits: Vec<usize> = (0..projects.len())
                .filter(|&i| s.matches(&projects[i]))
                .collect();
            if let (Term::Name(n), true) = (&s.term, hits.len() > 1) {
                let names = hits.iter().map(|&i| ids[i].clone()).collect();
                return Err(SelectError::Ambiguous(n.clone(), names));
            }
            for i in hits {
                keep[i] = !exclude;
            }
        }
    }
    Ok((0..projects.len()).filter(|&i| keep[i]).collect())
}

/// Resolves a selector that must match exactly one project (config `select`).
pub fn resolve_one<'a>(
    projects: &'a [Project],
    selector: &Selector,
) -> Result<&'a ProjectKey, SelectError> {
    let hits: Vec<&Project> = projects.iter().filter(|p| selector.matches(p)).collect();
    match hits.as_slice() {
        [one] => Ok(&one.key),
        [] => Err(SelectError::NoMatch(describe(selector))),
        many => {
            let ids = display_ids(projects);
            let names = projects
                .iter()
                .zip(ids)
                .filter(|(p, _)| many.iter().any(|m| m.key == p.key))
                .map(|(_, id)| id)
                .collect();
            Err(SelectError::Ambiguous(describe(selector), names))
        }
    }
}

fn describe(s: &Selector) -> String {
    match &s.term {
        Term::Id {
            eco,
            name,
            root: None,
        } => format!("{eco}:{name}"),
        Term::Id {
            eco,
            name,
            root: Some(r),
        } => format!("{eco}:{name}@{r}"),
        Term::Name(n) => n.clone(),
        Term::Path(r) => format!(
            "path:{}",
            r.as_ref().map_or_else(|| ".".into(), ToString::to_string)
        ),
        Term::Tag(t) => format!("tag:{t}"),
        Term::Eco(e) => format!("eco:{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProjectKind;

    fn proj(eco: Ecosystem, manifest: &str, name: &str, tags: &[&str]) -> Project {
        let key = ProjectKey {
            eco,
            manifest: RepoPath::new(manifest).unwrap(),
        };
        let mut p = Project::new(key, name, ProjectKind::Package);
        p.tags = tags.iter().map(|t| t.as_bytes().to_vec()).collect();
        p
    }

    fn asc() -> Vec<Project> {
        vec![
            proj(
                Ecosystem::Rust,
                "packages/dsl/a/Cargo.toml",
                "core",
                &["dsl"],
            ),
            proj(Ecosystem::Rust, "apps/b/Cargo.toml", "core", &[]),
            proj(
                Ecosystem::Rust,
                "apps/p/pipeline-api/Cargo.toml",
                "pipeline-api",
                &["deploy"],
            ),
            proj(
                Ecosystem::Node,
                "apps/p/pipeline-api/package.json",
                "analysis-pipeline-api",
                &["deploy"],
            ),
            proj(Ecosystem::Node, "packages/ui/package.json", "@asc/ui", &[]),
            proj(Ecosystem::Python, "pyproject.toml", "asc-soma", &[]),
        ]
    }

    fn sel(ps: &[Project], args: &[&str]) -> Result<Vec<String>, SelectError> {
        let ss: Vec<_> = args.iter().map(|a| Selector::parse(a).unwrap()).collect();
        let ids = display_ids(ps);
        Ok(select(ps, &ss)?
            .into_iter()
            .map(|i| ids[i].clone())
            .collect())
    }

    #[test]
    fn parse_forms() {
        let t = |s: &str| Selector::parse(s).unwrap();
        assert_eq!(
            t("cargo:core@apps/b").term,
            Term::Id {
                eco: Ecosystem::Rust,
                name: "core".into(),
                root: Some("apps/b".into())
            }
        );
        assert_eq!(
            t("bun:@asc/ui").term,
            Term::Id {
                eco: Ecosystem::Node,
                name: "@asc/ui".into(),
                root: None
            }
        );
        assert_eq!(
            t("node:@asc/ui@apps/web").term,
            Term::Id {
                eco: Ecosystem::Node,
                name: "@asc/ui".into(),
                root: Some("apps/web".into())
            }
        );
        assert_eq!(t("@asc/ui").term, Term::Name("@asc/ui".into()));
        assert_eq!(
            t("path:apps/b/").term,
            Term::Path(Some(RepoPath::new("apps/b").unwrap()))
        );
        assert_eq!(t("path:.").term, Term::Path(None));
        assert_eq!(t("tag:deploy").term, Term::Tag("deploy".into()));
        assert_eq!(t("eco:uv").term, Term::Eco(Ecosystem::Python));
        assert!(t("!tag:x").exclude);
        assert_eq!(Selector::parse(""), Err(SelectError::Empty));
        assert_eq!(Selector::parse("!"), Err(SelectError::Empty));
        assert_eq!(
            Selector::parse("eco:go"),
            Err(SelectError::UnknownEcosystem("go".into()))
        );
        assert!(matches!(
            Selector::parse("path:a//b"),
            Err(SelectError::BadPath(_))
        ));
    }

    #[test]
    fn selection() {
        let ps = asc();
        assert_eq!(
            sel(&ps, &["eco:node"]).unwrap(),
            ["node:analysis-pipeline-api", "node:@asc/ui"]
        );
        assert_eq!(
            sel(&ps, &["tag:deploy", "!eco:node"]).unwrap(),
            ["rust:pipeline-api"]
        );
        assert_eq!(
            sel(&ps, &["!eco:rust", "!eco:node"]).unwrap(),
            ["python:asc-soma"]
        );
        assert_eq!(
            sel(&ps, &["rust:core@apps/b"]).unwrap(),
            ["rust:core@apps/b"]
        );
        assert_eq!(sel(&ps, &["asc-soma"]).unwrap(), ["python:asc-soma"]);
        assert_eq!(
            sel(&ps, &["path:apps/p/pipeline-api"]).unwrap(),
            ["rust:pipeline-api", "node:analysis-pipeline-api"]
        );
        assert_eq!(sel(&ps, &["path:."]).unwrap(), ["python:asc-soma"]);
        assert_eq!(sel(&ps, &["tag:none"]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn ambiguous_bare_name() {
        let ps = asc();
        let err = sel(&ps, &["core"]).unwrap_err();
        assert_eq!(
            err,
            SelectError::Ambiguous(
                "core".into(),
                vec!["rust:core@packages/dsl/a".into(), "rust:core@apps/b".into()]
            )
        );
        assert!(err.to_string().contains("rust:core@apps/b"));
        assert!(sel(&ps, &["rust:core"]).unwrap().len() == 2);
    }

    #[test]
    fn resolve_exactly_one() {
        let ps = asc();
        let one = |s: &str| resolve_one(&ps, &Selector::parse(s).unwrap()).cloned();
        assert_eq!(
            one("node:@asc/ui").unwrap().manifest.to_string(),
            "packages/ui/package.json"
        );
        assert_eq!(
            one("node:nope"),
            Err(SelectError::NoMatch("node:nope".into()))
        );
        assert!(matches!(one("rust:core"), Err(SelectError::Ambiguous(..))));
        assert!(matches!(one("tag:deploy"), Err(SelectError::Ambiguous(..))));
    }
}
