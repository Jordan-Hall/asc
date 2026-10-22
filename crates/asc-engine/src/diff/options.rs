//! Diff settings: command-line style options and the `diff.*` configuration keys they default from.

use asc_core::path::RepoPath;

use super::line::Algorithm;
use super::rename::RenameOptions;

/// What one side of a comparison is, for `diff.mnemonicPrefix`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SideKind {
    #[default]
    Commit,
    Index,
    Worktree,
    Object,
}

impl SideKind {
    fn mnemonic(self) -> &'static str {
        match self {
            Self::Commit => "c/",
            Self::Index => "i/",
            Self::Worktree => "w/",
            Self::Object => "o/",
        }
    }
}

/// A bad configuration value.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("bad value '{value}' for {key}")]
pub struct ConfigError {
    pub key: String,
    pub value: String,
}

/// Everything the diff pipeline reads from the command line and configuration.
#[derive(Clone, Debug)]
pub struct DiffOptions {
    pub algorithm: Algorithm,
    pub indent_heuristic: bool,
    /// `-U<n>` / `diff.context`.
    pub context: usize,
    /// `--inter-hunk-context` / `diff.interHunkContext`.
    pub inter_hunk: usize,
    /// `diff.noprefix`.
    pub no_prefix: bool,
    /// `diff.mnemonicPrefix`.
    pub mnemonic_prefix: bool,
    /// `--src-prefix` / `--dst-prefix`; they win over the two settings above.
    pub src_prefix: Option<String>,
    pub dst_prefix: Option<String>,
    /// The kinds of the two sides, which pick the mnemonic prefixes.
    pub sides: (SideKind, SideKind),
    /// `diff.relative` is set; the caller resolves it to a directory.
    pub relative_config: bool,
    /// `--relative[=dir]`: only paths below it are shown, with it stripped.
    pub relative: Option<RepoPath>,
    /// Renames; `None` turns detection off.
    pub renames: Option<RenameOptions>,
    /// Hex digits shown for object ids.
    pub abbrev: usize,
    /// `-z`.
    pub nul: bool,
    /// `--stat=<width>,<name-width>,<count>`.
    pub stat_width: Option<usize>,
    pub stat_name_width: Option<usize>,
    pub stat_graph_width: Option<usize>,
    /// Files listed by `--stat` before `...`.
    pub stat_count: Option<usize>,
    /// `core.bigFileThreshold`: larger files are binary and are not read for patches.
    pub big_file_threshold: u64,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::Myers,
            indent_heuristic: true,
            context: 3,
            inter_hunk: 0,
            no_prefix: false,
            mnemonic_prefix: false,
            src_prefix: None,
            dst_prefix: None,
            sides: (SideKind::Commit, SideKind::Commit),
            relative_config: false,
            relative: None,
            renames: None,
            abbrev: 7,
            nul: false,
            stat_width: None,
            stat_name_width: None,
            stat_graph_width: None,
            stat_count: None,
            big_file_threshold: 512 * 1024 * 1024,
        }
    }
}

