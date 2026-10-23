//! `check-attr` (SPEC §2.4.1, §11.5): the attributes of paths, from, lowest precedence first,
//! `core.attributesFile`, each directory's `.ascattributes` else `.gitattributes` (root to leaf)
//! and `<common>/info/attributes`, read with `gix-attributes`. Attribute files in the work tree
//! are read through `asc_wc::SafeRoot`, so a file behind a link is never read.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use bstr::ByteSlice;
use asc_core::path::RepoPath;
use asc_wc::SafeRoot;
use gix_attributes::glob::pattern::Case;
use gix_attributes::search::{MetadataCollection, Outcome};
use gix_attributes::{Search, StateRef};

use super::add::cwd_prefix;
use super::add::spec::repo_path;
use super::check_ignore::{os_bytes, read_stdin, stdin_records, write_quoted};
use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "check-attr", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "check-attr",
    flags: &["-a", "--all", "-z", "--stdin"],
    placeholders: &[],
}];

/// Largest attributes file read; a larger one is refused (git's own limit is 100 MiB).
const MAX_ATTR_FILE: u64 = 1 << 20;

/// Most paths one invocation answers.
const MAX_PATHS: usize = 1_000_000;

const USAGE: &str = "usage: git check-attr [--source <tree-ish>] [-a | --all | <attr>...] [--] <pathname>...\n   or: git check-attr --stdin [-z] [--source <tree-ish>] [-a | --all | <attr>...]\n\n    -a, --[no-]all        report all attributes set on file\n    --[no-]cached         use .gitattributes only from the index\n    --[no-]stdin          read file names from stdin\n    -z                    terminate input and output records by a NUL character\n    --[no-]source <tree-ish>\n                          which tree-ish to check attributes at";

fn usage_error(message: &str) -> CliError {
    CliError::Usage(format!("error: {message}\n{USAGE}"))
}

/// The value of one attribute for one path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Set,
    Unset,
    Unspecified,
    Value(Vec<u8>),
}

impl State {
    fn of(state: StateRef<'_>) -> Self {
        match state {
            StateRef::Set => Self::Set,
            StateRef::Unset => Self::Unset,
            StateRef::Unspecified => Self::Unspecified,
            StateRef::Value(v) => Self::Value(v.as_bstr().to_vec()),
        }
    }

    fn text(&self) -> &[u8] {
        match self {
            Self::Set => b"set",
            Self::Unset => b"unset",
            Self::Unspecified => b"unspecified",
            Self::Value(v) => v,
        }
    }
}

/// Per-directory attribute searches, built on demand and cached.
struct AttrSearch {
    root: PathBuf,
    gate: SafeRoot,
    info: Option<PathBuf>,
    collection: MetadataCollection,
    outcome: Outcome,
    /// The search for each directory (`""` is the root), `info/attributes` last.
    dirs: HashMap<Vec<u8>, Rc<Search>>,
    base: Search,
    buf: Vec<u8>,
}

fn fatal_io(path: &Path, e: &io::Error) -> CliError {
    CliError::Fatal(format!("{}: {e}", path.display()))
}

/// `path` when it is a regular file small enough to read; a larger one is refused.
fn sized(path: PathBuf) -> Result<Option<PathBuf>> {
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => {
            if m.len() > MAX_ATTR_FILE {
                return Err(CliError::Fatal(format!(
                    "asc: {} is larger than {} MiB",
                    path.display(),
                    MAX_ATTR_FILE >> 20
                )));
            }
            Ok(Some(path))
        }
        _ => Ok(None),
    }
}

impl AttrSearch {
    fn load(root: &Path, common_dir: &Path, global: Option<PathBuf>) -> Result<Self> {
        let global = match global {
            Some(g) => sized(g)?,
            None => None,
        };
        let mut collection = MetadataCollection::default();
        let mut buf = Vec::new();
        let base = Search::new_globals(global, &mut buf, &mut collection).map_err(|e| fatal_io(root, &e))?;
        let info = sized(common_dir.join("info").join("attributes"))?;
        Ok(Self {
            root: root.to_owned(),
            gate: SafeRoot::new(root),
            info,
            collection,
            outcome: Outcome::default(),
            dirs: HashMap::new(),
            base,
            buf,
        })
    }

