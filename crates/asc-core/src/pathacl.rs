//! Path rules: per-directory access levels inside one repository (SPEC §15.5.1).
//!
//! A rule is `directory → subject → level`. A viewer's level on a path comes from the deepest
//! directory, ancestor-or-self of the path, that has a rule matching the viewer; among the
//! viewer's rules there the highest level wins (a user in two teams gets the better one). With
//! no matching rule anywhere the repository level applies. The result is capped by the
//! repository level, so a rule never grants more than the repository does.
//!
//! Directories are plain prefixes, not globs, so "can this directory be listed" has an exact
//! answer: a directory is listed when the viewer can read it or when some rule below it gives
//! the viewer read (then the listing is filtered entry by entry).

use std::collections::BTreeMap;

use crate::policy::{Kind, Who, name_glob};

/// Most rules on one repository.
pub const MAX_RULES: usize = 512;
/// Longest rule directory, in bytes.
pub const MAX_DIR_LEN: usize = 1024;
/// Longest subject, in bytes.
pub const MAX_SUBJECT_LEN: usize = 128;

/// An access level, ordered from least to most.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Nothing: the path shows as restricted.
    None,
    /// No source; cached build outputs and remote builds only.
    Build,
    Read,
    Write,
    Admin,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Build => "build",
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Self::None,
            "build" => Self::Build,
            "read" => Self::Read,
            "write" => Self::Write,
            "admin" => Self::Admin,
            _ => return None,
        })
    }
}

/// Who a path rule names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    /// `*`: everyone.
    Anyone,
    /// `user:<glob>`, `agent:<glob>`, `ci:<glob>`, `deploy:<glob>`.
    Of { kind: Kind, glob: String },
    /// `team:<name>`.
    Team(String),
}

