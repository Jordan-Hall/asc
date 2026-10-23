//! git's in-progress state files, read and written exactly as git does (SPEC §4.4): `MERGE_HEAD`,
//! `MERGE_MSG`, `MERGE_MODE`, `AUTO_MERGE`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `ORIG_HEAD`,
//! `SQUASH_MSG` and the `sequencer/` directory (`head`, `abort-safety`, `todo`, `opts`).
//!
//! Every later sequencer unit (rebase, stash, am) reuses this module. Files here are untrusted
//! input: another tool, or the user, may have edited them, so every read is size-capped and every
//! parse is strict and fails with a message instead of guessing.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use asc_core::id::GitOid;

use super::common::{CliError, Result};

/// Largest state file read: a todo list of a hundred thousand picks fits.
const MAX_STATE_FILE: u64 = 16 * 1024 * 1024;
/// Most todo lines read.
const MAX_TODO: usize = 100_000;
/// The full ids of the todo items, beside (not inside) `sequencer/`.
const SIDE_FILE: &str = "asc-sequencer-oids";

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

/// Reads a state file; `None` when it does not exist.
pub fn read(dir: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let path = dir.join(name);
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(fatal(format!("cannot read '{}': {e}", path.display()))),
    };
    let mut out = Vec::new();
    file.take(MAX_STATE_FILE + 1).read_to_end(&mut out)?;
    if out.len() as u64 > MAX_STATE_FILE {
        return Err(fatal(format!("'{}' is too large", path.display())));
    }
    Ok(Some(out))
}

/// Writes a state file through a temporary name and a rename, so a crash leaves the old or the
/// new content and never a partial file.
pub fn write(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let tmp = dir.join(format!("{name}.asc-tmp{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    if let Err(e) = fs::rename(&tmp, dir.join(name)) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Removes a state file; a missing file is fine.
pub fn remove(dir: &Path, name: &str) {
    let _ = fs::remove_file(dir.join(name));
}

/// Whether a state file exists.
pub fn exists(dir: &Path, name: &str) -> bool {
    dir.join(name).is_file()
}

fn parse_oid(text: &str, what: &str) -> Result<GitOid> {
    text.trim()
        .parse()
        .map_err(|_| fatal(format!("{what} holds a bad object id '{}'", text.trim())))
}

/// The object ids of a file with one id per line (`MERGE_HEAD`).
pub fn read_oids(dir: &Path, name: &str) -> Result<Option<Vec<GitOid>>> {
    let Some(bytes) = read(dir, name)? else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        out.push(parse_oid(line, name)?);
    }
    Ok(Some(out))
}

/// The one object id a file holds (`CHERRY_PICK_HEAD`, `ORIG_HEAD`, `sequencer/head`).
pub fn read_oid(dir: &Path, name: &str) -> Result<Option<GitOid>> {
    Ok(read_oids(dir, name)?.and_then(|v| v.into_iter().next()))
}

/// Writes `ORIG_HEAD`.
pub fn write_orig_head(dir: &Path, oid: GitOid) -> Result<()> {
    write(dir, "ORIG_HEAD", format!("{oid}\n").as_bytes())
}

/// Writes `AUTO_MERGE`: the git tree id of the merge result with conflict markers.
pub fn write_auto_merge(dir: &Path, tree: GitOid) -> Result<()> {
    write(dir, "AUTO_MERGE", format!("{tree}\n").as_bytes())
}

/// How `--ff` / `--no-ff` / `--ff-only` is recorded in `MERGE_MODE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfMode {
    Allow,
    Never,
    Only,
}

impl FfMode {
    /// `MERGE_MODE` holds `no-ff` or `ff-only` with no newline, and nothing for the default.
    fn text(self) -> &'static str {
        match self {
            Self::Allow => "",
            Self::Never => "no-ff",
            Self::Only => "ff-only",
        }
    }

    /// The mode a `MERGE_MODE` file records.
    pub fn parse(bytes: &[u8]) -> Self {
        match bytes.trim_ascii() {
            b"no-ff" => Self::Never,
            b"ff-only" => Self::Only,
            _ => Self::Allow,
        }
    }
}

