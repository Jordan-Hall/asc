//! A `git apply`-compatible patch parser and applier (PLAN U27b; used by `asc apply`, `asc am` and,
//! later, the `Apply` RPC).
//!
//! [`parse`] reads unified diffs with or without git's extended headers (renames, copies, modes,
//! `GIT binary patch`) into [`FilePatch`]es. [`apply_text`] and [`apply_file`] apply one file's
//! hunks to its bytes with git's placement rules: a hunk is looked for at the line the header
//! names and then outward, nearest first, and it must match in full (context included). A hunk
//! that starts at line 1, or has no trailing context, is pinned to the start or the end of the
//! file unless `unidiff_zero` is set.
//!
//! The input is untrusted: sizes, counts and work are bounded (see the `MAX_*` constants), and
//! every path a patch names is checked by [`path_allowed`], which is `asc_core::path::checkout_safe` and
//! nothing weaker (no `..`, no `.git` or `.asc/info` alias).
//! Binary patches carry deflated data; the engine has no inflater, so [`apply_binary`] takes one
//! from the caller.

use std::borrow::Cow;
use std::cell::Cell;

use asc_core::path::{RepoPath, checkout_safe};

/// Largest patch [`parse`] reads.
pub const MAX_PATCH_BYTES: usize = 1 << 30;
/// Most file patches in one input.
pub const MAX_FILES: usize = 100_000;
/// Most hunks in one file patch.
pub const MAX_HUNKS: usize = 1_000_000;
/// Largest file [`apply_text`] works on, and the largest binary result.
pub const MAX_APPLY_BYTES: usize = 1 << 30;
/// Bound on `lines x hunks`, the cost of splicing hunks into a file's line list.
const MAX_SPLICE_WORK: u128 = 4_000_000_000;
/// Bound on the work of placing hunks: one unit per preimage line compared, plus one per 64
/// bytes compared. This counts `lines x preimage length`, which `MAX_SPLICE_WORK` does not.
pub const MAX_MATCH_WORK: u64 = 1 << 30;

/// Why a patch could not be read or applied.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("corrupt patch at line {0}")]
    Corrupt(usize),
    #[error("patch fragment without header at line {0}: {1}")]
    NoHeader(usize, String),
    #[error("git diff header lacks filename information at line {0}")]
    NoName(usize),
    #[error("unsafe path in patch: {0}")]
    UnsafePath(String),
    #[error("patch is over the {MAX_PATCH_BYTES} byte limit")]
    TooLarge,
    #[error("patch has too many files or hunks")]
    TooMany,
    #[error("{0}: already exists in working directory")]
    Exists(String),
    #[error("{0}: No such file or directory")]
    Missing(String),
    #[error("patch failed: {path}:{line}")]
    HunkFailed { path: String, line: usize },
    #[error("removal patch leaves file contents")]
    RemovalLeavesContent,
    #[error("cannot apply binary patch without full index line")]
    BinaryNeedsIndex,
    #[error("cannot reverse-apply a binary patch without reverse hunk")]
    BinaryNotReversible,
    #[error("binary patch does not apply")]
    BinaryFailed,
    #[error("file is over the {MAX_APPLY_BYTES} byte limit")]
    FileTooLarge,
    #[error("the patch is too expensive to apply to this file")]
    TooExpensive,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What one hunk line does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineOp {
    Context,
    Add,
    Del,
}

/// One hunk line. `text` ends in `\n` unless the patch marked "No newline at end of file".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkLine {
    pub op: LineOp,
    pub text: Vec<u8>,
}

/// One `@@` hunk. Starts are 1-based as in the header (0 for an empty range at the top).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<HunkLine>,
}

impl Hunk {
    fn trailing(&self) -> usize {
        self.lines
            .iter()
            .rev()
            .take_while(|l| l.op == LineOp::Context)
            .count()
    }
}

/// How the body of one direction of a binary patch is coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryKind {
    Literal,
    Delta,
}

/// One direction of a binary patch: `size` is the inflated size; `deflated` is the zlib stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryBody {
    pub kind: BinaryKind,
    pub size: usize,
    pub deflated: Vec<u8>,
}

/// A `GIT binary patch`: the forward body and, usually, the reverse one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryPatch {
    pub forward: BinaryBody,
    pub reverse: Option<BinaryBody>,
}

/// The patch to one file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilePatch {
    /// The path before the patch (`None` for a new file), already stripped.
    pub old_path: Option<Vec<u8>>,
    /// The path after the patch (`None` for a deletion).
    pub new_path: Option<Vec<u8>>,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub is_new: bool,
    pub is_delete: bool,
    pub is_rename: bool,
    pub is_copy: bool,
    /// Similarity of a rename or copy, in percent.
    pub similarity: Option<u32>,
    /// The hex ids of the `index <old>..<new>` line (abbreviated or full).
    pub old_oid: Option<Vec<u8>>,
    pub new_oid: Option<Vec<u8>>,
    pub hunks: Vec<Hunk>,
    pub binary: Option<BinaryPatch>,
    /// `Binary files a and b differ` without a body: git cannot apply it.
    pub binary_no_body: bool,
}

impl FilePatch {
    /// The path the patch reads from (the old path, else the new one).
    pub fn source(&self) -> Option<&[u8]> {
        self.old_path.as_deref().or(self.new_path.as_deref())
    }

    /// The path the patch writes (the new path, else the old one).
    pub fn target(&self) -> Option<&[u8]> {
        self.new_path.as_deref().or(self.old_path.as_deref())
    }

    /// The patch undone: what `-R` applies. A binary patch needs its reverse body.
    pub fn reversed(&self) -> Result<FilePatch> {
        let mut r = self.clone();
        std::mem::swap(&mut r.old_path, &mut r.new_path);
        std::mem::swap(&mut r.old_mode, &mut r.new_mode);
        std::mem::swap(&mut r.old_oid, &mut r.new_oid);
        std::mem::swap(&mut r.is_new, &mut r.is_delete);
        for h in &mut r.hunks {
            std::mem::swap(&mut h.old_start, &mut h.new_start);
            std::mem::swap(&mut h.old_len, &mut h.new_len);
            for l in &mut h.lines {
                l.op = match l.op {
                    LineOp::Add => LineOp::Del,
                    LineOp::Del => LineOp::Add,
                    LineOp::Context => LineOp::Context,
                };
            }
            // Within a change group deletions come first, as git writes them.
            reorder_groups(&mut h.lines);
        }
        if let Some(b) = r.binary.take() {
            let rev = b.reverse.ok_or(Error::BinaryNotReversible)?;
            r.binary = Some(BinaryPatch {
                forward: rev,
                reverse: Some(b.forward),
            });
        }
        Ok(r)
    }
}

fn reorder_groups(lines: &mut [HunkLine]) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].op == LineOp::Context {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < lines.len() && lines[j].op != LineOp::Context {
            j += 1;
        }
        lines[i..j].sort_by_key(|l| u8::from(l.op == LineOp::Add));
        i = j;
    }
}

/// Parser settings.
#[derive(Clone, Debug)]
pub struct ParseOptions {
    /// Leading path components removed from `---`/`+++` and `diff --git` names (`-p<n>`).
    pub strip: usize,
    /// Ignore the line counts of `@@` headers and count the hunk lines (`--recount`).
    pub recount: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            strip: 1,
            recount: false,
        }
    }
}