impl Subject {
    pub fn parse(text: &str) -> Result<Self, AclError> {
        if text.is_empty() || text.len() > MAX_SUBJECT_LEN {
            return Err(AclError("a subject is empty or too long".into()));
        }
        if text == "*" {
            return Ok(Self::Anyone);
        }
        let (head, rest) = text.split_once(':').ok_or_else(|| {
            AclError(format!(
                "subject {text:?} is not `*`, `team:<name>` or `<kind>:<name>`"
            ))
        })?;
        let ok = !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._@+-/*".contains(&b));
        if !ok {
            return Err(AclError(format!(
                "subject {text:?} has an empty or unusual name"
            )));
        }
        let kind = match head {
            "team" => return Ok(Self::Team(rest.to_owned())),
            "user" => Kind::User,
            "agent" => Kind::Agent,
            "ci" => Kind::Ci,
            "deploy" => Kind::Deploy,
            _ => return Err(AclError(format!("unknown subject kind {head:?}"))),
        };
        Ok(Self::Of {
            kind,
            glob: rest.to_owned(),
        })
    }

    pub fn render(&self) -> String {
        match self {
            Self::Anyone => "*".into(),
            Self::Of { kind, glob } => format!("{}:{glob}", kind.as_str()),
            Self::Team(t) => format!("team:{t}"),
        }
    }

    fn matches(&self, v: &Viewer) -> bool {
        match self {
            Self::Anyone => true,
            Self::Of { kind, glob } => v.who.kind == Some(*kind) && name_glob(glob, &v.who.name),
            Self::Team(t) => v.teams.iter().any(|m| m == t),
        }
    }
}

/// A refused rule.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct AclError(pub String);

/// One rule as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// Repository-relative directory, `/` separated, no leading or trailing `/`. Empty is the root.
    pub dir: String,
    pub subject: Subject,
    pub level: Level,
}

/// A caller: the principal, its teams, and its level on the whole repository.
#[derive(Clone, Debug)]
pub struct Viewer {
    pub who: Who,
    pub teams: Vec<String>,
    pub repo_level: Level,
}

/// A repository's compiled rules.
#[derive(Clone, Debug, Default)]
pub struct PathAcl {
    /// Rules keyed by directory, as bytes.
    by_dir: BTreeMap<Vec<u8>, Vec<(Subject, Level)>>,
}

fn valid_dir(d: &str) -> Result<(), AclError> {
    if d.len() > MAX_DIR_LEN {
        return Err(AclError("a rule directory is too long".into()));
    }
    if d.is_empty() {
        return Ok(());
    }
    if d.starts_with('/') || d.ends_with('/') || d.contains('\\') || d.chars().any(char::is_control)
    {
        return Err(AclError(format!(
            "rule directory {d:?} must be relative, with `/` separators and no trailing `/`"
        )));
    }
    if d.split('/').any(|s| matches!(s, "" | "." | "..")) {
        return Err(AclError(format!(
            "rule directory {d:?} has an empty, `.` or `..` segment"
        )));
    }
    if d.contains(['*', '?', '[']) {
        return Err(AclError(format!(
            "rule directory {d:?} is a glob; path rules name plain directories"
        )));
    }
    Ok(())
}

/// The ancestors-or-self of `path`, deepest first, ending with the root (empty).
fn ancestors(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    let cuts = path
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, b)| **b == b'/')
        .map(|(i, _)| i);
    std::iter::once(path.len())
        .filter(move |_| !path.is_empty())
        .chain(cuts)
        .map(move |i| &path[..i])
        .chain(std::iter::once(&path[..0]))
}

impl PathAcl {
    /// Validates and compiles `rules`. Two rules for one directory and subject are an error.
    pub fn new(rules: &[Rule]) -> Result<Self, AclError> {
        if rules.len() > MAX_RULES {
            return Err(AclError(format!(
                "too many path rules (at most {MAX_RULES})"
            )));
        }
        let mut by_dir: BTreeMap<Vec<u8>, Vec<(Subject, Level)>> = BTreeMap::new();
        for r in rules {
            valid_dir(&r.dir)?;
            let slot = by_dir.entry(r.dir.as_bytes().to_vec()).or_default();
            if slot.iter().any(|(s, _)| *s == r.subject) {
                return Err(AclError(format!(
                    "two rules for {} on {:?}",
                    r.subject.render(),
                    r.dir
                )));
            }
            slot.push((r.subject.clone(), r.level));
        }
        Ok(Self { by_dir })
    }

    pub fn is_empty(&self) -> bool {
        self.by_dir.is_empty()
    }

    /// The viewer's level on `path` (a file or directory).
    pub fn level(&self, v: &Viewer, path: &[u8]) -> Level {
        self.decide(v, path).0
    }

    /// The viewer's level on `path` and the directory whose rules decided it (`None`: no rule
    /// matched, so the repository level stands).
    pub fn decide<'p>(&self, v: &Viewer, path: &'p [u8]) -> (Level, Option<&'p [u8]>) {
        for dir in ancestors(path) {
            if let Some(rules) = self.by_dir.get(dir)
                && let Some(best) = rules
                    .iter()
                    .filter(|(s, _)| s.matches(v))
                    .map(|(_, l)| *l)
                    .max()
            {
                return (best.min(v.repo_level), Some(dir));
            }
        }
        (v.repo_level, None)
    }

    /// Whether some path is unreadable for `v`. Every path's level is the level at a rule
    /// directory or the repository level, so checking those is exact.
    pub fn restricts(&self, v: &Viewer) -> bool {
        v.repo_level < Level::Read || self.by_dir.keys().any(|d| !self.can_read(v, d))
    }

    /// Whether some path is below the viewer's repository level: unreadable, or (for a viewer
    /// who may write) not writable. For a reader this is [`Self::restricts`].
    pub fn limits(&self, v: &Viewer) -> bool {
        self.restricts(v) || self.by_dir.keys().any(|d| self.level(v, d) < v.repo_level)
    }

    /// Whether the viewer reads `dir` and everything below it (no rule under it withholds read).
    pub fn subtree_readable(&self, v: &Viewer, dir: &[u8]) -> bool {
        self.can_read(v, dir)
            && self
                .by_dir
                .keys()
                .filter(|d| {
                    dir.is_empty()
                        || (d.len() > dir.len() && d.starts_with(dir) && d[dir.len()] == b'/')
                })
                .all(|d| self.can_read(v, d))
    }

    pub fn can_read(&self, v: &Viewer, path: &[u8]) -> bool {
        self.level(v, path) >= Level::Read
    }

    pub fn can_write(&self, v: &Viewer, path: &[u8]) -> bool {
        self.level(v, path) >= Level::Write
    }

    /// Whether the directory `dir` may be listed: the viewer reads it, or some rule strictly
    /// below it gives the viewer read. A listing must still filter each entry with
    /// [`Self::entry_visible`].
    pub fn can_list(&self, v: &Viewer, dir: &[u8]) -> bool {
        self.can_read(v, dir) || self.reads_below(v, dir)
    }

    /// Whether an entry at `path` keeps its content in a listing; a `false` entry is shown by
    /// name and flagged restricted.
    pub fn entry_visible(&self, v: &Viewer, path: &[u8], is_dir: bool) -> bool {
        self.can_read(v, path) || (is_dir && self.reads_below(v, path))
    }

    /// The directories where a rule matches `v`, with the level it gives there (capped at the
    /// repository level), in path order. With the repository level this is all a client needs
    /// to compute [`Self::level`] for `v` (the deepest listed ancestor decides), and it names no
    /// directory whose rules are only about others. A hidden directory inside a hidden one is
    /// left out: it changes no level, and its name is one no listing shows.
    pub fn decided_dirs(&self, v: &Viewer) -> Vec<(Vec<u8>, Level)> {
        let mut out: Vec<(Vec<u8>, Level)> = Vec::new();
        for (dir, rules) in &self.by_dir {
            let Some(best) = rules.iter().filter(|(s, _)| s.matches(v)).map(|(_, l)| *l).max()
            else {
                continue;
            };
            let level = best.min(v.repo_level);
            let inherited = out
                .iter()
                .filter(|(d, _)| {
                    d.is_empty() || (dir.starts_with(d) && dir.get(d.len()) == Some(&b'/'))
                })
                .max_by_key(|(d, _)| d.len())
                .map(|(_, l)| *l);
            if level == Level::None && inherited == Some(Level::None) {
                continue;
            }
            out.push((dir.clone(), level));
        }
        out
    }

    fn reads_below(&self, v: &Viewer, dir: &[u8]) -> bool {
        let mut prefix = dir.to_vec();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        self.by_dir
            .range(prefix.clone()..)
            .take_while(|(d, _)| d.starts_with(&prefix))
            .filter(|(d, _)| d.len() > dir.len())
            .any(|(d, _)| self.can_read(v, d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(dir: &str, subject: &str, level: Level) -> Rule {
        Rule {
            dir: dir.into(),
            subject: Subject::parse(subject).unwrap(),
            level,
        }
    }

    fn user(name: &str, teams: &[&str], repo_level: Level) -> Viewer {
        Viewer {
            who: Who::new(Kind::User, name),
            teams: teams.iter().map(|t| (*t).to_owned()).collect(),
            repo_level,
        }
    }

    fn acl() -> PathAcl {
        PathAcl::new(&[
            rule("secret", "*", Level::None),
            rule("secret", "team:core", Level::Write),
            rule("secret/docs", "*", Level::Read),
            rule("vendor", "user:bot*", Level::Read),
            rule("infra", "team:ops", Level::Admin),
        ])
        .unwrap()
    }

    #[test]
    fn decided_dirs_name_only_the_viewers_directories_and_no_hidden_one_inside_another() {
        let acl = PathAcl::new(&[
            rule("", "*", Level::Read),
            rule("secret", "*", Level::None),
            rule("secret/plans", "*", Level::None),
            rule("secret/docs", "user:bob", Level::Write),
            rule("ops", "user:carol", Level::Write),
        ])
        .unwrap();
        let bob = user("bob", &[], Level::Write);
        assert_eq!(
            acl.decided_dirs(&bob),
            [
                (b"".to_vec(), Level::Read),
                (b"secret".to_vec(), Level::None),
                (b"secret/docs".to_vec(), Level::Write),
            ]
        );
        for p in [&b"secret/plans/x"[..], b"secret/docs/a", b"ops/y", b"src"] {
            let deepest = acl
                .decided_dirs(&bob)
                .into_iter()
                .filter(|(d, _)| d.is_empty() || p.starts_with(&[&d[..], b"/"].concat()))
                .max_by_key(|(d, _)| d.len())
                .map(|(_, l)| l);
            assert_eq!(deepest, Some(acl.level(&bob, p)), "{p:?}");
        }
    }

    #[test]
    fn restricts_is_exact_over_the_rule_directories() {
        let a = acl();
        assert!(a.restricts(&user("alice", &[], Level::Write)));
        // `core` reads everything `secret` hides; nothing else is below read.
        assert!(!a.restricts(&user("bob", &["core"], Level::Write)));
        assert!(a.restricts(&user("bob", &["core"], Level::None)));
        assert!(!PathAcl::default().restricts(&user("x", &[], Level::Read)));
    }

    #[test]
    fn the_deepest_matching_directory_decides() {
        let a = acl();
        let alice = user("alice", &[], Level::Write);
        assert_eq!(a.level(&alice, b"src/main.rs"), Level::Write);
        assert_eq!(a.level(&alice, b"secret"), Level::None);
        assert_eq!(a.level(&alice, b"secret/key.pem"), Level::None);
        assert_eq!(a.level(&alice, b"secret/docs/readme.md"), Level::Read);
        // `secretive` is not under `secret`.
        assert_eq!(a.level(&alice, b"secretive/x"), Level::Write);
    }

    #[test]
    fn the_best_rule_for_the_viewer_wins_and_the_repo_level_caps_it() {
        let a = acl();
        let core = user("bob", &["core"], Level::Write);
        assert_eq!(a.level(&core, b"secret/key.pem"), Level::Write);
        let ops_reader = user("carol", &["ops"], Level::Read);
        assert_eq!(a.level(&ops_reader, b"infra/main.tf"), Level::Read);
        let bot = Viewer {
            who: Who::new(Kind::User, "bot-1"),
            teams: vec![],
            repo_level: Level::Write,
        };
        // A rule that does not match the viewer leaves the repository level.
        assert_eq!(a.level(&bot, b"vendor/lib.c"), Level::Read);
        assert_eq!(
            a.level(&user("dan", &[], Level::Write), b"vendor/lib.c"),
            Level::Write
        );
    }

    #[test]
    fn listing_shows_a_restricted_directory_only_down_to_what_is_readable() {
        let a = acl();
        let alice = user("alice", &[], Level::Write);
        assert!(a.can_list(&alice, b""));
        // `secret` is listed because `secret/docs` is readable below it.
        assert!(a.can_list(&alice, b"secret"));
        assert!(a.entry_visible(&alice, b"secret/docs", true));
        assert!(!a.entry_visible(&alice, b"secret/key.pem", false));
        assert!(!a.entry_visible(&alice, b"secret/inner", true));
        let a2 = PathAcl::new(&[rule("hidden", "*", Level::None)]).unwrap();
        assert!(!a2.can_list(&alice, b"hidden"));
        assert!(!a2.entry_visible(&alice, b"hidden", true));
        assert!(a2.entry_visible(&alice, b"shown", true));
    }

    #[test]
    fn rules_are_validated() {
        for dir in ["/a", "a/", "a//b", "a/../b", "a/*", "a\\b"] {
            assert!(
                PathAcl::new(&[rule(dir, "*", Level::None)]).is_err(),
                "{dir}"
            );
        }
        assert!(PathAcl::new(&[rule("a", "*", Level::None), rule("a", "*", Level::Read)]).is_err());
        assert!(Subject::parse("role:x").is_err());
        assert!(Subject::parse("user:").is_err());
        assert_eq!(Subject::parse("team:core").unwrap().render(), "team:core");
        assert!(PathAcl::new(&[rule("", "*", Level::Read)]).is_ok());
        let many: Vec<Rule> = (0..=MAX_RULES)
            .map(|i| rule(&format!("d{i}"), "*", Level::Read))
            .collect();
        assert!(PathAcl::new(&many).is_err());
    }

    #[test]
    fn a_root_rule_applies_everywhere_below() {
        let a =
            PathAcl::new(&[rule("", "*", Level::Read), rule("open", "*", Level::Write)]).unwrap();
        let v = user("x", &[], Level::Admin);
        assert_eq!(a.level(&v, b"any/file"), Level::Read);
        assert_eq!(a.level(&v, b"open/file"), Level::Write);
        assert_eq!(Level::parse("build"), Some(Level::Build));
        assert!(Level::Build < Level::Read);
    }

    #[test]
    fn a_read_rule_limits_a_writer_but_not_a_reader() {
        let a = PathAcl::new(&[rule("secret", "*", Level::Read)]).unwrap();
        assert!(a.limits(&user("bob", &[], Level::Write)));
        assert!(!a.restricts(&user("bob", &[], Level::Write)));
        assert!(!a.limits(&user("bob", &[], Level::Read)));
    }

    #[test]
    fn a_subtree_is_readable_only_when_no_rule_below_hides_part_of_it() {
        let a = acl();
        let bob = user("bob", &[], Level::Write);
        assert!(
            !a.subtree_readable(&bob, b""),
            "secret is hidden under the root"
        );
        assert!(a.subtree_readable(&bob, b"vendor"));
        assert!(!a.subtree_readable(&bob, b"secret"));
        assert!(a.subtree_readable(&bob, b"secret/docs"));
        let core = user("carol", &["core"], Level::Write);
        assert!(a.subtree_readable(&core, b""));
    }
}