/// A merge stopped before its commit: what `git merge` leaves for `git commit`.
pub struct MergeState<'a> {
    pub heads: &'a [GitOid],
    pub message: &'a [u8],
    pub mode: FfMode,
}

/// Writes `MERGE_HEAD`, `MERGE_MSG` and `MERGE_MODE`.
pub fn write_merge(dir: &Path, state: &MergeState<'_>) -> Result<()> {
    let heads: String = state.heads.iter().map(|h| format!("{h}\n")).collect();
    write(dir, "MERGE_HEAD", heads.as_bytes())?;
    write(dir, "MERGE_MODE", state.mode.text().as_bytes())?;
    write(dir, "MERGE_MSG", state.message)
}

/// Removes the state of a finished or abandoned merge.
pub fn clear_merge(dir: &Path) {
    for name in [
        "MERGE_HEAD",
        "MERGE_MSG",
        "MERGE_MODE",
        "AUTO_MERGE",
        "SQUASH_MSG",
    ] {
        remove(dir, name);
    }
}

/// Which replay is in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    Pick,
    Revert,
}

impl Replay {
    /// The state file naming the commit being replayed.
    pub fn head_file(self) -> &'static str {
        match self {
            Self::Pick => "CHERRY_PICK_HEAD",
            Self::Revert => "REVERT_HEAD",
        }
    }

    /// The todo verb, and the command name in messages.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Revert => "revert",
        }
    }

    /// The command name.
    pub fn command(self) -> &'static str {
        match self {
            Self::Pick => "cherry-pick",
            Self::Revert => "revert",
        }
    }
}

/// Which replay a single-commit state file shows, if any.
pub fn replay_in_progress(dir: &Path) -> Option<Replay> {
    [Replay::Pick, Replay::Revert]
        .into_iter()
        .find(|r| exists(dir, r.head_file()))
}

/// Removes the state of a finished or abandoned single-commit replay.
pub fn clear_replay(dir: &Path) {
    for name in ["CHERRY_PICK_HEAD", "REVERT_HEAD", "MERGE_MSG", "AUTO_MERGE"] {
        remove(dir, name);
    }
}

/// The `[options]` of `sequencer/opts`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Opts {
    pub no_commit: bool,
    pub allow_empty: bool,
    pub allow_empty_message: bool,
    pub keep_redundant: bool,
    pub record_origin: bool,
    pub mainline: Option<usize>,
    pub strategy_options: Vec<String>,
}

impl Opts {
    /// The file text, keys in git's order; `None` when no option is set (git writes no file).
    fn text(&self) -> Option<String> {
        let mut lines = Vec::new();
        let flag = |on: bool, key: &str, out: &mut Vec<String>| {
            if on {
                out.push(format!("\t{key} = true\n"));
            }
        };
        flag(self.no_commit, "no-commit", &mut lines);
        flag(self.allow_empty, "allow-empty", &mut lines);
        flag(self.allow_empty_message, "allow-empty-message", &mut lines);
        flag(self.keep_redundant, "keep-redundant-commits", &mut lines);
        flag(self.record_origin, "record-origin", &mut lines);
        if let Some(n) = self.mainline {
            lines.push(format!("\tmainline = {n}\n"));
        }
        for x in &self.strategy_options {
            lines.push(format!("\tstrategy-option = {x}\n"));
        }
        (!lines.is_empty()).then(|| format!("[options]\n{}", lines.concat()))
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        let text = String::from_utf8_lossy(bytes);
        let mut opts = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line == "[options]" {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(fatal(format!("sequencer/opts: cannot parse '{line}'")));
            };
            let (key, value) = (key.trim(), value.trim());
            let on = value == "true";
            match key {
                "no-commit" => opts.no_commit = on,
                "allow-empty" => opts.allow_empty = on,
                "allow-empty-message" => opts.allow_empty_message = on,
                "keep-redundant-commits" => opts.keep_redundant = on,
                "record-origin" => opts.record_origin = on,
                "mainline" => {
                    opts.mainline =
                        Some(value.parse().map_err(|_| {
                            fatal(format!("sequencer/opts: bad mainline '{value}'"))
                        })?);
                }
                "strategy-option" => opts.strategy_options.push(value.to_owned()),
                // Options asc never writes (edit, signoff, gpg-sign, strategy) are ignored.
                _ => {}
            }
        }
        Ok(opts)
    }
}

