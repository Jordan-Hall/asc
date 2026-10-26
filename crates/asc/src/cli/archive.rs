//! `archive [--format=tar|zip] [-o <file>] [--prefix=<dir>/] [-v] [-l] <tree-ish> [<path>…]`
//! (SPEC §11.2): writes a commit's tree as a tar or zip archive, locally.
//!
//! Attributes come from the archived revision, never the work tree (a per-directory
//! `.ascattributes`, else `.gitattributes`, root to leaf, SPEC §2.4.1): `export-ignore` leaves a
//! path (and everything under an ignored directory) out, and `export-subst` expands
//! `$Format:<fmt>$` in the file with the commit's fields (`%H %h %T %t %P %p %s %b %B %an %ae
//! %at %cn %ce %ct %n %%`), as `git archive` does. The server's `Archive` RPC does the same
//! over the wire.
//!
//! Work and memory are bounded: file content is verified and streamed (a chunked file is read
//! chunk by chunk; only an `export-subst` file, at most 1 MiB, is held), attribute files are
//! read to 1 MiB, an expansion is capped at 4 MiB, and a symlink target is limited to 4096
//! bytes. The tar writer is built in (ustar, with pax records for long names and sizes); `.tar.gz`
//! output is not offered. A mode other than 0644/0755 is never written; every entry is dated
//! with the commit's committer time.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use asc_core::codec::decode_envelope;
use asc_core::content::{CHUNK_THRESHOLD, ChunkAssembler};
use asc_core::git::tree_oid;
use asc_core::id::{FileId, GitOid, TreeId};
use asc_core::object::{ChunkList, Kind};
use asc_core::path::RepoPath;
use asc_core::sig::Signature;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use bytes::Bytes;
use futures::TryStreamExt;

use super::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use super::refs_plumbing::args::{Cursor, Word};
use super::restore::{Spec, resolve_commit};
use crate::native::NativeSpec;

const USAGE: &str = "usage: git archive [<options>] <tree-ish> [<path>...]

    --format <fmt>        archive format (tar or zip)
    --prefix <prefix>     prepend prefix to each pathname in the archive
    -o, --output <file>   write the archive to this file
    -l, --list            list supported archive formats
    -v, --verbose         report archived files on stderr
";

/// Largest attributes file read.
const ATTR_MAX_FILE: u64 = 1 << 20;
/// Largest file an `export-subst` expansion holds.
const SUBST_MAX_FILE: u64 = 1 << 20;
/// Largest expanded `export-subst` file.
const SUBST_MAX_OUT: usize = 4 << 20;
/// Largest symlink target archived.
const SYMLINK_MAX: u64 = 4096;
/// Longest `--prefix`.
const MAX_PREFIX: usize = 4096;

#[derive(clap::Parser, Debug)]
#[command(
    name = "archive",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "archive",
    flags: &[
        "--format",
        "--output",
        "-o",
        "--prefix",
        "--list",
        "-l",
        "--verbose",
        "-v",
    ],
    placeholders: &[],
}];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Tar,
    Zip,
}

#[derive(Debug, PartialEq, Eq)]
struct Opts {
    format: Option<Format>,
    output: Option<String>,
    prefix: String,
    list: bool,
    verbose: bool,
    rest: Vec<OsString>,
}

fn parse_format(text: &str) -> Result<Format> {
    match text {
        "tar" => Ok(Format::Tar),
        "zip" => Ok(Format::Zip),
        other => Err(CliError::Fatal(format!(
            "Unknown archive format '{other}' (asc writes tar and zip)"
        ))),
    }
}

/// The format a `-o` file name implies, as git infers it. A compressed tar is refused rather
/// than written uncompressed under a name that promises compression.
fn format_of_name(name: &str) -> Result<Option<Format>> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return Err(CliError::Fatal(
            "asc: compressed tar output is not offered; write a .tar or a .zip".into(),
        ));
    }
    Ok(if lower.ends_with(".zip") {
        Some(Format::Zip)
    } else if lower.ends_with(".tar") {
        Some(Format::Tar)
    } else {
        None
    })
}