impl DiffOptions {
    /// Applies one configuration entry. Returns whether the key is a diff setting this handles.
    pub fn apply_config(&mut self, key: &str, value: &str) -> Result<bool, ConfigError> {
        let bad = || ConfigError {
            key: key.to_string(),
            value: value.to_string(),
        };
        let boolean = || parse_bool(value).ok_or_else(bad);
        let number = || value.trim().parse::<usize>().map_err(|_| bad());
        match key.to_ascii_lowercase().as_str() {
            "diff.algorithm" => self.algorithm = value.parse().map_err(|_| bad())?,
            "diff.indentheuristic" => self.indent_heuristic = boolean()?,
            "diff.context" => self.context = number()?,
            "diff.interhunkcontext" => self.inter_hunk = number()?,
            "diff.noprefix" => self.no_prefix = boolean()?,
            "diff.mnemonicprefix" => self.mnemonic_prefix = boolean()?,
            "diff.relative" => self.relative_config = boolean()?,
            "diff.renamelimit" => {
                self.renames.get_or_insert_with(Default::default).limit = number()?;
            }
            "diff.renames" => {
                // `copies` and `copy` still detect renames; copy detection is not implemented.
                let on =
                    matches!(value.to_ascii_lowercase().as_str(), "copies" | "copy") || boolean()?;
                match (on, self.renames.is_some()) {
                    (true, false) => self.renames = Some(RenameOptions::default()),
                    (false, _) => self.renames = None,
                    _ => {}
                }
            }
            "core.bigfilethreshold" => {
                self.big_file_threshold = parse_size(value).ok_or_else(bad)?;
            }
            "core.abbrev" => {
                if !value.eq_ignore_ascii_case("auto") {
                    self.abbrev = number()?.clamp(4, 40);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// The `(old, new)` path prefixes: explicit ones, else none for `diff.noprefix`, else the
    /// mnemonic pair for index and work tree comparisons, else `a/` and `b/`.
    pub fn prefixes(&self) -> (String, String) {
        let mnemonic = if self.mnemonic_prefix {
            match self.sides {
                (SideKind::Index | SideKind::Commit | SideKind::Object, SideKind::Worktree)
                | (SideKind::Commit, SideKind::Index) => {
                    Some((self.sides.0.mnemonic(), self.sides.1.mnemonic()))
                }
                _ => None,
            }
        } else {
            None
        };
        let (a, b) = if self.no_prefix {
            ("", "")
        } else {
            mnemonic.unwrap_or(("a/", "b/"))
        };
        (
            self.src_prefix.clone().unwrap_or_else(|| a.to_string()),
            self.dst_prefix.clone().unwrap_or_else(|| b.to_string()),
        )
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" | "" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// A byte size with git's optional `k`, `m`, `g` suffix.
fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim().to_ascii_lowercase();
    let (digits, mul) = match v.chars().last()? {
        'k' => (&v[..v.len() - 1], 1u64 << 10),
        'm' => (&v[..v.len() - 1], 1 << 20),
        'g' => (&v[..v.len() - 1], 1 << 30),
        _ => (&v[..], 1),
    };
    digits.parse::<u64>().ok()?.checked_mul(mul)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_keys_are_applied_case_insensitively() {
        let mut o = DiffOptions::default();
        assert!(o.apply_config("diff.algorithm", "histogram").unwrap());
        assert!(o.apply_config("diff.indentHeuristic", "false").unwrap());
        assert!(o.apply_config("diff.context", "5").unwrap());
        assert!(o.apply_config("diff.interHunkContext", "2").unwrap());
        assert!(o.apply_config("core.bigFileThreshold", "1m").unwrap());
        assert!(!o.apply_config("user.name", "x").unwrap());
        assert_eq!(o.algorithm, Algorithm::Histogram);
        assert!(!o.indent_heuristic);
        assert_eq!((o.context, o.inter_hunk), (5, 2));
        assert_eq!(o.big_file_threshold, 1 << 20);
        assert!(o.apply_config("diff.context", "many").is_err());
        assert!(o.apply_config("diff.algorithm", "fast").is_err());
    }

    #[test]
    fn prefix_rules() {
        let mut o = DiffOptions::default();
        assert_eq!(o.prefixes(), ("a/".into(), "b/".into()));
        o.no_prefix = true;
        assert_eq!(o.prefixes(), (String::new(), String::new()));
        o.no_prefix = false;
        o.mnemonic_prefix = true;
        o.sides = (SideKind::Index, SideKind::Worktree);
        assert_eq!(o.prefixes(), ("i/".into(), "w/".into()));
        o.sides = (SideKind::Commit, SideKind::Commit);
        assert_eq!(o.prefixes(), ("a/".into(), "b/".into()));
        o.src_prefix = Some("x/".into());
        assert_eq!(o.prefixes(), ("x/".into(), "b/".into()));
    }
}