/// True when a patch may write `name`: it is a valid repository path and passes
/// `asc_core::path::checkout_safe`, the one rule every checkout and import uses. There is no
/// rule of our own here; `.git` and `.asc/info` aliases are refused by that function.
pub fn path_allowed(name: &[u8]) -> bool {
    RepoPath::new(name.to_vec()).is_ok_and(|p| checkout_safe(&p))
}

/// True when a symlink at `link` may point at `target`: relative, never above the top of the
/// work tree, and naming no `.git` or `.asc/info` alias once the `.`/`..` components are folded.
pub fn symlink_target_allowed(link: &[u8], target: &[u8]) -> bool {
    if target.is_empty()
        || target.contains(&0)
        || target.contains(&92)
        || target[0] == b'/'
        || (target.len() >= 2 && target[1] == b':' && target[0].is_ascii_alphabetic())
    {
        return false;
    }
    // Resolve against the directory that holds the link.
    let mut stack: Vec<&[u8]> = link.split(|&b| b == b'/').collect();
    stack.pop();
    for c in target.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                if stack.pop().is_none() {
                    return false;
                }
            }
            c => stack.push(c),
        }
    }
    // A link to the top of the tree, or to an ancestor of the link, names no component.
    stack.is_empty() || path_allowed(&stack.join(&b'/'))
}

struct Cur<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn peek(&self) -> Option<&'a [u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let rest = &self.data[self.pos..];
        let end = memchr::memchr(b'\n', rest).map_or(rest.len(), |i| i + 1);
        Some(&rest[..end])
    }

    fn next(&mut self) -> Option<&'a [u8]> {
        let l = self.peek()?;
        self.pos += l.len();
        Some(l)
    }

    fn line_no(&self) -> usize {
        1 + memchr::memchr_iter(b'\n', &self.data[..self.pos.min(self.data.len())]).count()
    }
}

fn trim_eol(l: &[u8]) -> &[u8] {
    let l = l.strip_suffix(b"\n").unwrap_or(l);
    l.strip_suffix(b"\r").filter(|_| false).unwrap_or(l)
}

/// Parses every file patch in `input`. Text outside patches (mail headers, a commit message,
/// the diffstat) is skipped.
pub fn parse(input: &[u8], opts: &ParseOptions) -> Result<Vec<FilePatch>> {
    if input.len() > MAX_PATCH_BYTES {
        return Err(Error::TooLarge);
    }
    let mut cur = Cur {
        data: input,
        pos: 0,
    };
    let mut out = Vec::new();
    let mut total_hunks = 0usize;
    while let Some(line) = cur.peek() {
        let is_git = line.starts_with(b"diff --git ");
        let is_trad = line.starts_with(b"--- ")
            && next_line_after(&cur).is_some_and(|n| n.starts_with(b"+++ "));
        if is_git || is_trad {
            let mut fp = if is_git {
                parse_git_header(&mut cur, opts)?
            } else {
                FilePatch::default()
            };
            if !is_git || cur.peek().is_some_and(|l| l.starts_with(b"--- ")) {
                parse_names(&mut cur, &mut fp, opts, is_git)?;
            }
            parse_body(&mut cur, &mut fp, opts)?;
            total_hunks += fp.hunks.len();
            if out.len() >= MAX_FILES || total_hunks > MAX_HUNKS {
                return Err(Error::TooMany);
            }
            finish(&mut fp, cur.line_no())?;
            out.push(fp);
        } else if line.starts_with(b"@@ -") && !out.is_empty() {
            // A fragment after a complete patch with no header of its own.
            return Err(Error::NoHeader(
                cur.line_no(),
                String::from_utf8_lossy(trim_eol(line)).into_owned(),
            ));
        } else {
            cur.next();
        }
    }
    Ok(out)
}

fn next_line_after<'a>(cur: &Cur<'a>) -> Option<&'a [u8]> {
    let l = cur.peek()?;
    let c = Cur {
        data: cur.data,
        pos: cur.pos + l.len(),
    };
    c.peek()
}

/// Names and flags are settled; check the paths and fill what the header left open.
fn finish(fp: &mut FilePatch, line: usize) -> Result<()> {
    if fp.old_path.is_none() && fp.new_path.is_none() {
        return Err(Error::NoName(line));
    }
    if fp.old_path.is_none() {
        fp.is_new = true;
    }
    if fp.new_path.is_none() {
        fp.is_delete = true;
    }
    for p in [&fp.old_path, &fp.new_path].into_iter().flatten() {
        if !path_allowed(p) {
            return Err(Error::UnsafePath(String::from_utf8_lossy(p).into_owned()));
        }
    }
    // A symlink's content is its target: refuse one that leaves the tree or names a `.git` alias.
    for (path, mode, ops) in [
        (&fp.old_path, fp.old_mode, [LineOp::Context, LineOp::Del]),
        (&fp.new_path, fp.new_mode, [LineOp::Context, LineOp::Add]),
    ] {
        let (Some(path), Some(m)) = (path, mode) else {
            continue;
        };
        if m & 0o170000 != 0o120000 {
            continue;
        }
        let mut target = Vec::new();
        for l in fp.hunks.iter().flat_map(|h| &h.lines) {
            if ops.contains(&l.op) {
                target.extend_from_slice(&l.text);
            }
        }
        if target.last() == Some(&10) {
            target.pop();
        }
        if !symlink_target_allowed(path, &target) {
            return Err(Error::UnsafePath(format!(
                "{} -> {}",
                String::from_utf8_lossy(path),
                String::from_utf8_lossy(&target)
            )));
        }
    }
    Ok(())
}

fn parse_mode(v: &[u8]) -> Option<u32> {
    u32::from_str_radix(std::str::from_utf8(v).ok()?.trim(), 8).ok()
}