fn parse(rest: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(rest, USAGE);
    let mut o = Opts {
        format: None,
        output: None,
        prefix: String::new(),
        list: false,
        verbose: false,
        rest: Vec::new(),
    };
    while let Some(word) = cur.next() {
        match word {
            Word::End => {
                o.rest.extend(cur.rest().into_iter().map(OsString::from));
            }
            Word::Operand(op) => {
                o.rest.push(OsString::from(op));
                // The tree-ish is the first operand; git stops option parsing there only for
                // paths, so options may still follow, and everything else is a path.
            }
            Word::Short(flags) => {
                let mut chars = flags.chars();
                while let Some(c) = chars.next() {
                    match c {
                        'v' => o.verbose = true,
                        'l' => o.list = true,
                        'o' => {
                            let attached: String = chars.by_ref().collect();
                            let inline = (!attached.is_empty()).then_some(attached);
                            o.output = Some(cur.value("o", inline)?);
                        }
                        other => {
                            return Err(cur.usage_error(&format!("unknown switch `{other}'")));
                        }
                    }
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "format" => o.format = Some(parse_format(&cur.value("format", inline)?)?),
                "output" => o.output = Some(cur.value("output", inline)?),
                "prefix" => {
                    let p = cur.value("prefix", inline)?;
                    if p.len() > MAX_PREFIX {
                        return Err(CliError::Fatal("asc: the prefix is too long".into()));
                    }
                    o.prefix = p;
                }
                "list" => o.list = true,
                "verbose" => o.verbose = true,
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

/// A validated archive prefix: empty, or `a/b/` with every component a legal path name.
fn archive_prefix(p: &str) -> Result<Vec<u8>> {
    if p.is_empty() {
        return Ok(Vec::new());
    }
    let trimmed = p.strip_suffix('/').unwrap_or(p);
    if RepoPath::new(trimmed.as_bytes().to_vec()).is_err() {
        return Err(CliError::Fatal(format!(
            "asc: '{p}' is not a usable prefix"
        )));
    }
    Ok(format!("{trimmed}/").into_bytes())
}

// ------------------------------------------------------------------ attributes

/// One attribute's state for a path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Attr {
    Set,
    Other,
}

/// Attribute lookup at one revision (SPEC §2.4.1): the nearest `.ascattributes`, else
/// `.gitattributes`, per directory from the root, never merged with a sibling. An unreadable,
/// linked or oversized file reads as absent.
struct TreeAttrs<'a> {
    engine: &'a CliEngine,
    root: TreeId,
    collection: gix_attributes::search::MetadataCollection,
    outcome: gix_attributes::search::Outcome,
    base: gix_attributes::Search,
    dirs: HashMap<Vec<u8>, std::sync::Arc<gix_attributes::Search>>,
    any: bool,
    ignored: HashMap<Vec<u8>, bool>,
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn attr_err(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: attributes: {e}"))
}

impl<'a> TreeAttrs<'a> {
    fn new(engine: &'a CliEngine, root: TreeId) -> Result<Self> {
        let mut collection = gix_attributes::search::MetadataCollection::default();
        let mut buf = Vec::new();
        let base = gix_attributes::Search::new_globals(
            None::<std::path::PathBuf>,
            &mut buf,
            &mut collection,
        )
        .map_err(attr_err)?;
        Ok(Self {
            engine,
            root,
            collection,
            outcome: gix_attributes::search::Outcome::default(),
            base,
            dirs: HashMap::new(),
            any: false,
            ignored: HashMap::new(),
        })
    }

    async fn read_attr_file(&self, dir: &[u8], name: &str) -> Result<Option<Bytes>> {
        let path = if dir.is_empty() {
            name.as_bytes().to_vec()
        } else {
            [dir, b"/", name.as_bytes()].concat()
        };
        let Ok(path) = RepoPath::new(path) else {
            return Ok(None);
        };
        let Some(e) = self.engine.lookup(self.root, &path).await? else {
            return Ok(None);
        };
        if !matches!(e.kind, Kind::File | Kind::Exec) || e.size > ATTR_MAX_FILE {
            return Ok(None);
        }
        Ok(self
            .engine
            .read_file(FileId::from_bytes(e.id), e.size)
            .await
            .ok())
    }

    /// The search that applies to paths directly inside `dir` (`""` is the root).
    async fn search(&mut self, dir: &[u8]) -> Result<std::sync::Arc<gix_attributes::Search>> {
        if let Some(s) = self.dirs.get(dir) {
            return Ok(s.clone());
        }
        let mut prefixes: Vec<&[u8]> = vec![&dir[..0]];
        prefixes.extend(
            dir.iter()
                .enumerate()
                .filter(|(_, c)| **c == b'/')
                .map(|(i, _)| &dir[..i]),
        );
        if !dir.is_empty() {
            prefixes.push(dir);
        }
        let mut cur: Option<std::sync::Arc<gix_attributes::Search>> = None;
        for d in prefixes {
            if let Some(s) = self.dirs.get(d) {
                cur = Some(s.clone());
                continue;
            }
            let mut search = match &cur {
                Some(p) => (**p).clone(),
                None => self.base.clone(),
            };
            let file = match self.read_attr_file(d, ".ascattributes").await? {
                Some(f) => Some(f),
                None => self.read_attr_file(d, ".gitattributes").await?,
            };
            if let Some(f) = file {
                self.any = true;
                let source = if d.is_empty() {
                    PathBuf::from(".gitattributes")
                } else {
                    PathBuf::from(format!("{}/.gitattributes", lossy(d)))
                };
                search
                    .add_patterns_buffer(
                        &f,
                        source,
                        Some(std::path::Path::new("")),
                        &mut self.collection,
                        true,
                    )
                    .map_err(attr_err)?;
            }
            let search = std::sync::Arc::new(search);
            if self.dirs.len() >= 4096 {
                self.dirs.clear();
            }
            self.dirs.insert(d.to_vec(), search.clone());
            cur = Some(search);
        }
        cur.ok_or_else(|| attr_err("no search built"))
    }

    /// Whether attribute `name` is set for `path`.
    async fn is_set(&mut self, path: &[u8], is_dir: bool, name: &str) -> Result<bool> {
        let dir = path
            .iter()
            .rposition(|&c| c == b'/')
            .map_or(&[][..], |i| &path[..i]);
        let search = self.search(dir).await?;
        if !self.any {
            return Ok(false);
        }
        use bstr::ByteSlice;
        use gix_attributes::StateRef;
        self.outcome
            .initialize_with_selection(&self.collection, [name]);
        search.pattern_matching_relative_path(
            path.as_bstr(),
            gix_attributes::glob::pattern::Case::Sensitive,
            Some(is_dir),
            &mut self.outcome,
        );
        let mut state = Attr::Other;
        for m in self.outcome.iter_selected() {
            if m.assignment.name.as_str() == name {
                state = if matches!(m.assignment.state, StateRef::Set) {
                    Attr::Set
                } else {
                    Attr::Other
                };
            }
        }
        Ok(state == Attr::Set)
    }

    async fn dir_ignored(&mut self, dir: &[u8], parent: bool) -> Result<bool> {
        if let Some(&h) = self.ignored.get(dir) {
            return Ok(h);
        }
        let hit = parent || self.is_set(dir, true, "export-ignore").await?;
        if self.ignored.len() >= 8192 {
            self.ignored.clear();
        }
        self.ignored.insert(dir.to_vec(), hit);
        Ok(hit)
    }

    /// Whether `path` or one of its directories is `export-ignore`.
    async fn export_ignored(&mut self, path: &[u8]) -> Result<bool> {
        let mut parent = false;
        for (i, _) in path.iter().enumerate().filter(|(_, c)| **c == b'/') {
            parent = self.dir_ignored(&path[..i], parent).await?;
            if parent {
                return Ok(true);
            }
        }
        self.is_set(path, false, "export-ignore").await
    }
}

// ------------------------------------------------------------------ export-subst

/// What `$Format:…$` placeholders expand from.
struct Subst {
    git: GitOid,
    tree: GitOid,
    parents: Vec<GitOid>,
    author: Signature,
    committer: Signature,
    message: String,
    subject: String,
    body: String,
}

/// The expansion passed [`SUBST_MAX_OUT`].
struct Overflow;

/// A capped output buffer: a push that would pass the cap fails.
struct Capped {
    buf: Vec<u8>,
}

impl Capped {
    fn push(&mut self, b: &[u8]) -> std::result::Result<(), Overflow> {
        if b.len() > SUBST_MAX_OUT - self.buf.len() {
            return Err(Overflow);
        }
        self.buf.extend_from_slice(b);
        Ok(())
    }
}

impl Subst {
    fn new(
        git: GitOid,
        tree: GitOid,
        parents: Vec<GitOid>,
        author: Signature,
        committer: Signature,
        message: &[u8],
    ) -> Self {
        let message = lossy(message);
        let (subject, body) = {
            let trimmed = message.trim_start_matches('\n');
            match trimmed.split_once("\n\n") {
                Some((s, b)) => (s.replace('\n', " "), b.to_owned()),
                None => (trimmed.trim_end().replace('\n', " "), String::new()),
            }
        };
        Self {
            git,
            tree,
            parents,
            author,
            committer,
            message,
            subject,
            body,
        }
    }

    fn expand(&self, fmt: &str, out: &mut Capped) -> std::result::Result<(), Overflow> {
        let hex = |g: &GitOid| g.to_string();
        let mut chars = fmt.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c.encode_utf8(&mut [0; 4]).as_bytes())?;
                continue;
            }
            let Some(&n) = chars.peek() else {
                out.push(b"%")?;
                break;
            };
            let two: String = chars.clone().take(2).collect();
            let (text, used): (std::borrow::Cow<'_, str>, usize) = match (n, two.as_str()) {
                ('%', _) => ("%".into(), 1),
                ('n', _) => ("\n".into(), 1),
                ('H', _) => (hex(&self.git).into(), 1),
                ('h', _) => (hex(&self.git)[..7].to_owned().into(), 1),
                ('T', _) => (hex(&self.tree).into(), 1),
                ('t', _) => (hex(&self.tree)[..7].to_owned().into(), 1),
                ('P', _) => (
                    self.parents
                        .iter()
                        .map(hex)
                        .collect::<Vec<_>>()
                        .join(" ")
                        .into(),
                    1,
                ),
                ('p', _) => (
                    self.parents
                        .iter()
                        .map(|p| hex(p)[..7].to_owned())
                        .collect::<Vec<_>>()
                        .join(" ")
                        .into(),
                    1,
                ),
                ('s', _) => (self.subject.as_str().into(), 1),
                ('b', _) => (self.body.as_str().into(), 1),
                ('B', _) => (self.message.as_str().into(), 1),
                (_, "an") => (lossy(self.author.name()).into(), 2),
                (_, "ae") => (lossy(self.author.email()).into(), 2),
                (_, "at") => (self.author.time().to_string().into(), 2),
                (_, "cn") => (lossy(self.committer.name()).into(), 2),
                (_, "ce") => (lossy(self.committer.email()).into(), 2),
                (_, "ct") => (self.committer.time().to_string().into(), 2),
                _ => {
                    out.push(b"%")?;
                    continue;
                }
            };
            out.push(text.as_bytes())?;
            for _ in 0..used {
                chars.next();
            }
        }
        Ok(())
    }

    /// Replaces each `$Format:<fmt>$` on one line, as `git archive` does with `export-subst`.
    fn apply(&self, data: &[u8]) -> Result<Vec<u8>> {
        const OPEN: &[u8] = b"$Format:";
        let overflow =
            |_: Overflow| CliError::Fatal("asc: an export-subst expansion is too large".into());
        let mut out = Capped {
            buf: Vec::with_capacity(data.len()),
        };
        let mut rest = data;
        while let Some(i) = rest.windows(OPEN.len()).position(|w| w == OPEN) {
            let after = &rest[i + OPEN.len()..];
            let stop = after.iter().position(|&c| c == b'$' || c == b'\n');
            match stop {
                Some(j) if after[j] == b'$' => {
                    out.push(&rest[..i]).map_err(overflow)?;
                    self.expand(&lossy(&after[..j]), &mut out)
                        .map_err(overflow)?;
                    rest = &after[j + 1..];
                }
                _ => {
                    let upto = i + OPEN.len() + stop.map_or(after.len(), |j| j + 1);
                    out.push(&rest[..upto]).map_err(overflow)?;
                    rest = &rest[upto..];
                }
            }
        }
        out.push(rest).map_err(overflow)?;
        Ok(out.buf)
    }
}

// ------------------------------------------------------------------ writers

/// An archive format writer. A file is `start_file`, `data` any number of times, `end_file`.
trait Sink {
    fn dir(&mut self, name: &[u8]) -> io::Result<()>;
    fn start_file(&mut self, name: &[u8], exec: bool, size: u64) -> io::Result<()>;
    fn data(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn end_file(&mut self) -> io::Result<()>;
    fn symlink(&mut self, name: &[u8], target: &[u8]) -> io::Result<()>;
    fn finish(self: Box<Self>) -> io::Result<()>;
}

const BLOCK: usize = 512;

fn octal(field: &mut [u8], value: u64) {
    // `len - 1` digits, zero padded, then NUL.
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    let text = text.as_bytes();
    let take = text.len().min(digits);
    field[..digits - take].fill(b'0');
    field[digits - take..digits].copy_from_slice(&text[text.len() - take..]);
    field[digits] = 0;
}

fn put(field: &mut [u8], bytes: &[u8]) {
    let n = bytes.len().min(field.len());
    field[..n].copy_from_slice(&bytes[..n]);
}

/// One pax record: `<len> <key>=<value>\n`, where `len` counts the whole record.
fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let body_len = 1 + key.len() + 1 + value.len() + 1; // " key=value\n"
    let mut digits = 1;
    loop {
        let total = body_len + digits;
        let need = total.to_string().len();
        if need == digits {
            let mut rec = format!("{total} {key}=").into_bytes();
            rec.extend_from_slice(value);
            rec.push(b'\n');
            return rec;
        }
        digits = need;
    }
}

struct TarSink<W: Write> {
    out: W,
    mtime: u64,
    /// Bytes of the current file still to come, and its size (for the padding).
    left: u64,
    total: u64,
}

impl<W: Write> TarSink<W> {
    fn header(
        &mut self,
        name: &[u8],
        typeflag: u8,
        mode: u32,
        size: u64,
        link: &[u8],
    ) -> io::Result<()> {
        let mut pax: Vec<u8> = Vec::new();
        let mut h = [0u8; BLOCK];
        if name.len() > 100 {
            pax.extend(pax_record("path", name));
        }
        if link.len() > 100 {
            pax.extend(pax_record("linkpath", link));
        }
        let size_field = if size > 0o77_777_777_777 {
            pax.extend(pax_record("size", size.to_string().as_bytes()));
            0
        } else {
            size
        };
        if !pax.is_empty() {
            self.pax_block(b'x', name, &pax)?;
        }
        put(&mut h[0..100], name);
        octal(&mut h[100..108], u64::from(mode));
        octal(&mut h[108..116], 0);
        octal(&mut h[116..124], 0);
        octal(&mut h[124..136], size_field);
        octal(&mut h[136..148], self.mtime);
        h[148..156].fill(b' ');
        h[156] = typeflag;
        put(&mut h[157..257], link);
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        self.seal_and_write(&mut h)
    }

    fn seal_and_write(&mut self, h: &mut [u8; BLOCK]) -> io::Result<()> {
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        let text = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(text.as_bytes());
        self.out.write_all(h)
    }

    /// A pax extended header (`x`) entry carrying `records`.
    fn pax_block(&mut self, typeflag: u8, name: &[u8], records: &[u8]) -> io::Result<()> {
        let mut h = [0u8; BLOCK];
        let mut label = b"PaxHeader/".to_vec();
        label.extend_from_slice(&name[name.len().saturating_sub(80)..]);
        put(&mut h[0..100], &label);
        octal(&mut h[100..108], 0o644);
        octal(&mut h[108..116], 0);
        octal(&mut h[116..124], 0);
        octal(&mut h[124..136], records.len() as u64);
        octal(&mut h[136..148], self.mtime);
        h[148..156].fill(b' ');
        h[156] = typeflag;
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        self.seal_and_write(&mut h)?;
        self.out.write_all(records)?;
        self.pad(records.len() as u64)
    }

    fn pad(&mut self, written: u64) -> io::Result<()> {
        let rem = (written % BLOCK as u64) as usize;
        if rem != 0 {
            self.out.write_all(&[0u8; BLOCK][..BLOCK - rem])?;
        }
        Ok(())
    }

    /// The global pax header `git archive` writes: the commit id in a `comment` record.
    fn global(&mut self, commit: &GitOid) -> io::Result<()> {
        let records = pax_record("comment", commit.to_string().as_bytes());
        self.pax_block(b'g', b"pax_global_header", &records)
    }
}

impl<W: Write> Sink for TarSink<W> {
    fn dir(&mut self, name: &[u8]) -> io::Result<()> {
        self.header(name, b'5', 0o755, 0, b"")
    }

    fn start_file(&mut self, name: &[u8], exec: bool, size: u64) -> io::Result<()> {
        self.header(name, b'0', if exec { 0o755 } else { 0o644 }, size, b"")?;
        self.left = size;
        self.total = size;
        Ok(())
    }

    fn data(&mut self, bytes: &[u8]) -> io::Result<()> {
        let n = bytes.len() as u64;
        if n > self.left {
            return Err(io::Error::other("a file is longer than its recorded size"));
        }
        self.left -= n;
        self.out.write_all(bytes)
    }

    fn end_file(&mut self) -> io::Result<()> {
        if self.left != 0 {
            return Err(io::Error::other("a file is shorter than its recorded size"));
        }
        self.pad(self.total)
    }

    fn symlink(&mut self, name: &[u8], target: &[u8]) -> io::Result<()> {
        self.header(name, b'2', 0o777, 0, target)
    }

    fn finish(mut self: Box<Self>) -> io::Result<()> {
        self.out.write_all(&[0u8; BLOCK * 2])?;
        self.out.flush()
    }
}

struct ZipSink<W: Write> {
    zip: zip::ZipWriter<zip::write::StreamWriter<W>>,
    time: zip::DateTime,
}

fn zip_err(e: zip::result::ZipError) -> io::Error {
    match e {
        zip::result::ZipError::Io(e) => e,
        e => io::Error::other(e.to_string()),
    }
}

impl<W: Write> ZipSink<W> {
    fn options(&self, mode: u32, size: u64) -> zip::write::SimpleFileOptions {
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(self.time)
            .unix_permissions(mode)
            .large_file(size >= u64::from(u32::MAX))
    }
}

impl<W: Write> Sink for ZipSink<W> {
    fn dir(&mut self, name: &[u8]) -> io::Result<()> {
        let Ok(name) = std::str::from_utf8(name) else {
            return Ok(());
        };
        let opts = self.options(0o040_755, 0);
        self.zip.add_directory(name, opts).map_err(zip_err)
    }

    fn start_file(&mut self, name: &[u8], exec: bool, size: u64) -> io::Result<()> {
        let Ok(name) = std::str::from_utf8(name) else {
            return Err(io::Error::other("a path is not UTF-8, which zip needs"));
        };
        let opts = self.options(if exec { 0o100_755 } else { 0o100_644 }, size);
        self.zip.start_file(name, opts).map_err(zip_err)
    }

    fn data(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.zip.write_all(bytes)
    }

    fn end_file(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn symlink(&mut self, name: &[u8], target: &[u8]) -> io::Result<()> {
        let (Ok(name), Ok(target)) = (std::str::from_utf8(name), std::str::from_utf8(target))
        else {
            return Err(io::Error::other("a path is not UTF-8, which zip needs"));
        };
        let opts = self.options(0o120_777, 0);
        self.zip.add_symlink(name, target, opts).map_err(zip_err)
    }

    fn finish(self: Box<Self>) -> io::Result<()> {
        let mut w = self.zip.finish().map_err(zip_err)?.into_inner();
        w.flush()
    }
}

/// The calendar date of `secs` since the Unix epoch (UTC).
fn civil(secs: u64) -> (u16, u8, u8, u8, u8, u8) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        u16::try_from(y).unwrap_or(1980),
        m as u8,
        d as u8,
        (rem / 3600) as u8,
        (rem % 3600 / 60) as u8,
        (rem % 60) as u8,
    )
}