    /// The attribute file of directory `dir` (`""` is the root): `.ascattributes` else
    /// `.gitattributes`, never through a link.
    fn dir_file(&self, dir: &[u8]) -> Result<Option<PathBuf>> {
        for name in [".ascattributes", ".gitattributes"] {
            let rel = if dir.is_empty() { name.as_bytes().to_vec() } else { [dir, b"/", name.as_bytes()].concat() };
            let Some(abs) = RepoPath::new(rel).ok().and_then(|rp| self.gate.resolve(&rp)) else {
                return Ok(None);
            };
            if let Some(found) = sized(abs)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    fn search(&mut self, dir: &[u8]) -> Result<Rc<Search>> {
        if let Some(found) = self.dirs.get(dir) {
            return Ok(found.clone());
        }
        let mut search = if dir.is_empty() {
            self.base.clone()
        } else {
            let parent = dir.iter().rposition(|&c| c == b'/').map_or(&[][..], |i| &dir[..i]);
            let mut inherited = (*self.search(parent)?).clone();
            if self.info.is_some() {
                inherited.pop_pattern_list(); // `info/attributes` goes last again below
            }
            inherited
        };
        if let Some(file) = self.dir_file(dir)? {
            search
                .add_patterns_file(file.clone(), false, Some(&self.root), &mut self.buf, &mut self.collection, true)
                .map_err(|e| fatal_io(&file, &e))?;
        }
        if let Some(info) = &self.info {
            search
                .add_patterns_file(info.clone(), false, None, &mut self.buf, &mut self.collection, true)
                .map_err(|e| fatal_io(info, &e))?;
        }
        let search = Rc::new(search);
        self.dirs.insert(dir.to_vec(), search.clone());
        Ok(search)
    }

    /// Matches `path` (`is_dir` when it was typed with a trailing slash) and leaves the answer
    /// in the outcome.
    fn matched(&mut self, path: &[u8], selection: Option<&[String]>) -> Result<()> {
        let trimmed = path.strip_suffix(b"/").unwrap_or(path);
        let dir = trimmed.iter().rposition(|&c| c == b'/').map_or(&[][..], |i| &trimmed[..i]);
        let search = self.search(dir)?;
        match selection {
            Some(names) => self.outcome.initialize_with_selection(&self.collection, names),
            None => self.outcome.initialize(&self.collection),
        }
        search.pattern_matching_relative_path(
            trimmed.as_bstr(),
            Case::Sensitive,
            Some(path.ends_with(b"/")),
            &mut self.outcome,
        );
        Ok(())
    }

    /// The state of each of `names`, in the order asked.
    fn named(&mut self, path: &[u8], names: &[String]) -> Result<Vec<(String, State)>> {
        self.matched(path, Some(names))?;
        Ok(self
            .outcome
            .iter_selected()
            .map(|m| (m.assignment.name.as_str().to_owned(), State::of(m.assignment.state)))
            .collect())
    }

    /// Every attribute set, unset or valued on `path`.
    fn all(&mut self, path: &[u8]) -> Result<Vec<(String, State)>> {
        self.matched(path, None)?;
        Ok(self
            .outcome
            .iter()
            .map(|m| (m.assignment.name.as_str().to_owned(), State::of(m.assignment.state)))
            .filter(|(_, s)| *s != State::Unspecified)
            .collect())
    }
}

/// git's `attr_name_valid`.
fn name_valid(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
}

#[derive(Default)]
struct Opts {
    all: bool,
    nul: bool,
    stdin: bool,
    names: Vec<String>,
    paths: Vec<OsString>,
}

fn parse(rest: &[OsString]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut positional: Vec<OsString> = Vec::new();
    let mut ended = false;
    for arg in rest {
        let text = arg.to_string_lossy();
        if ended || !text.starts_with('-') || text == "-" {
            positional.push(arg.clone());
            continue;
        }
        if text == "--" {
            ended = true;
            positional.push(arg.clone());
            continue;
        }
        if let Some(long) = text.strip_prefix("--") {
            match long {
                "all" => o.all = true,
                "no-all" => o.all = false,
                "stdin" => o.stdin = true,
                "no-stdin" => o.stdin = false,
                other => return Err(usage_error(&format!("unknown option `{other}'"))),
            }
            continue;
        }
        for c in text[1..].chars() {
            match c {
                'a' => o.all = true,
                'z' => o.nul = true,
                other => return Err(usage_error(&format!("unknown switch `{other}'"))),
            }
        }
    }
    let dashes = positional.iter().position(|a| a == OsStr::new("--"));
    let split = if o.all {
        if dashes.is_some_and(|d| d >= 1) {
            return Err(usage_error("Attributes and --all both specified"));
        }
        dashes.map_or(0, |d| d + 1)
    } else {
        match dashes {
            Some(0) => return Err(usage_error("No attribute specified")),
            Some(d) => d,
            None if positional.is_empty() => return Err(usage_error("No attribute specified")),
            None if o.stdin => positional.len(),
            None => 1,
        }
    };
    let (names, files) = if o.all { (&[][..], &positional[split..]) } else { (&positional[..split], &positional[split..]) };
    for name in names {
        let text = name.to_string_lossy().into_owned();
        if !name_valid(&text) {
            return Err(CliError::Fatal(format!("{text} is not a valid attribute name")));
        }
        o.names.push(text);
    }
    let files: Vec<OsString> = match files.first() {
        Some(first) if first == OsStr::new("--") && dashes.is_some() => files[1..].to_vec(),
        _ => files.to_vec(),
    };
    if o.stdin {
        if !files.is_empty() {
            return Err(usage_error("Can't specify files with --stdin"));
        }
    } else if files.is_empty() {
        return Err(usage_error("No file specified"));
    }
    o.paths = files;
    Ok(o)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = parse(&a.rest)?;
    let ws = ctx.ws()?;
    let root = ws.projection.toplevel();
    let prefix = cwd_prefix(ctx, ws);
    let quote_high = ctx.config.bool("core.quotePath").unwrap_or(true);
    let global = ctx.config.path("core.attributesFile").or_else(|| {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|h| h.join(".config")))
            .map(|c| c.join("git").join("attributes"))
    });
    let mut search = AttrSearch::load(root, ws.projection.common_dir(), global)?;
    let inputs: Vec<Vec<u8>> = if o.stdin {
        stdin_records(&read_stdin()?, o.nul)?
    } else {
        o.paths.iter().map(|p| os_bytes(p)).collect()
    };
    if inputs.len() > MAX_PATHS {
        return Err(CliError::Fatal(format!("asc: more than {MAX_PATHS} paths given")));
    }
    let mut out = io::BufWriter::new(io::stdout().lock());
    for arg in &inputs {
        let text = String::from_utf8_lossy(arg).into_owned();
        let rel = repo_path(OsStr::new(&text), &prefix, root)?;
        let mut keyed = rel.clone();
        if arg.ends_with(b"/") {
            keyed.push(b'/');
        }
        let rows = if o.all { search.all(&keyed)? } else { search.named(&keyed, &o.names)? };
        let written = write_rows(&mut out, &o, arg, &rows, quote_high);
        match written {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(ExitCode::SUCCESS),
            Err(e) => return Err(e.into()),
        }
    }
    match out.flush() {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(ExitCode::SUCCESS),
    }
}