/// Reads a possibly C-quoted name that runs to the end of `v` (or to a tab).
fn read_name(v: &[u8]) -> Option<(Vec<u8>, usize)> {
    if v.first() == Some(&b'"') {
        let mut out = Vec::new();
        let mut i = 1;
        while i < v.len() {
            match v[i] {
                b'"' => return Some((out, i + 1)),
                b'\\' => {
                    i += 1;
                    let c = *v.get(i)?;
                    match c {
                        b'a' => out.push(7),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'v' => out.push(11),
                        b'"' | b'\\' => out.push(c),
                        b'0'..=b'3' => {
                            let oct = v.get(i..i + 3)?;
                            if !oct.iter().all(|d| (b'0'..=b'7').contains(d)) {
                                return None;
                            }
                            out.push((oct[0] - b'0') << 6 | (oct[1] - b'0') << 3 | (oct[2] - b'0'));
                            i += 2;
                        }
                        _ => return None,
                    }
                    i += 1;
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        return None;
    }
    let end = v.iter().position(|&b| b == b'\t').unwrap_or(v.len());
    Some((v[..end].to_vec(), end))
}

fn strip_components(name: &[u8], n: usize) -> Option<Vec<u8>> {
    let mut rest = name;
    for _ in 0..n {
        let i = memchr::memchr(b'/', rest)?;
        // Collapse repeated slashes after the removed component.
        rest = &rest[i + 1..];
        while rest.first() == Some(&b'/') {
            rest = &rest[1..];
        }
    }
    (!rest.is_empty()).then(|| rest.to_vec())
}

/// `a/x b/x` after `diff --git `: the two names are equal after the strip, so split where both
/// halves match.
fn git_header_names(v: &[u8], strip: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    if v.first() == Some(&b'"') {
        let (a, n) = read_name(v)?;
        let rest = v.get(n..)?.strip_prefix(b" ")?;
        let (b, _) = read_name(rest)?;
        return Some((strip_components(&a, strip)?, strip_components(&b, strip)?));
    }
    for (i, _) in v.iter().enumerate().filter(|(_, b)| **b == b' ') {
        let (a, b) = (&v[..i], &v[i + 1..]);
        if b.first() == Some(&b'"') {
            continue;
        }
        if let (Some(sa), Some(sb)) = (strip_components(a, strip), strip_components(b, strip))
            && sa == sb
        {
            return Some((sa, sb));
        }
    }
    None
}

fn parse_git_header(cur: &mut Cur<'_>, opts: &ParseOptions) -> Result<FilePatch> {
    let first = cur.next().unwrap_or_default();
    let mut fp = FilePatch::default();
    let default_names = git_header_names(trim_eol(&first[b"diff --git ".len()..]), opts.strip);
    let (mut old_set, mut new_set) = (false, false);
    while let Some(line) = cur.peek() {
        let l = trim_eol(line);
        if l.starts_with(b"diff --git ") || l.starts_with(b"--- ") || l.starts_with(b"@@ ") {
            break;
        }
        if let Some(v) = l.strip_prefix(b"old mode ") {
            fp.old_mode = Some(parse_mode(v).ok_or(Error::Corrupt(cur.line_no()))?);
        } else if let Some(v) = l.strip_prefix(b"new mode ") {
            fp.new_mode = Some(parse_mode(v).ok_or(Error::Corrupt(cur.line_no()))?);
        } else if let Some(v) = l.strip_prefix(b"deleted file mode ") {
            fp.is_delete = true;
            fp.old_mode = Some(parse_mode(v).ok_or(Error::Corrupt(cur.line_no()))?);
        } else if let Some(v) = l.strip_prefix(b"new file mode ") {
            fp.is_new = true;
            fp.new_mode = Some(parse_mode(v).ok_or(Error::Corrupt(cur.line_no()))?);
        } else if let Some(v) = l.strip_prefix(b"copy from ") {
            fp.is_copy = true;
            fp.old_path = Some(read_name(v).ok_or(Error::Corrupt(cur.line_no()))?.0);
            old_set = true;
        } else if let Some(v) = l.strip_prefix(b"copy to ") {
            fp.is_copy = true;
            fp.new_path = Some(read_name(v).ok_or(Error::Corrupt(cur.line_no()))?.0);
            new_set = true;
        } else if let Some(v) = l.strip_prefix(b"rename from ") {
            fp.is_rename = true;
            fp.old_path = Some(read_name(v).ok_or(Error::Corrupt(cur.line_no()))?.0);
            old_set = true;
        } else if let Some(v) = l.strip_prefix(b"rename to ") {
            fp.is_rename = true;
            fp.new_path = Some(read_name(v).ok_or(Error::Corrupt(cur.line_no()))?.0);
            new_set = true;
        } else if let Some(v) = l.strip_prefix(b"similarity index ") {
            let n = v.strip_suffix(b"%").unwrap_or(v);
            fp.similarity = std::str::from_utf8(n).ok().and_then(|s| s.parse().ok());
        } else if l.starts_with(b"dissimilarity index ") {
            // Informational.
        } else if let Some(v) = l.strip_prefix(b"index ") {
            let (ids, mode) = match v.iter().position(|&b| b == b' ') {
                Some(i) => (&v[..i], Some(&v[i + 1..])),
                None => (v, None),
            };
            if let Some(d) = ids.windows(2).position(|w| w == b"..") {
                let (a, b) = (&ids[..d], &ids[d + 2..]);
                if a.iter().chain(b).all(u8::is_ascii_hexdigit) && !a.is_empty() && !b.is_empty() {
                    fp.old_oid = Some(a.to_vec());
                    fp.new_oid = Some(b.to_vec());
                }
            }
            if let Some(m) = mode.and_then(parse_mode) {
                fp.old_mode = Some(m);
                fp.new_mode = Some(m);
            }
        } else {
            break;
        }
        cur.next();
    }
    // A name from `diff --git` serves when no rename/copy line or `---` line names the file.
    if let Some((a, b)) = default_names {
        if !old_set && !fp.is_new {
            fp.old_path = Some(a);
        }
        if !new_set && !fp.is_delete {
            fp.new_path = Some(b);
        }
    } else if !(cur.peek().is_some_and(|l| l.starts_with(b"--- ")) || (old_set && new_set)) {
        // Names unknown here and no `---` line follows.
        if cur
            .peek()
            .is_some_and(|l| l.starts_with(b"GIT binary patch") || l.starts_with(b"Binary files "))
        {
            return Err(Error::NoName(cur.line_no()));
        }
        if fp.old_path.is_none() && fp.new_path.is_none() {
            return Err(Error::NoName(cur.line_no()));
        }
    }
    Ok(fp)
}

/// Reads the `--- `/`+++ ` pair.
fn parse_names(
    cur: &mut Cur<'_>,
    fp: &mut FilePatch,
    opts: &ParseOptions,
    git: bool,
) -> Result<()> {
    let old = cur.next().ok_or(Error::Corrupt(cur.line_no()))?;
    let new = cur
        .next()
        .filter(|l| l.starts_with(b"+++ "))
        .ok_or(Error::Corrupt(cur.line_no()))?;
    let name = |l: &[u8]| -> Result<Option<Vec<u8>>> {
        let (n, _) = read_name(trim_eol(&l[4..])).ok_or(Error::Corrupt(0))?;
        if n == b"/dev/null" {
            return Ok(None);
        }
        Ok(Some(
            strip_components(&n, opts.strip).ok_or(Error::Corrupt(0))?,
        ))
    };
    let (o, n) = (name(old)?, name(new)?);
    if !git {
        // A traditional diff: `/dev/null` marks creation or deletion. With both names, git
        // prefers the one that exists; we take the new name for both when they differ only by
        // the prefix handled in the strip.
        fp.old_path = o;
        fp.new_path = n;
        return Ok(());
    }
    if o.is_none() {
        fp.is_new = true;
        fp.old_path = None;
    } else if fp.old_path.is_none() && !fp.is_new {
        fp.old_path = o;
    }
    if n.is_none() {
        fp.is_delete = true;
        fp.new_path = None;
    } else if fp.new_path.is_none() && !fp.is_delete {
        fp.new_path = n;
    }
    Ok(())
}

fn parse_range(v: &[u8]) -> Option<(usize, usize, usize)> {
    // `<start>[,<len>]`, returning the rest after it as the third value (unused).
    let s = std::str::from_utf8(v).ok()?;
    let (a, b) = match s.split_once(',') {
        Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
        None => (s.parse().ok()?, 1),
    };
    Some((a, b, 0))
}

fn parse_hunk_header(l: &[u8]) -> Option<(usize, usize, usize, usize)> {
    let rest = l.strip_prefix(b"@@ -")?;
    let end = rest.windows(3).position(|w| w == b" @@")?;
    let body = &rest[..end];
    let plus = body.windows(2).position(|w| w == b" +")?;
    let (a, b, _) = parse_range(&body[..plus])?;
    let (c, d, _) = parse_range(&body[plus + 2..])?;
    Some((a, b, c, d))
}

fn parse_body(cur: &mut Cur<'_>, fp: &mut FilePatch, opts: &ParseOptions) -> Result<()> {
    if let Some(l) = cur.peek() {
        if l.starts_with(b"GIT binary patch") {
            cur.next();
            fp.binary = Some(parse_binary(cur)?);
            return Ok(());
        }
        if l.starts_with(b"Binary files ") {
            cur.next();
            fp.binary_no_body = true;
            return Ok(());
        }
    }
    while let Some(l) = cur.peek() {
        if !l.starts_with(b"@@ -") {
            break;
        }
        let (os, ol, ns, nl) =
            parse_hunk_header(trim_eol(l)).ok_or(Error::Corrupt(cur.line_no()))?;
        cur.next();
        let mut hunk = Hunk {
            old_start: os,
            old_len: ol,
            new_start: ns,
            new_len: nl,
            lines: Vec::new(),
        };
        read_hunk_lines(cur, &mut hunk, opts)?;
        fp.hunks.push(hunk);
        if fp.hunks.len() > MAX_HUNKS {
            return Err(Error::TooMany);
        }
    }
    Ok(())
}

fn read_hunk_lines(cur: &mut Cur<'_>, h: &mut Hunk, opts: &ParseOptions) -> Result<()> {
    let (mut old_left, mut new_left) = (h.old_len, h.new_len);
    let (mut old_seen, mut new_seen) = (0usize, 0usize);
    loop {
        if !opts.recount && old_left == 0 && new_left == 0 {
            break;
        }
        let Some(l) = cur.peek() else {
            if opts.recount {
                break;
            }
            return Err(Error::Corrupt(cur.line_no()));
        };
        let op = match l.first() {
            Some(b' ') => LineOp::Context,
            Some(b'\n') | None => LineOp::Context,
            Some(b'-') => LineOp::Del,
            Some(b'+') => LineOp::Add,
            Some(b'\\') => {
                // "\ No newline at end of file" applies to the previous line.
                let last = h.lines.last_mut().ok_or(Error::Corrupt(cur.line_no()))?;
                if last.text.last() == Some(&b'\n') {
                    last.text.pop();
                }
                cur.next();
                continue;
            }
            _ => {
                if opts.recount {
                    break;
                }
                return Err(Error::Corrupt(cur.line_no()));
            }
        };
        if opts.recount {
            if l.starts_with(b"@@ -") || l.starts_with(b"diff --git ") {
                break;
            }
            if l.starts_with(b"--- ")
                && next_line_after(cur).is_some_and(|n| n.starts_with(b"+++ "))
            {
                break;
            }
        }
        let text = match op {
            LineOp::Context if l.first() != Some(&b' ') => l.to_vec(), // a bare empty line
            _ => l[1..].to_vec(),
        };
        match op {
            LineOp::Context => {
                if !opts.recount && (old_left == 0 || new_left == 0) {
                    return Err(Error::Corrupt(cur.line_no()));
                }
                old_left = old_left.saturating_sub(1);
                new_left = new_left.saturating_sub(1);
                old_seen += 1;
                new_seen += 1;
            }
            LineOp::Del => {
                if !opts.recount && old_left == 0 {
                    return Err(Error::Corrupt(cur.line_no()));
                }
                old_left = old_left.saturating_sub(1);
                old_seen += 1;
            }
            LineOp::Add => {
                if !opts.recount && new_left == 0 {
                    return Err(Error::Corrupt(cur.line_no()));
                }
                new_left = new_left.saturating_sub(1);
                new_seen += 1;
            }
        }
        h.lines.push(HunkLine { op, text });
        cur.next();
    }
    // A trailing "\ No newline" marker belongs to the last line read.
    while cur.peek().is_some_and(|l| l.starts_with(b"\\")) {
        if let Some(last) = h.lines.last_mut()
            && last.text.last() == Some(&b'\n')
        {
            last.text.pop();
        }
        cur.next();
    }
    if opts.recount {
        h.old_len = old_seen;
        h.new_len = new_seen;
    }
    Ok(())
}

const B85: &[u8; 85] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";

fn b85_value(c: u8) -> Option<u32> {
    B85.iter().position(|&x| x == c).map(|i| i as u32)
}

/// Decodes one length-prefixed base85 line into `out`.
fn decode_b85_line(line: &[u8], out: &mut Vec<u8>) -> Option<()> {
    let (&first, body) = line.split_first()?;
    let len = match first {
        b'A'..=b'Z' => (first - b'A') as usize + 1,
        b'a'..=b'z' => (first - b'a') as usize + 27,
        _ => return None,
    };
    if body.len() != len.div_ceil(4) * 5 {
        return None;
    }
    let start = out.len();
    for chunk in body.chunks(5) {
        let mut acc: u64 = 0;
        for &c in chunk {
            acc = acc * 85 + u64::from(b85_value(c)?);
        }
        if acc > u64::from(u32::MAX) {
            return None;
        }
        out.extend_from_slice(&(acc as u32).to_be_bytes());
    }
    out.truncate(start + len);
    Some(())
}

fn parse_binary(cur: &mut Cur<'_>) -> Result<BinaryPatch> {
    let forward = parse_binary_body(cur)?;
    let reverse = match cur.peek() {
        Some(l) if l.starts_with(b"literal ") || l.starts_with(b"delta ") => {
            Some(parse_binary_body(cur)?)
        }
        _ => None,
    };
    Ok(BinaryPatch { forward, reverse })
}

fn parse_binary_body(cur: &mut Cur<'_>) -> Result<BinaryBody> {
    let head = cur.next().ok_or(Error::Corrupt(cur.line_no()))?;
    let head = trim_eol(head);
    let (kind, size) = if let Some(n) = head.strip_prefix(b"literal ") {
        (BinaryKind::Literal, n)
    } else if let Some(n) = head.strip_prefix(b"delta ") {
        (BinaryKind::Delta, n)
    } else {
        return Err(Error::Corrupt(cur.line_no()));
    };
    let size: usize = std::str::from_utf8(size)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or(Error::Corrupt(cur.line_no()))?;
    if size > MAX_APPLY_BYTES {
        return Err(Error::TooLarge);
    }
    let mut deflated = Vec::new();
    while let Some(l) = cur.peek() {
        let l = trim_eol(l);
        if l.is_empty() {
            cur.next();
            break;
        }
        decode_b85_line(l, &mut deflated).ok_or(Error::Corrupt(cur.line_no()))?;
        if deflated.len() > MAX_APPLY_BYTES {
            return Err(Error::TooLarge);
        }
        cur.next();
    }
    Ok(BinaryBody {
        kind,
        size,
        deflated,
    })
}

// ------------------------------------------------------------------- applying

/// One line of the file being patched: a span of the base or of the arena of added lines.
#[derive(Clone, Copy)]
struct Span {
    arena: bool,
    start: usize,
    end: usize,
}

fn split_lines(data: &[u8]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut s = 0;
    while s < data.len() {
        let e = memchr::memchr(b'\n', &data[s..]).map_or(data.len(), |i| s + i + 1);
        out.push(Span {
            arena: false,
            start: s,
            end: e,
        });
        s = e;
    }
    out
}

/// Where each hunk landed, for `-v` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// 1-based hunk number.
    pub hunk: usize,
    /// 1-based line the hunk applied at.
    pub line: usize,
    /// Distance from the line the header named.
    pub offset: isize,
}

/// The result of applying a text patch.
#[derive(Debug, PartialEq, Eq)]
pub struct Applied {
    pub content: Vec<u8>,
    pub placements: Vec<Placement>,
}

/// Options for placing hunks.
#[derive(Clone, Copy, Debug, Default)]
pub struct ApplyOptions {
    /// Hunks without context are placed by their line numbers alone (`--unidiff-zero`).
    pub unidiff_zero: bool,
}

/// The work one apply session may spend placing hunks, counted in line comparisons. A session is
/// one run of a command over all its inputs (`asc apply`, one `asc am` run, one server call): make
/// one [`Work`] for it and pass it to every [`apply_text`] and [`apply_file`] call, including a
/// retry, so that many file patches share one budget instead of each having their own.
#[derive(Debug)]
pub struct Work {
    limit: u64,
    used: Cell<u64>,
}

impl Default for Work {
    /// A session budget of [`MAX_MATCH_WORK`].
    fn default() -> Self {
        Self::new(MAX_MATCH_WORK)
    }
}

impl Work {
    /// A budget of `limit` units.
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            used: Cell::new(0),
        }
    }

    /// Units spent so far.
    pub fn used(&self) -> u64 {
        self.used.get()
    }

    /// The most units the session may spend.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    fn spend(&self, units: u64) -> Result<()> {
        let used = self.used.get().saturating_add(units);
        self.used.set(used);
        if used > self.limit {
            return Err(Error::TooExpensive);
        }
        Ok(())
    }

    /// Charges one comparison of lines `len` bytes long.
    fn charge(&self, len: usize) -> Result<()> {
        self.spend(1 + (len as u64 >> 6))
    }
}