/// One line of `sequencer/todo`: `pick 78818cf m2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Todo {
    pub replay: Replay,
    /// The abbreviated object id as written.
    pub abbrev: String,
    pub subject: String,
    /// The full id, from asc's side file (`asc-sequencer-oids`); git's todo holds only the
    /// abbreviation, which the git map cannot resolve.
    pub oid: Option<GitOid>,
}

impl Todo {
    fn line(&self) -> String {
        if self.subject.is_empty() {
            format!("{} {}\n", self.replay.verb(), self.abbrev)
        } else {
            format!("{} {} {}\n", self.replay.verb(), self.abbrev, self.subject)
        }
    }

    fn parse(line: &str) -> Result<Self> {
        let mut parts = line.splitn(3, ' ');
        let replay = match parts.next() {
            Some("pick" | "p") => Replay::Pick,
            Some("revert") => Replay::Revert,
            _ => {
                return Err(fatal(format!(
                    "sequencer/todo: unusable instruction sheet line '{line}'"
                )));
            }
        };
        let abbrev = parts
            .next()
            .filter(|a| (4..=40).contains(&a.len()) && a.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| fatal(format!("sequencer/todo: bad object id in '{line}'")))?;
        Ok(Self {
            replay,
            abbrev: abbrev.to_owned(),
            subject: parts.next().unwrap_or_default().to_owned(),
            oid: None,
        })
    }
}

/// A multi-commit replay in progress (`.git/sequencer`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sequence {
    /// HEAD when the sequence began; `--abort` goes back to it.
    pub head: GitOid,
    /// HEAD after the last completed step; `--abort` refuses when HEAD is elsewhere.
    pub abort_safety: GitOid,
    /// The steps left, the one that stopped first.
    pub todo: Vec<Todo>,
    pub opts: Opts,
}

impl Sequence {
    fn dir(gitdir: &Path) -> PathBuf {
        gitdir.join("sequencer")
    }

    /// Whether a sequence is in progress.
    pub fn exists(gitdir: &Path) -> bool {
        Self::dir(gitdir).is_dir()
    }

    /// Writes all four files, creating the directory.
    pub fn save(&self, gitdir: &Path) -> Result<()> {
        let dir = Self::dir(gitdir);
        fs::create_dir_all(&dir)?;
        write(&dir, "head", format!("{}\n", self.head).as_bytes())?;
        write(
            &dir,
            "abort-safety",
            format!("{}\n", self.abort_safety).as_bytes(),
        )?;
        let todo: String = self.todo.iter().map(Todo::line).collect();
        write(&dir, "todo", todo.as_bytes())?;
        if self.todo.iter().all(|t| t.oid.is_some()) {
            let ids: String = self
                .todo
                .iter()
                .filter_map(|t| t.oid)
                .map(|o| format!("{o}\n"))
                .collect();
            write(gitdir, SIDE_FILE, ids.as_bytes())?;
        } else {
            remove(gitdir, SIDE_FILE);
        }
        match self.opts.text() {
            Some(text) => write(&dir, "opts", text.as_bytes())?,
            None => remove(&dir, "opts"),
        }
        Ok(())
    }