// ------------------------------------------------------------------ content

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: archive failed: {e}"))
}

/// Streams the content of one file to `sink`, verified, holding at most one chunk.
async fn stream_file(ws: &WsCtx, file: FileId, size: u64, sink: &mut dyn Sink) -> Result<()> {
    if size <= CHUNK_THRESHOLD {
        let data = ws.engine.read_file(file, size).await.map_err(fatal)?;
        return sink.data(&data).map_err(fatal);
    }
    let list = ws
        .objects
        .get(&ObjKey::ChunkList(file))
        .await
        .map_err(fatal)?
        .ok_or_else(|| fatal(format!("the chunk list of {file} is not available")))?;
    let list = decode_envelope::<ChunkList>(&list).map_err(fatal)?;
    if list.size != size {
        return Err(fatal(format!(
            "the chunk list of {file} disagrees with its size"
        )));
    }
    let chunks = list.chunks.clone();
    let mut assembler = ChunkAssembler::new(list, file).map_err(fatal)?;
    for (id, len) in &chunks {
        let chunk = ws
            .objects
            .get(&ObjKey::Chunk(*id))
            .await
            .map_err(fatal)?
            .ok_or_else(|| fatal(format!("a chunk of {file} is not available")))?;
        if chunk.len() != *len as usize {
            return Err(fatal(format!("a chunk of {file} has the wrong length")));
        }
        assembler.push(&chunk).map_err(fatal)?;
        sink.data(&chunk).map_err(fatal)?;
    }
    assembler.finish().map(|_| ()).map_err(fatal)
}