/// Applies the hunks of `fp` to `base`, charging the session's `work`.
pub fn apply_text(base: &[u8], fp: &FilePatch, opts: ApplyOptions, work: &Work) -> Result<Applied> {
    if base.len() > MAX_APPLY_BYTES {
        return Err(Error::FileTooLarge);
    }
    let path = || String::from_utf8_lossy(fp.target().unwrap_or_default()).into_owned();
    let mut lines = split_lines(base);
    if (lines.len() as u128 + 1) * (fp.hunks.len() as u128 + 1) > MAX_SPLICE_WORK {
        return Err(Error::TooExpensive);
    }
    let mut arena: Vec<u8> = Vec::new();
    let mut placements = Vec::new();
    let mut delta: isize = 0;
    for (n, h) in fp.hunks.iter().enumerate() {
        let pre: Vec<&[u8]> = h
            .lines
            .iter()
            .filter(|l| l.op != LineOp::Add)
            .map(|l| l.text.as_slice())
            .collect();
        let match_beginning = h.old_start == 0 || (h.old_start == 1 && !opts.unidiff_zero);
        let match_end = !opts.unidiff_zero && h.trailing() == 0;
        // The 0-based line the preimage starts at, as the header says (adjusted for earlier hunks).
        let want = if h.old_len == 0 {
            h.old_start as isize
        } else {
            h.old_start as isize - 1
        } + delta;
        let want = want.max(0) as usize;
        let at = find_pos(
            base,
            &arena,
            &lines,
            &pre,
            (want, match_beginning, match_end),
            work,
        )?
        .ok_or_else(|| Error::HunkFailed {
            path: path(),
            line: h.old_start,
        })?;
        let mut repl: Vec<Span> = Vec::with_capacity(h.new_len);
        let mut pre_i = 0usize;
        for l in &h.lines {
            match l.op {
                LineOp::Context => {
                    repl.push(lines[at + pre_i]);
                    pre_i += 1;
                }
                LineOp::Del => pre_i += 1,
                LineOp::Add => {
                    if arena.len() + l.text.len() > MAX_APPLY_BYTES {
                        return Err(Error::FileTooLarge);
                    }
                    let s = arena.len();
                    arena.extend_from_slice(&l.text);
                    repl.push(Span {
                        arena: true,
                        start: s,
                        end: arena.len(),
                    });
                }
            }
        }
        let new_len = repl.len();
        // Moving the tail of the line list is work too.
        work.spend(1 + (lines.len() as u64 >> 6))?;
        lines.splice(at..at + pre.len(), repl);
        delta += new_len as isize - pre.len() as isize;
        placements.push(Placement {
            hunk: n + 1,
            line: at + 1,
            offset: at as isize - want as isize,
        });
    }
    let mut out = Vec::with_capacity(base.len());
    for s in &lines {
        let src = if s.arena { &arena[..] } else { base };
        out.extend_from_slice(&src[s.start..s.end]);
        if out.len() > MAX_APPLY_BYTES {
            return Err(Error::FileTooLarge);
        }
    }
    Ok(Applied {
        content: out,
        placements,
    })
}