fn write_rows(out: &mut dyn Write, o: &Opts, path: &[u8], rows: &[(String, State)], quote_high: bool) -> io::Result<()> {
    for (name, state) in rows {
        if o.nul {
            out.write_all(path)?;
            out.write_all(&[0])?;
            out.write_all(name.as_bytes())?;
            out.write_all(&[0])?;
            out.write_all(state.text())?;
            out.write_all(&[0])?;
        } else {
            write_quoted(out, path, quote_high)?;
            out.write_all(b": ")?;
            out.write_all(name.as_bytes())?;
            out.write_all(b": ")?;
            out.write_all(state.text())?;
            out.write_all(b"\n")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn attribute_and_file_split_follows_git() {
        let o = parse(&args(&["text", "a.txt"])).unwrap();
        assert_eq!((o.names, o.paths), (vec!["text".to_owned()], vec![OsString::from("a.txt")]));
        let o = parse(&args(&["text", "eol", "--", "a", "b"])).unwrap();
        assert_eq!(o.names, ["text", "eol"]);
        assert_eq!(o.paths.len(), 2);
        let o = parse(&args(&["-a", "x", "y"])).unwrap();
        assert!(o.all && o.names.is_empty() && o.paths.len() == 2);
        let o = parse(&args(&["--stdin", "text", "diff"])).unwrap();
        assert_eq!(o.names, ["text", "diff"]);
    }

    #[test]
    fn misuse_is_a_usage_error() {
        for bad in [&["--"][..], &[][..], &["text"][..], &["-a", "text", "--", "f"][..], &["--stdin", "text", "--", "f"][..]] {
            assert!(matches!(parse(&args(bad)), Err(CliError::Usage(_))), "{bad:?}");
        }
        assert!(matches!(parse(&args(&["-bad-", "--", "f"])), Err(CliError::Usage(_))));
        assert!(matches!(parse(&args(&["-x", "f"])), Err(CliError::Usage(_))));
        assert!(matches!(parse(&args(&["a b", "--", "f"])), Err(CliError::Fatal(_))));
    }
}