fn open_output(ctx: &Ctx, output: Option<&str>) -> Result<(Box<dyn Write>, Option<PathBuf>)> {
    match output {
        Some(name) => {
            let path = if std::path::Path::new(name).is_absolute() {
                PathBuf::from(name)
            } else {
                ctx.cwd.join(name)
            };
            let file = std::fs::File::create(&path).map_err(|e| {
                CliError::Fatal(format!("asc: cannot create {}: {e}", path.display()))
            })?;
            Ok((Box::new(io::BufWriter::new(file)), Some(path)))
        }
        None => {
            let out = io::stdout();
            if out.is_terminal() {
                return Err(CliError::Fatal(
                    "refusing to write archive content to terminal (missing -o option?)".into(),
                ));
            }
            Ok((Box::new(io::BufWriter::new(out)), None))
        }
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let o = parse(&args.rest)?;
    if o.list {
        println!("tar\nzip");
        return Ok(ExitCode::SUCCESS);
    }
    let ws = ctx.ws()?;
    let mut words = o.rest.iter();
    let Some(treeish) = words.next() else {
        return Err(CliError::Usage(USAGE.trim_end().to_owned()));
    };
    let paths: Vec<OsString> = words.cloned().collect();
    let treeish = treeish.to_string_lossy().into_owned();
    let format = match (o.format, &o.output) {
        (Some(f), _) => f,
        (None, Some(name)) => format_of_name(name)?.unwrap_or(Format::Tar),
        (None, None) => Format::Tar,
    };
    let prefix = archive_prefix(&o.prefix)?;
    let id = resolve_commit(ctx, &treeish)
        .await?
        .ok_or_else(|| CliError::Fatal(format!("not a valid object name: {treeish}")))?;
    let cs = ws.engine.get_changeset(id).await?;
    let spec = if paths.is_empty() {
        None
    } else {
        Some(Spec::new(ctx, &paths)?)
    };
    let mut parents = Vec::with_capacity(cs.parents.len());
    for p in &cs.parents {
        parents.push(ws.engine.git_commit_of(*p).await.map_err(fatal)?);
    }
    let commit_git = ws.engine.git_commit_of(id).await.map_err(fatal)?;
    let tree = ws.engine.get_tree(cs.root).await?;
    let subst = Subst::new(
        commit_git,
        tree_oid(&tree),
        parents,
        cs.author.clone(),
        cs.committer.clone(),
        &cs.message,
    );
    let mtime = u64::try_from(cs.committer.time()).unwrap_or(0);

    let (out, out_path) = open_output(ctx, o.output.as_deref())?;
    let result = write_archive(
        ws,
        format,
        out,
        &prefix,
        &o,
        spec.as_ref(),
        cs.root,
        &subst,
        mtime,
    )
    .await;
    if let (Err(_), Some(path)) = (&result, &out_path) {
        let _ = std::fs::remove_file(path);
    }
    result?;
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
async fn write_archive(
    ws: &WsCtx,
    format: Format,
    out: Box<dyn Write>,
    prefix: &[u8],
    o: &Opts,
    spec: Option<&Spec>,
    root: TreeId,
    subst: &Subst,
    mtime: u64,
) -> Result<()> {
    let mut sink: Box<dyn Sink> = match format {
        Format::Tar => {
            let mut tar = TarSink {
                out,
                mtime,
                left: 0,
                total: 0,
            };
            tar.global(&subst.git).map_err(fatal)?;
            Box::new(tar)
        }
        Format::Zip => {
            let (y, mo, d, h, mi, s) = civil(mtime);
            let time = zip::DateTime::from_date_and_time(y, mo, d, h, mi, s).unwrap_or_default();
            Box::new(ZipSink {
                zip: zip::ZipWriter::new_stream(out),
                time,
            })
        }
    };
    let mut attrs = TreeAttrs::new(&ws.engine, root)?;
    let mut emitted: HashSet<Vec<u8>> = HashSet::new();
    // The prefix directories come first, as git writes them.
    let mut acc: Vec<u8> = Vec::new();
    for comp in prefix.split(|&c| c == b'/').filter(|c| !c.is_empty()) {
        acc.extend_from_slice(comp);
        acc.push(b'/');
        sink.dir(&acc).map_err(fatal)?;
    }
    let mut walk = ws.engine.walk(root, spec.map(|s| &s.spec));
    while let Some((path, e)) = walk.try_next().await? {
        if e.kind == Kind::Gitlink || attrs.export_ignored(path.as_bytes()).await? {
            continue;
        }
        // Directory entries for the ancestors not written yet.
        let bytes = path.as_bytes();
        for (i, _) in bytes.iter().enumerate().filter(|(_, c)| **c == b'/') {
            let dir = &bytes[..i];
            if emitted.insert(dir.to_vec()) {
                let name = [prefix, dir, b"/"].concat();
                sink.dir(&name).map_err(fatal)?;
            }
        }
        let name = [prefix, bytes].concat();
        if o.verbose {
            eprintln!("{}", lossy(&name));
        }
        match e.kind {
            Kind::Symlink => {
                if e.size <= SYMLINK_MAX {
                    let target = ws
                        .engine
                        .read_file(FileId::from_bytes(e.id), e.size)
                        .await
                        .map_err(fatal)?;
                    sink.symlink(&name, &target).map_err(fatal)?;
                }
            }
            Kind::File | Kind::Exec => {
                let exec = e.kind == Kind::Exec;
                if e.size <= SUBST_MAX_FILE && attrs.is_set(bytes, false, "export-subst").await? {
                    let data = ws
                        .engine
                        .read_file(FileId::from_bytes(e.id), e.size)
                        .await
                        .map_err(fatal)?;
                    let data = subst.apply(&data)?;
                    sink.start_file(&name, exec, data.len() as u64)
                        .map_err(fatal)?;
                    sink.data(&data).map_err(fatal)?;
                } else {
                    sink.start_file(&name, exec, e.size).map_err(fatal)?;
                    stream_file(ws, FileId::from_bytes(e.id), e.size, &mut *sink).await?;
                }
                sink.end_file().map_err(fatal)?;
            }
            Kind::Dir | Kind::Gitlink => {}
        }
    }
    sink.finish().map_err(fatal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn options() {
        let o = parse(&os(&[
            "--format=zip",
            "-o",
            "x.zip",
            "--prefix",
            "p/",
            "-v",
            "HEAD",
            "src",
        ]))
        .unwrap();
        assert_eq!(o.format, Some(Format::Zip));
        assert_eq!(o.output.as_deref(), Some("x.zip"));
        assert_eq!(o.prefix, "p/");
        assert!(o.verbose);
        assert_eq!(o.rest, os(&["HEAD", "src"]));
        assert_eq!(
            parse(&os(&["-ofile.tar", "HEAD"]))
                .unwrap()
                .output
                .as_deref(),
            Some("file.tar")
        );
        assert!(parse(&os(&["--format=rar"])).is_err());
        assert!(parse(&os(&["--bogus"])).is_err());
    }

    #[test]
    fn names_imply_formats() {
        assert_eq!(format_of_name("a.zip").unwrap(), Some(Format::Zip));
        assert_eq!(format_of_name("a.TAR").unwrap(), Some(Format::Tar));
        assert_eq!(format_of_name("a").unwrap(), None);
        assert!(format_of_name("a.tar.gz").is_err());
        assert!(format_of_name("a.tgz").is_err());
    }

    #[test]
    fn prefixes() {
        assert_eq!(archive_prefix("").unwrap(), b"");
        assert_eq!(archive_prefix("a/b").unwrap(), b"a/b/");
        assert_eq!(archive_prefix("a/").unwrap(), b"a/");
        assert!(archive_prefix("../x").is_err());
    }

    #[test]
    fn pax_lengths_count_themselves() {
        for value in ["x", "xxxxxxxx", &"y".repeat(95), &"z".repeat(990)] {
            let rec = pax_record("path", value.as_bytes());
            let text = String::from_utf8(rec.clone()).unwrap();
            let (len, _) = text.split_once(' ').unwrap();
            assert_eq!(len.parse::<usize>().unwrap(), rec.len(), "{value:.5}");
        }
    }

    #[test]
    fn octal_fields() {
        let mut f = [0xffu8; 8];
        octal(&mut f, 0o644);
        assert_eq!(&f, b"0000644\0");
    }

    #[test]
    fn tar_blocks_are_whole() {
        let mut t = TarSink {
            out: Vec::new(),
            mtime: 1_700_000_000,
            left: 0,
            total: 0,
        };
        t.start_file(b"a.txt", false, 3).unwrap();
        t.data(b"abc").unwrap();
        t.end_file().unwrap();
        assert_eq!(t.out.len() % BLOCK, 0);
        assert_eq!(&t.out[0..5], b"a.txt");
        assert_eq!(&t.out[257..262], b"ustar");
        let sum: u32 = {
            let mut h = t.out[..BLOCK].to_vec();
            h[148..156].fill(b' ');
            h.iter().map(|&b| u32::from(b)).sum()
        };
        let stored = std::str::from_utf8(&t.out[148..154]).unwrap();
        assert_eq!(u32::from_str_radix(stored, 8).unwrap(), sum);
        assert!(t.data(b"x").is_err() || t.left == 0);
    }

    #[test]
    fn export_subst_expands() {
        let sig = Signature::new(b"A".to_vec(), b"a@x".to_vec(), 5, 0).unwrap();
        let s = Subst::new(
            GitOid::from_bytes([1; 20]),
            GitOid::from_bytes([2; 20]),
            vec![GitOid::from_bytes([3; 20])],
            sig.clone(),
            sig,
            b"subject line\n\nbody\n",
        );
        let out = s.apply(b"v=$Format:%h %s|%an|%ct|%%$ end\n").unwrap();
        assert_eq!(out, b"v=0101010 subject line|A|5|% end\n");
        // An opener without a closer on its line is copied through.
        assert_eq!(s.apply(b"$Format:%H\nx").unwrap(), b"$Format:%H\nx");
    }
}