fn span_bytes<'a>(base: &'a [u8], arena: &'a [u8], s: &Span) -> &'a [u8] {
    if s.arena {
        &arena[s.start..s.end]
    } else {
        &base[s.start..s.end]
    }
}

fn matches_at(
    base: &[u8],
    arena: &[u8],
    lines: &[Span],
    pre: &[&[u8]],
    at: usize,
    work: &Work,
) -> Result<bool> {
    if at + pre.len() > lines.len() {
        return Ok(false);
    }
    for (i, p) in pre.iter().enumerate() {
        work.charge(p.len())?;
        if span_bytes(base, arena, &lines[at + i]) != *p {
            return Ok(false);
        }
    }
    Ok(true)
}

/// git's `find_pos`: the preimage must match in full; nearest to `want` wins, earlier lines
/// before later ones at equal distance. `place` is `(want, match_beginning, match_end)`. The
/// comparisons are charged to `work`; past its limit the search stops with `TooExpensive`.
fn find_pos(
    base: &[u8],
    arena: &[u8],
    lines: &[Span],
    pre: &[&[u8]],
    place: (usize, bool, bool),
    work: &Work,
) -> Result<Option<usize>> {
    let (want, match_beginning, match_end) = place;
    let Some(max_start) = lines.len().checked_sub(pre.len()) else {
        return Ok(None);
    };
    if match_beginning {
        // With match_end too, the preimage must be the whole file.
        let ok = (!match_end || pre.len() == lines.len())
            && matches_at(base, arena, lines, pre, 0, work)?;
        return Ok(ok.then_some(0));
    }
    if match_end {
        return Ok(matches_at(base, arena, lines, pre, max_start, work)?.then_some(max_start));
    }
    let start = want.min(max_start);
    let (mut back, mut fwd) = (start, start);
    loop {
        if matches_at(base, arena, lines, pre, back, work)? {
            return Ok(Some(back));
        }
        if matches_at(base, arena, lines, pre, fwd, work)? {
            return Ok(Some(fwd));
        }
        if back == 0 && fwd == max_start {
            return Ok(None);
        }
        back = back.saturating_sub(1);
        if fwd < max_start {
            fwd += 1;
        }
    }
}

/// What applying one file patch produced.
#[derive(Debug, PartialEq, Eq)]
pub struct FileResult {
    /// The new content; `None` when the file is deleted.
    pub content: Option<Vec<u8>>,
    pub placements: Vec<Placement>,
}