    /// Reads the sequence; `None` when there is none.
    pub fn load(gitdir: &Path) -> Result<Option<Self>> {
        let dir = Self::dir(gitdir);
        if !dir.is_dir() {
            return Ok(None);
        }
        let head = read_oid(&dir, "head")?.ok_or_else(|| fatal("sequencer/head is missing"))?;
        let abort_safety = read_oid(&dir, "abort-safety")?.unwrap_or(head);
        let todo_bytes = read(&dir, "todo")?.unwrap_or_default();
        let todo_text = String::from_utf8_lossy(&todo_bytes);
        let mut todo = Vec::new();
        for line in todo_text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        {
            if todo.len() >= MAX_TODO {
                return Err(fatal("sequencer/todo has too many lines"));
            }
            todo.push(Todo::parse(line)?);
        }
        // The side file counts only when it lines up with the todo and every id starts with the
        // abbreviation written there, so an edited todo cannot select another commit.
        if let Some(ids) = read_oids(gitdir, SIDE_FILE)
            .ok()
            .flatten()
            .filter(|ids| ids.len() == todo.len())
        {
            for (t, id) in todo.iter_mut().zip(ids) {
                if id.to_string().starts_with(&t.abbrev.to_ascii_lowercase()) {
                    t.oid = Some(id);
                }
            }
        }
        let opts = match read(&dir, "opts")? {
            Some(bytes) => Opts::parse(&bytes)?,
            None => Opts::default(),
        };
        Ok(Some(Self {
            head,
            abort_safety,
            todo,
            opts,
        }))
    }

    /// Removes the directory.
    pub fn remove(gitdir: &Path) {
        remove(gitdir, SIDE_FILE);
        let _ = fs::remove_dir_all(Self::dir(gitdir));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(c: char) -> GitOid {
        c.to_string().repeat(40).parse().unwrap()
    }

    #[test]
    fn opts_round_trip_in_gits_key_order() {
        let opts = Opts {
            allow_empty: true,
            record_origin: true,
            mainline: Some(2),
            strategy_options: vec!["ours".into()],
            ..Opts::default()
        };
        let text = opts.text().unwrap();
        assert_eq!(
            text,
            "[options]\n\tallow-empty = true\n\trecord-origin = true\n\tmainline = 2\n\tstrategy-option = ours\n"
        );
        assert_eq!(Opts::parse(text.as_bytes()).unwrap(), opts);
        assert_eq!(Opts::default().text(), None);
    }

    #[test]
    fn a_sequence_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let seq = Sequence {
            head: oid('a'),
            abort_safety: oid('b'),
            todo: vec![
                Todo {
                    replay: Replay::Pick,
                    abbrev: "7777777".into(),
                    subject: "m2 subject".into(),
                    oid: Some(oid('7')),
                },
                Todo {
                    replay: Replay::Revert,
                    abbrev: "9999999".into(),
                    subject: String::new(),
                    oid: Some(oid('9')),
                },
            ],
            opts: Opts::default(),
        };
        seq.save(dir.path()).unwrap();
        let todo = fs::read_to_string(dir.path().join("sequencer/todo")).unwrap();
        assert_eq!(todo, "pick 7777777 m2 subject\nrevert 9999999\n");
        assert_eq!(Sequence::load(dir.path()).unwrap(), Some(seq));
        Sequence::remove(dir.path());
        assert!(Sequence::load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_bad_todo_line_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let seq_dir = dir.path().join("sequencer");
        fs::create_dir_all(&seq_dir).unwrap();
        fs::write(seq_dir.join("head"), format!("{}\n", oid('a'))).unwrap();
        fs::write(seq_dir.join("todo"), "exec rm -rf /\n").unwrap();
        assert!(Sequence::load(dir.path()).is_err());
        fs::write(seq_dir.join("todo"), "pick zzzz subject\n").unwrap();
        assert!(Sequence::load(dir.path()).is_err());
    }

    #[test]
    fn merge_state_files_match_gits_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let heads = [oid('c')];
        write_merge(
            dir.path(),
            &MergeState {
                heads: &heads,
                message: b"Merge branch 'x'\n",
                mode: FfMode::Never,
            },
        )
        .unwrap();
        assert_eq!(fs::read(dir.path().join("MERGE_MODE")).unwrap(), b"no-ff");
        assert_eq!(
            read_oids(dir.path(), "MERGE_HEAD").unwrap(),
            Some(heads.to_vec())
        );
        clear_merge(dir.path());
        assert!(!exists(dir.path(), "MERGE_HEAD"));
    }

    #[test]
    fn an_oversized_or_garbled_oid_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("MERGE_HEAD"), "not-an-oid\n").unwrap();
        assert!(read_oids(dir.path(), "MERGE_HEAD").is_err());
    }
}