/// Applies a text file patch to the file's current content (`None` when it does not exist),
/// with git's existence rules: a new file must not exist, others must.
pub fn apply_file(
    base: Option<&[u8]>,
    fp: &FilePatch,
    opts: ApplyOptions,
    work: &Work,
) -> Result<FileResult> {
    let name = || String::from_utf8_lossy(fp.target().unwrap_or_default()).into_owned();
    let src = String::from_utf8_lossy(fp.source().unwrap_or_default()).into_owned();
    match (fp.is_new, base) {
        (true, Some(_)) if !fp.is_rename && !fp.is_copy => return Err(Error::Exists(name())),
        (false, None) => return Err(Error::Missing(src)),
        _ => {}
    }
    let applied = apply_text(base.unwrap_or_default(), fp, opts, work)?;
    if fp.is_delete {
        if !applied.content.is_empty() {
            return Err(Error::RemovalLeavesContent);
        }
        return Ok(FileResult {
            content: None,
            placements: applied.placements,
        });
    }
    Ok(FileResult {
        content: Some(applied.content),
        placements: applied.placements,
    })
}

/// A zlib inflater bounded by a size.
pub type Inflate = dyn Fn(&[u8], usize) -> Option<Vec<u8>>;

/// Applies a binary patch. `inflate` turns a zlib stream into at most `size` bytes.
pub fn apply_binary(base: Option<&[u8]>, body: &BinaryBody, inflate: &Inflate) -> Result<Vec<u8>> {
    let data = inflate(&body.deflated, body.size).ok_or(Error::BinaryFailed)?;
    if data.len() != body.size {
        return Err(Error::BinaryFailed);
    }
    match body.kind {
        BinaryKind::Literal => Ok(data),
        BinaryKind::Delta => {
            apply_delta(base.ok_or(Error::BinaryFailed)?, &data).ok_or(Error::BinaryFailed)
        }
    }
}

fn delta_size(d: &[u8], i: &mut usize) -> Option<usize> {
    let (mut v, mut shift) = (0usize, 0u32);
    loop {
        let c = *d.get(*i)?;
        *i += 1;
        v |= usize::from(c & 0x7f).checked_shl(shift)?;
        if c & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 56 {
            return None;
        }
    }
}

/// git's `patch_delta`.
fn apply_delta(base: &[u8], d: &[u8]) -> Option<Vec<u8>> {
    let mut i = 0;
    if delta_size(d, &mut i)? != base.len() {
        return None;
    }
    let dst = delta_size(d, &mut i)?;
    if dst > MAX_APPLY_BYTES {
        return None;
    }
    let mut out = Vec::with_capacity(dst.min(1 << 26));
    while i < d.len() {
        let cmd = d[i];
        i += 1;
        if cmd & 0x80 != 0 {
            let (mut off, mut size) = (0usize, 0usize);
            for bit in 0..4 {
                if cmd & (1 << bit) != 0 {
                    off |= usize::from(*d.get(i)?) << (8 * bit);
                    i += 1;
                }
            }
            for bit in 0..3 {
                if cmd & (0x10 << bit) != 0 {
                    size |= usize::from(*d.get(i)?) << (8 * bit);
                    i += 1;
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            out.extend_from_slice(base.get(off..off.checked_add(size)?)?);
        } else if cmd != 0 {
            out.extend_from_slice(d.get(i..i + usize::from(cmd))?);
            i += usize::from(cmd);
        } else {
            return None;
        }
        if out.len() > dst {
            return None;
        }
    }
    (out.len() == dst).then_some(out)
}

/// A clean three-way merge for `--3way`: `base` is the preimage the patch was made against,
/// `ours` the current content, `theirs` the patch applied to `base`. `None` on a conflict.
pub fn merge_clean(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    use gix_merge::blob::Resolution;
    use gix_merge::blob::builtin_driver::text::{self, Conflict, Labels};
    let mut input = gix_imara_diff::InternedInput::default();
    let mut out = Vec::new();
    let res = gix_merge::blob::builtin_driver::text(
        &mut out,
        &mut input,
        Labels::default(),
        ours,
        base,
        theirs,
        text::Options {
            conflict: Conflict::Keep {
                style: text::ConflictStyle::Merge,
                marker_size: std::num::NonZeroU8::new(7)?,
            },
            ..Default::default()
        },
    );
    (res == Resolution::Complete).then_some(out)
}

/// Whether `bytes` looks like text for `--stat`-style decisions: no NUL in the first 8 KiB.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

/// Lossy display of a patch path.
pub fn display(name: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(patch: &str) -> FilePatch {
        let mut v = parse(patch.as_bytes(), &ParseOptions::default()).unwrap();
        assert_eq!(v.len(), 1);
        v.remove(0)
    }

    const MODIFY: &str = "diff --git a/f b/f\nindex 1111111..2222222 100644\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";

    #[test]
    fn parses_and_applies_a_modification() {
        let fp = one(MODIFY);
        assert_eq!(fp.old_path.as_deref(), Some(&b"f"[..]));
        assert_eq!(fp.old_oid.as_deref(), Some(&b"1111111"[..]));
        let r = apply_file(
            Some(b"a\nb\nc\n"),
            &fp,
            ApplyOptions::default(),
            &Work::default(),
        )
        .unwrap();
        assert_eq!(r.content.unwrap(), b"a\nB\nc\n");
    }

    #[test]
    fn finds_a_hunk_at_an_offset() {
        let patch = "--- a/f\n+++ b/f\n@@ -2,3 +2,3 @@\n x\n-y\n+Y\n z\n";
        let fp = one(patch);
        let base = b"0\n1\n2\n3\nx\ny\nz\n";
        let r = apply_text(base, &fp, ApplyOptions::default(), &Work::default()).unwrap();
        assert_eq!(r.content, b"0\n1\n2\n3\nx\nY\nz\n");
        assert_eq!(r.placements[0].offset, 3);
    }

    #[test]
    fn a_mismatch_fails_with_the_hunk_line() {
        let fp = one(MODIFY);
        let e = apply_file(
            Some(b"a\nX\nc\n"),
            &fp,
            ApplyOptions::default(),
            &Work::default(),
        )
        .unwrap_err();
        assert_eq!(
            e,
            Error::HunkFailed {
                path: "f".into(),
                line: 1
            }
        );
    }

    #[test]
    fn creation_and_removal_rules() {
        let new = one(
            "diff --git a/n b/n\nnew file mode 100644\nindex 0000000..e69de29\n--- /dev/null\n+++ b/n\n@@ -0,0 +1,2 @@\n+x\n+y\n",
        );
        assert!(new.is_new);
        assert_eq!(
            apply_file(None, &new, ApplyOptions::default(), &Work::default())
                .unwrap()
                .content
                .unwrap(),
            b"x\ny\n"
        );
        assert_eq!(
            apply_file(Some(b"q"), &new, ApplyOptions::default(), &Work::default()).unwrap_err(),
            Error::Exists("n".into())
        );
        let del = one(
            "diff --git a/n b/n\ndeleted file mode 100644\nindex e69de29..0000000\n--- a/n\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-x\n-y\n",
        );
        assert!(del.is_delete);
        assert_eq!(
            apply_file(
                Some(b"x\ny\n"),
                &del,
                ApplyOptions::default(),
                &Work::default()
            )
            .unwrap()
            .content,
            None
        );
        assert_eq!(
            apply_file(
                Some(b"x\ny\nz\n"),
                &del,
                ApplyOptions::default(),
                &Work::default()
            )
            .unwrap_err(),
            Error::HunkFailed {
                path: "n".into(),
                line: 1
            }
        );
    }

    #[test]
    fn no_newline_markers_are_honoured() {
        let fp = one(
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n",
        );
        let r = apply_text(b"a\nb", &fp, ApplyOptions::default(), &Work::default()).unwrap();
        assert_eq!(r.content, b"a\nc");
        assert!(apply_text(b"a\nb\n", &fp, ApplyOptions::default(), &Work::default()).is_err());
    }

    #[test]
    fn zero_context_needs_unidiff_zero() {
        let fp = one("--- a/f\n+++ b/f\n@@ -2,0 +3,1 @@\n+new\n");
        let base = b"1\n2\n3\n4\n";
        let z = ApplyOptions { unidiff_zero: true };
        assert_eq!(
            apply_text(base, &fp, z, &Work::default()).unwrap().content,
            b"1\n2\nnew\n3\n4\n"
        );
        // Without it a hunk with no trailing context is pinned to the end of the file.
        assert_eq!(
            apply_text(base, &fp, ApplyOptions::default(), &Work::default())
                .unwrap()
                .content,
            b"1
2
3
4
new
"
        );
    }

    #[test]
    fn recount_ignores_wrong_counts() {
        let patch = "--- a/f\n+++ b/f\n@@ -1,9 +1,9 @@\n a\n-b\n+B\n c\n";
        assert!(parse(patch.as_bytes(), &ParseOptions::default()).is_err());
        let fp = parse(
            patch.as_bytes(),
            &ParseOptions {
                strip: 1,
                recount: true,
            },
        )
        .unwrap()
        .remove(0);
        assert_eq!(fp.hunks[0].old_len, 3);
        assert_eq!(
            apply_text(b"a\nb\nc\n", &fp, ApplyOptions::default(), &Work::default())
                .unwrap()
                .content,
            b"a\nB\nc\n"
        );
    }

    #[test]
    fn rename_and_mode_only_patches_parse() {
        let fp = one("diff --git a/x b/y\nsimilarity index 100%\nrename from x\nrename to y\n");
        assert!(fp.is_rename);
        assert_eq!(
            (fp.old_path.as_deref(), fp.new_path.as_deref()),
            (Some(&b"x"[..]), Some(&b"y"[..]))
        );
        let fp = one("diff --git a/s.sh b/s.sh\nold mode 100644\nnew mode 100755\n");
        assert_eq!((fp.old_mode, fp.new_mode), (Some(0o100644), Some(0o100755)));
        assert!(fp.hunks.is_empty());
    }

    fn create(name: &str) -> String {
        format!("--- /dev/null\n+++ b/{name}\n@@ -0,0 +1 @@\n+x\n")
    }

    const ALIASES: [&str; 13] = [
        "../x",
        "a/../x",
        ".git/config",
        "a/.GIT/x",
        r"a\b",
        ".git /config",
        ".git./hooks/x",
        "GIT~1/config",
        "git~1/config",
        "a/.git:stream",
        ".git::$INDEX_ALLOCATION/x",
        ".asc/info/x",
        ".g\u{200c}it/config",
    ];

    #[test]
    fn unsafe_paths_are_refused() {
        for name in ALIASES {
            let r = parse(create(name).as_bytes(), &ParseOptions::default());
            assert!(matches!(r, Err(Error::UnsafePath(_))), "{name}: {r:?}");
        }
        let abs = "--- /dev/null
+++ /etc/passwd
@@ -0,0 +1 @@
+x
";
        let r = parse(
            abs.as_bytes(),
            &ParseOptions {
                strip: 0,
                recount: false,
            },
        );
        assert!(matches!(r, Err(Error::UnsafePath(_))), "{r:?}");
        let r = parse(create(".asc").as_bytes(), &ParseOptions::default());
        assert!(matches!(r, Err(Error::UnsafePath(_))), "{r:?}");
        assert!(path_allowed(b"a/b.txt"));
        assert!(path_allowed(b"a/.gitignore"));
    }

    #[test]
    fn every_name_a_patch_writes_is_checked() {
        for name in ALIASES {
            let shapes = [
                // A rename into the alias, and out of it.
                format!("diff --git a/x b/{name}\nrename from x\nrename to {name}\n"),
                format!("diff --git a/{name} b/x\nrename from {name}\nrename to x\n"),
                // A copy, a deletion, a mode change.
                format!("diff --git a/x b/{name}\ncopy from x\ncopy to {name}\n"),
                format!(
                    "diff --git a/{name} b/{name}\ndeleted file mode 100644\n--- a/{name}\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n"
                ),
                format!("diff --git a/{name} b/{name}\nold mode 100644\nnew mode 100755\n"),
                // A later file of a patch that creates a directory first, and a file inside
                // the directory the same patch creates.
                format!("{}{}", create("d/e/f"), create(name)),
                format!("{}{}", create("d/e/f"), create(&format!("{name}/g"))),
                // A traditional diff, with no `diff --git` line.
                format!("--- a/x\n+++ b/{name}\n@@ -1 +1 @@\n-x\n+y\n"),
            ];
            for p in shapes {
                let r = parse(p.as_bytes(), &ParseOptions::default());
                assert!(matches!(r, Err(Error::UnsafePath(_))), "{name}: {p}: {r:?}");
            }
        }
    }

    #[test]
    fn symlinks_may_not_leave_the_tree_or_name_dotgit() {
        let link = |name: &str, target: &str| {
            format!(
                "diff --git a/{name} b/{name}\nnew file mode 120000\n--- /dev/null\n+++ b/{name}\n@@ -0,0 +1 @@\n+{target}\n\\ No newline at end of file\n"
            )
        };
        for (name, target) in [
            ("l", "../outside"),
            ("a/l", "../../outside"),
            ("l", "/etc/passwd"),
            ("l", "C:/x"),
            ("l", r"a\..\b"),
            ("l", ".git/hooks"),
            ("l", "d/.git /hooks"),
            ("l", "GIT~1"),
            ("a/l", "../.git/config"),
            ("l", "a/../../x"),
            ("l", ".asc/info/exclude"),
            ("l", ""),
        ] {
            let r = parse(link(name, target).as_bytes(), &ParseOptions::default());
            assert!(
                matches!(r, Err(Error::UnsafePath(_))),
                "{name} -> {target}: {r:?}"
            );
        }
        for (name, target) in [
            ("l", "x"),
            ("a/l", "../x"),
            ("a/b/l", "../../c/./d"),
            ("l", "."),
        ] {
            let r = parse(link(name, target).as_bytes(), &ParseOptions::default());
            assert!(r.is_ok(), "{name} -> {target}: {r:?}");
        }
    }

    #[test]
    fn a_large_preimage_over_a_large_file_is_too_expensive() {
        // Every position matches `m - 2` lines and then fails: `n x m` work.
        let (n, m) = (4_000usize, 2_000usize);
        let base = "a\n".repeat(n).into_bytes();
        let mut hunk = format!("--- a/f\n+++ b/f\n@@ -1000,{m} +1000,{m} @@\n");
        for _ in 0..m - 2 {
            hunk.push_str(" a\n");
        }
        hunk.push_str("-b\n+c\n a\n");
        let fp = one(&hunk);
        let work = Work::new(1_000_000);
        let r = apply_text(&base, &fp, ApplyOptions::default(), &work);
        assert_eq!(r, Err(Error::TooExpensive));
        // It stopped at the limit instead of doing the n x m comparisons.
        assert!(work.used() > work.limit(), "{}", work.used());
        assert!(work.used() <= work.limit() + 2, "{}", work.used());
        assert!((work.used() as usize) < n * m / 4);
        // A patch that matches at once costs about its own length.
        let ok = one("--- a/f
+++ b/f
@@ -1,3 +1,3 @@
 a
-a
+b
 a
");
        let work = Work::new(1_000_000);
        apply_text(&base, &ok, ApplyOptions::default(), &work).unwrap();
        // Three comparisons, and moving the tail of the line list.
        assert!(work.used() <= 3 + 1 + (n as u64 >> 6), "{}", work.used());
    }

    #[test]
    fn many_files_share_one_budget() {
        // A file patch that succeeds, but only after a long search: about `n x m` work.
        let (n, m) = (3_000usize, 500usize);
        let mut base = "a
"
        .repeat(n);
        base.push_str(
            "b
a
",
        );
        let mut hunk = format!(
            "--- a/f
+++ b/f
@@ -2,{0} +2,{0} @@
",
            m + 1
        );
        for _ in 0..m - 1 {
            hunk.push_str(
                " a
",
            );
        }
        hunk.push_str(
            "-b
+c
 a
",
        );
        let fp = one(&hunk);
        let opts = ApplyOptions::default();
        let probe = Work::new(u64::MAX);
        apply_text(base.as_bytes(), &fp, opts, &probe).unwrap();
        let one_file = probe.used();
        assert!(one_file > 100_000, "{one_file}");
        // Room for two files and a half: each file is under the budget on its own.
        let limit = one_file * 5 / 2;
        for _ in 0..5 {
            apply_text(base.as_bytes(), &fp, opts, &Work::new(limit)).unwrap();
        }
        // The same files in one session run out of budget at the third.
        let session = Work::new(limit);
        let mut results = Vec::new();
        for _ in 0..3 {
            results.push(apply_text(base.as_bytes(), &fp, opts, &session).map(|_| ()));
        }
        assert_eq!(results[0], Ok(()));
        assert_eq!(results[1], Ok(()));
        assert_eq!(results[2], Err(Error::TooExpensive));
        assert!(session.used() <= limit + 2, "{}", session.used());
    }

    #[test]
    fn quoted_names_decode() {
        let fp = one(
            "diff --git \"a/f\\303\\251\" \"b/f\\303\\251\"\nnew file mode 100644\n--- /dev/null\n+++ \"b/f\\303\\251\"\n@@ -0,0 +1 @@\n+x\n",
        );
        assert_eq!(fp.new_path.as_deref(), Some("f\u{e9}".as_bytes()));
    }

    #[test]
    fn reversal_undoes_a_patch() {
        let fp = one(MODIFY).reversed().unwrap();
        let r = apply_text(b"a\nB\nc\n", &fp, ApplyOptions::default(), &Work::default()).unwrap();
        assert_eq!(r.content, b"a\nb\nc\n");
    }

    #[test]
    fn leading_text_and_several_files_parse() {
        let text = format!(
            "From: x\nSubject: s\n\nbody\n---\n f | 2 +-\n\n{MODIFY}diff --git a/g b/g\nold mode 100644\nnew mode 100755\n-- \n2.0\n"
        );
        let v = parse(text.as_bytes(), &ParseOptions::default()).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].new_mode, Some(0o100755));
    }

    #[test]
    fn delta_applies() {
        // copy 3 bytes from offset 0, insert "XY".
        let base = b"abcdef";
        let delta = [6u8, 5, 0x90, 3, 2, b'X', b'Y'];
        assert_eq!(apply_delta(base, &delta).unwrap(), b"abcXY");
        assert!(apply_delta(b"abc", &delta).is_none());
    }

    fn b85_line(data: &[u8]) -> Vec<u8> {
        let n = data.len();
        let mut line = vec![if n <= 26 {
            b'A' + (n as u8 - 1)
        } else {
            b'a' + (n as u8 - 27)
        }];
        for c in data.chunks(4) {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            let mut acc = u32::from_be_bytes(w);
            let mut d = [0u8; 5];
            for x in d.iter_mut().rev() {
                *x = B85[(acc % 85) as usize];
                acc /= 85;
            }
            line.extend_from_slice(&d);
        }
        line.push(b'\n');
        line
    }

    #[test]
    fn base85_lines_decode() {
        let line = b85_line(b"hello");
        let mut out = Vec::new();
        decode_b85_line(trim_eol(&line), &mut out).unwrap();
        assert_eq!(out, b"hello");
        assert!(decode_b85_line(b"E0000", &mut out).is_none());
    }

    #[test]
    fn binary_patch_body_parses() {
        let mut text = b"diff --git a/b.bin b/b.bin\nnew file mode 100644\nindex 0000000..1111111\nGIT binary patch\nliteral 5\n".to_vec();
        text.extend(b85_line(b"hello"));
        text.extend(b"\nliteral 0\n");
        text.extend(b85_line(b"x"));
        text.extend(b"\n");
        let fp = parse(&text, &ParseOptions::default()).unwrap().remove(0);
        let b = fp.binary.as_ref().unwrap();
        assert_eq!(b.forward.kind, BinaryKind::Literal);
        assert_eq!(b.forward.size, 5);
        assert_eq!(b.forward.deflated, b"hello");
        assert!(b.reverse.is_some());
        let got = apply_binary(None, &b.forward, &|d, _| Some(d.to_vec())).unwrap();
        assert_eq!(got, b"hello");
    }

    /// Patches our own writer produces over the golden corpus apply back to the new side.
    #[tokio::test]
    async fn own_patches_round_trip() {
        use crate::diff::tests_support::MemSource;
        use crate::diff::{DiffCtx, DiffOptions, NoAttributes, write_patch};
        use asc_core::git::blob_oid;
        use asc_core::id::FileId;
        use asc_core::object::{Entry, Kind};
        use asc_core::path::{Name, RepoPath};

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/diff_golden/pairs");
        let entry = |c: &[u8]| Entry {
            name: Name::new("file").unwrap(),
            kind: Kind::File,
            id: *FileId::of(c).as_bytes(),
            size: c.len() as u64,
            git: blob_oid(c),
        };
        let mut checked = 0;
        for e in std::fs::read_dir(&dir).unwrap() {
            let name = e.unwrap().file_name().into_string().unwrap();
            let Some(id) = name.strip_suffix(".old") else {
                continue;
            };
            let old = std::fs::read(dir.join(format!("{id}.old"))).unwrap();
            let new = std::fs::read(dir.join(format!("{id}.new"))).unwrap();
            if looks_binary(&old) || looks_binary(&new) || old == new {
                continue;
            }
            let mut src = MemSource::default();
            src.add(&old);
            src.add(&new);
            let path = RepoPath::new("file").unwrap();
            let change = crate::diff::TreeChange {
                old_path: path.clone(),
                path,
                old: Some(entry(&old)),
                new: Some(entry(&new)),
                status: crate::diff::Status::Modified,
            };
            let opts = DiffOptions::default();
            let ctx = DiffCtx {
                opts: &opts,
                content: &src,
                attrs: &NoAttributes,
            };
            let mut patch = Vec::new();
            write_patch(&mut patch, &ctx, &[change]).await.unwrap();
            let fp = parse(&patch, &ParseOptions::default()).unwrap().remove(0);
            let got = apply_file(Some(&old), &fp, ApplyOptions::default(), &Work::default())
                .unwrap()
                .content
                .unwrap();
            assert!(got == new, "{id}: round trip differs");
            let back = fp.reversed().unwrap();
            let again = apply_file(Some(&new), &back, ApplyOptions::default(), &Work::default())
                .unwrap()
                .content
                .unwrap();
            assert!(again == old, "{id}: reverse differs");
            checked += 1;
        }
        assert!(checked > 0);
    }
}
