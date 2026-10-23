//! Git object rendering and parsing (SPEC §2.9).
//!
//! Pure functions over bytes. Bodies are returned without the `<kind> <len>\0`
//! header; [`object_oid`] adds it when hashing.

use data_encoding::BASE64;
use sha1::{Digest, Sha1};

use crate::id::{ChangesetId, GitOid, TreeId};
use crate::object::{AscGitExtra, Change, Changeset, Kind, ObjectError, SortedMap, TagRecord, Tree};
use crate::path::RepoPath;
use crate::sig::{SigError, Signature};

/// Extra key holding the raw bytes of a non-canonical git commit.
pub const EXTRA_GIT_COMMIT: &[u8] = b"git.commit";
/// Extra key holding a commit's `gpgsig` value, or a tag's trailing signature block.
pub const EXTRA_GIT_GPGSIG: &[u8] = b"git.gpgsig";
/// Extra key holding the raw bytes of a non-canonical git tag.
pub const EXTRA_GIT_TAG: &[u8] = b"git.tag";
/// Extra key marking a snapshot changeset.
pub const EXTRA_SNAPSHOT: &[u8] = b"asc.snapshot";

/// Why git bytes could not be parsed or canonicalised.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GitError {
    #[error("malformed commit: {0}")]
    Commit(&'static str),
    #[error("malformed tag: {0}")]
    Tag(&'static str),
    #[error("commit has {found} parents, caller supplied {expected}")]
    Parents { expected: usize, found: usize },
    #[error("blob size mismatch: declared {declared}, hashed {hashed}")]
    BlobSize { declared: u64, hashed: u64 },
    #[error(transparent)]
    Object(#[from] ObjectError),
    #[error(transparent)]
    Signature(#[from] SigError),
    #[error("raw git bytes do not match the changeset: {0}")]
    RawMismatch(&'static str),
}

/// Git object type, for hashing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectKind {
    fn name(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
            Self::Tag => "tag",
        }
    }
}

fn start(kind: ObjectKind, len: u64) -> Sha1 {
    let mut h = Sha1::new();
    h.update(format!("{} {len}\0", kind.name()).as_bytes());
    h
}

fn finish(h: Sha1) -> GitOid {
    GitOid::from_bytes(h.finalize().into())
}

/// `sha1("<kind> <len>\0" ‖ body)`.
pub fn object_oid(kind: ObjectKind, body: &[u8]) -> GitOid {
    let mut h = start(kind, body.len() as u64);
    h.update(body);
    finish(h)
}

/// Git blob oid of `bytes`.
pub fn blob_oid(bytes: &[u8]) -> GitOid {
    object_oid(ObjectKind::Blob, bytes)
}

/// Streaming blob oid. The size must be known up front (the header needs it).
pub struct BlobOidHasher {
    sha: Sha1,
    declared: u64,
    seen: u64,
}

impl BlobOidHasher {
    pub fn new(size: u64) -> Self {
        Self {
            sha: start(ObjectKind::Blob, size),
            declared: size,
            seen: 0,
        }
    }

    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.sha.update(bytes);
        self.seen += bytes.len() as u64;
        self
    }

    /// Fails if the bytes fed in do not add up to the declared size.
    pub fn finalize(self) -> Result<GitOid, GitError> {
        if self.seen != self.declared {
            return Err(GitError::BlobSize {
                declared: self.declared,
                hashed: self.seen,
            });
        }
        Ok(finish(self.sha))
    }
}

// ---------------------------------------------------------------- trees

fn mode(kind: Kind) -> &'static [u8] {
    match kind {
        Kind::File => b"100644",
        Kind::Exec => b"100755",
        Kind::Symlink => b"120000",
        Kind::Dir => b"40000",
        Kind::Gitlink => b"160000",
    }
}

/// Git tree body: entries in git order (directories compare as `name/`).
pub fn render_tree(tree: &Tree) -> Vec<u8> {
    let mut order: Vec<_> = tree.entries().iter().collect();
    order.sort_by(|a, b| {
        sort_key(a.kind, a.name.as_bytes()).cmp(sort_key(b.kind, b.name.as_bytes()))
    });
    let mut out = Vec::new();
    for e in order {
        out.extend_from_slice(mode(e.kind));
        out.push(b' ');
        out.extend_from_slice(e.name.as_bytes());
        out.push(0);
        out.extend_from_slice(e.git.as_bytes());
    }
    out
}

fn sort_key(kind: Kind, name: &[u8]) -> impl Iterator<Item = u8> + '_ {
    name.iter()
        .copied()
        .chain((kind == Kind::Dir).then_some(b'/'))
}

/// Git oid of the rendered tree.
pub fn tree_oid(tree: &Tree) -> GitOid {
    object_oid(ObjectKind::Tree, &render_tree(tree))
}

// ----------------------------------------------------------- time zones

/// `+HHMM` / `-HHMM` from minutes east of UTC (valid for |tz| <= 99:59).
pub fn tz_to_string(tz_min: i16) -> String {
    let sign = if tz_min < 0 { '-' } else { '+' };
    let m = tz_min.unsigned_abs();
    format!("{sign}{:02}{:02}", m / 60, m % 60)
}

/// Inverse of [`tz_to_string`]; `None` unless exactly `[+-]HHMM` with MM < 60.
pub fn parse_tz(s: &[u8]) -> Option<i16> {
    let [sign @ (b'+' | b'-'), d @ ..] = s else {
        return None;
    };
    if d.len() != 4 || !d.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = |i: usize| i16::from(d[i] - b'0');
    let (h, m) = (n(0) * 10 + n(1), n(2) * 10 + n(3));
    if m >= 60 {
        return None;
    }
    let v = h * 60 + m;
    Some(if *sign == b'-' { -v } else { v })
}

fn render_signature(out: &mut Vec<u8>, s: &Signature) {
    out.extend_from_slice(s.name());
    out.extend_from_slice(b" <");
    out.extend_from_slice(s.email());
    out.extend_from_slice(format!("> {} {}", s.time(), tz_to_string(s.tz_min())).as_bytes());
}

fn parse_signature(b: &[u8], what: &'static str) -> Result<Signature, &'static str> {
    let gt = b.iter().rposition(|&c| c == b'>').ok_or(what)?;
    let lt = b[..gt].iter().rposition(|&c| c == b'<').ok_or(what)?;
    let name = &b[..lt];
    let name = name.strip_suffix(b" ").unwrap_or(name);
    let rest = b[gt + 1..].strip_prefix(b" ").ok_or(what)?;
    let sp = rest.iter().position(|&c| c == b' ').ok_or(what)?;
    let time = std::str::from_utf8(&rest[..sp])
        .ok()
        .and_then(|t| t.parse().ok())
        .ok_or(what)?;
    let tz_min = parse_tz(&rest[sp + 1..]).ok_or(what)?;
    Signature::new(name.to_vec(), b[lt + 1..gt].to_vec(), time, tz_min).map_err(|_| what)
}

// -------------------------------------------------------------- headers

fn push_header(out: &mut Vec<u8>, key: &str, value: &[u8]) {
    out.extend_from_slice(key.as_bytes());
    out.push(b' ');
    for (i, line) in value.split(|&c| c == b'\n').enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\n ");
        }
        out.extend_from_slice(line);
    }
    out.push(b'\n');
}

type Headers<'a> = Vec<(&'a [u8], Vec<u8>)>;

/// Splits an object body into headers (continuation lines joined with `\n`) and the message.
fn split_headers(raw: &[u8]) -> (Headers<'_>, &[u8]) {
    let mut headers: Headers = Vec::new();
    let mut pos = 0;
    while pos < raw.len() {
        let end = raw[pos..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(raw.len(), |i| pos + i);
        let line = &raw[pos..end];
        pos = (end + 1).min(raw.len());
        if line.is_empty() {
            return (headers, &raw[pos..]);
        }
        match (line.strip_prefix(b" "), headers.last_mut()) {
            (Some(cont), Some((_, v))) => {
                v.push(b'\n');
                v.extend_from_slice(cont);
            }
            _ => {
                let sp = line.iter().position(|&c| c == b' ').unwrap_or(line.len());
                headers.push((&line[..sp], line.get(sp + 1..).unwrap_or_default().to_vec()));
            }
        }
    }
    (headers, &[])
}

fn parse_oid(v: &[u8], what: &'static str) -> Result<GitOid, &'static str> {
    std::str::from_utf8(v)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or(what)
}

fn oid_hex(oid: GitOid) -> Vec<u8> {
    oid.to_string().into_bytes()
}

// -------------------------------------------------------------- commits

fn extra_of(cs: &Changeset) -> AscGitExtra {
    let copies = cs
        .changes
        .as_slice()
        .iter()
        .filter_map(|(p, c)| match c {
            Change::Write {
                copy_from: Some(from),
                ..
            } => Some((p.clone(), from.clone())),
            _ => None,
        })
        .collect();
    AscGitExtra {
        copies: SortedMap::new(copies).expect("changes are sorted"),
        snapshot: cs.extra.get(&EXTRA_SNAPSHOT.to_vec()).is_some(),
    }
}

/// Canonical git commit body (ignores any `git.commit` extra).
pub fn render_commit(cs: &Changeset, root_git: GitOid, parent_gits: &[GitOid]) -> Vec<u8> {
    let mut out = Vec::new();
    push_header(&mut out, "tree", &oid_hex(root_git));
    for p in parent_gits {
        push_header(&mut out, "parent", &oid_hex(*p));
    }
    for (key, sig) in [("author", &cs.author), ("committer", &cs.committer)] {
        out.extend_from_slice(key.as_bytes());
        out.push(b' ');
        render_signature(&mut out, sig);
        out.push(b'\n');
    }
    let extra = extra_of(cs);
    if !extra.copies.is_empty() || extra.snapshot {
        push_header(&mut out, "asc", BASE64.encode(&extra.encode()).as_bytes());
    }
    if let Some(sig) = cs.extra.get(&EXTRA_GIT_GPGSIG.to_vec()) {
        push_header(&mut out, "gpgsig", sig);
    }
    out.push(b'\n');
    out.extend_from_slice(&cs.message);
    out
}

/// Checks that `raw` describes the same commit as `cs`: tree, parents (in order),
/// author, committer, message, `gpgsig` and the `asc` header must all match.
pub fn verify_raw_commit(
    raw: &[u8],
    cs: &Changeset,
    root_git: GitOid,
    parent_gits: &[GitOid],
) -> Result<(), GitError> {
    let p = parse_commit(raw)?;
    let bad = GitError::RawMismatch;
    if p.tree != root_git {
        return Err(bad("tree"));
    }
    if p.parents != parent_gits {
        return Err(bad("parents"));
    }
    if p.author != cs.author {
        return Err(bad("author"));
    }
    if p.committer != cs.committer {
        return Err(bad("committer"));
    }
    if p.message != cs.message {
        return Err(bad("message"));
    }
    if p.gpgsig.as_deref() != cs.extra.get(&EXTRA_GIT_GPGSIG.to_vec()).map(Vec::as_slice) {
        return Err(bad("gpgsig"));
    }
    let expected = extra_of(cs);
    let want_asc = (!expected.copies.is_empty() || expected.snapshot).then_some(expected);
    let got_asc = p.asc.filter(|c| !c.copies.is_empty() || c.snapshot);
    if got_asc != want_asc {
        return Err(bad("asc"));
    }
    Ok(())
}

/// The bytes git has for this commit: `git.commit` if present (after
/// [`verify_raw_commit`]), else the canonical render.
///
/// The `git.commit` and `git.tag` extras are only accepted on the import path
/// ([`canonicalise_commit`], [`canonicalise_tag`]); rejecting them on native
/// ingest is asc-engine's job, not enforced here.
pub fn export_commit(
    cs: &Changeset,
    root_git: GitOid,
    parent_gits: &[GitOid],
) -> Result<Vec<u8>, GitError> {
    match cs.extra.get(&EXTRA_GIT_COMMIT.to_vec()) {
        Some(raw) => {
            verify_raw_commit(raw, cs, root_git, parent_gits)?;
            Ok(raw.clone())
        }
        None => Ok(render_commit(cs, root_git, parent_gits)),
    }
}

/// A git commit split into the fields asc models. Unknown headers are dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCommit {
    pub tree: GitOid,
    pub parents: Vec<GitOid>,
    pub author: Signature,
    pub committer: Signature,
    /// Decoded `asc` header; `None` if absent or undecodable.
    pub asc: Option<AscGitExtra>,
    pub gpgsig: Option<Vec<u8>>,
    pub message: Vec<u8>,
}

/// Parses a commit following git fsck's grammar: one `tree`, a contiguous run of
/// `parent`, one `author`, one `committer`, then optional headers. `encoding`,
/// `gpgsig` and `asc` may appear once; other headers (`mergetag`, unknown) are skipped.
pub fn parse_commit(raw: &[u8]) -> Result<ParsedCommit, GitError> {
    let (h, message) = split_headers(raw);
    let bad = GitError::Commit;
    let mut it = h.iter().peekable();
    let tree = match it.next() {
        Some((k, v)) if *k == b"tree" => parse_oid(v, "bad tree oid").map_err(bad)?,
        _ => return Err(bad("missing tree")),
    };
    let mut parents = Vec::new();
    while let Some((_, v)) = it.next_if(|(k, _)| *k == b"parent") {
        parents.push(parse_oid(v, "bad parent oid").map_err(bad)?);
    }
    let mut ident = |key: &[u8], missing, what| match it.next() {
        Some((k, v)) if *k == key => parse_signature(v, what).map_err(bad),
        _ => Err(bad(missing)),
    };
    let author = ident(b"author", "missing author", "bad author")?;
    let committer = ident(b"committer", "missing committer", "bad committer")?;
    let (mut asc, mut gpgsig, mut encoding) = (None, None, false);
    for (k, v) in it {
        match *k {
            b"tree" | b"parent" | b"author" | b"committer" => {
                return Err(bad("duplicate or misplaced header"));
            }
            b"encoding" if !std::mem::replace(&mut encoding, true) => {}
            b"gpgsig" if gpgsig.is_none() => gpgsig = Some(v.clone()),
            b"asc" if asc.is_none() => {
                let decoded = BASE64
                    .decode(v)
                    .ok()
                    .and_then(|b| AscGitExtra::decode(&b).ok());
                asc = Some(decoded.ok_or(bad("bad asc header"))?);
            }
            b"encoding" | b"gpgsig" | b"asc" => return Err(bad("duplicate header")),
            _ => {}
        }
    }
    Ok(ParsedCommit {
        tree,
        parents,
        author,
        committer,
        asc,
        gpgsig,
        message: message.to_vec(),
    })
}

/// SPEC §2.9 import rule. Builds the canonical changeset from `raw` plus the
/// caller's `root`, `parents` and `changes` (leaf diff against `parents[0]`),
/// applying copies from the `asc` header. If the canonical render differs from
/// `raw`, the result carries `git.commit = raw`.
pub fn canonicalise_commit(
    raw: &[u8],
    root: TreeId,
    parents: Vec<ChangesetId>,
    mut changes: Vec<(RepoPath, Change)>,
) -> Result<Changeset, GitError> {
    let p = parse_commit(raw)?;
    if parents.len() != p.parents.len() {
        return Err(GitError::Parents {
            expected: parents.len(),
            found: p.parents.len(),
        });
    }
    let mut extra = Vec::new();
    if let Some(asc) = &p.asc {
        if asc.copies.as_slice().len() > changes.len() {
            return Err(GitError::Commit("more asc copies than changes"));
        }
        let index: std::collections::HashMap<&RepoPath, usize> = changes
            .iter()
            .enumerate()
            .map(|(i, (q, _))| (q, i))
            .collect();
        let hits: Vec<(usize, RepoPath)> = asc
            .copies
            .as_slice()
            .iter()
            .filter_map(|(path, from)| index.get(path).map(|i| (*i, from.clone())))
            .collect();
        for (i, from) in hits {
            if let Change::Write { copy_from, .. } = &mut changes[i].1 {
                *copy_from = Some(from);
            }
        }
        if asc.snapshot {
            extra.push((EXTRA_SNAPSHOT.to_vec(), Vec::new()));
        }
    }
    if let Some(sig) = p.gpgsig {
        extra.push((EXTRA_GIT_GPGSIG.to_vec(), sig));
    }
    let mut cs = Changeset {
        parents,
        root,
        author: p.author,
        committer: p.committer,
        message: p.message,
        extra: SortedMap::new(extra)?,
        changes: SortedMap::new(changes)?,
    };
    if render_commit(&cs, p.tree, &p.parents) != raw {
        let mut items = cs.extra.as_slice().to_vec();
        items.push((EXTRA_GIT_COMMIT.to_vec(), raw.to_vec()));
        cs.extra = SortedMap::new(items)?;
    }
    Ok(cs)
}

// ----------------------------------------------------------------- tags

/// Signature block markers git recognises at the end of a tag message.
const SIG_MARKERS: [&[u8]; 3] = [
    b"-----BEGIN PGP SIGNATURE-----",
    b"-----BEGIN SSH SIGNATURE-----",
    b"-----BEGIN SIGNED MESSAGE-----",
];

/// Splits a tag body after the headers into message and trailing signature block.
fn split_tag_signature(body: &[u8]) -> (&[u8], Option<&[u8]>) {
    let at_line_start = std::iter::once(0).chain(
        body.iter()
            .enumerate()
            .filter(|(_, c)| **c == b'\n')
            .map(|(i, _)| i + 1),
    );
    for i in at_line_start {
        if SIG_MARKERS.iter().any(|m| body[i..].starts_with(m)) {
            return (&body[..i], Some(&body[i..]));
        }
    }
    (body, None)
}

/// Canonical git tag body. The signature block, if any, follows the message.
/// Only commit targets are modelled.
pub fn render_tag(tag: &TagRecord, target_git: GitOid) -> Vec<u8> {
    let mut out = Vec::new();
    push_header(&mut out, "object", &oid_hex(target_git));
    push_header(&mut out, "type", b"commit");
    push_header(&mut out, "tag", &tag.name);
    if let Some(t) = &tag.tagger {
        out.extend_from_slice(b"tagger ");
        render_signature(&mut out, t);
        out.push(b'\n');
    }
    out.push(b'\n');
    out.extend_from_slice(&tag.message);
    if let Some(sig) = tag.extra.get(&EXTRA_GIT_GPGSIG.to_vec()) {
        out.extend_from_slice(sig);
    }
    out
}

/// Checks that `raw` describes the same tag as `tag`: commit type, target, name,
/// tagger, message and trailing signature.
pub fn verify_raw_tag(raw: &[u8], tag: &TagRecord, target_git: GitOid) -> Result<(), GitError> {
    let p = parse_tag(raw)?;
    let bad = GitError::RawMismatch;
    if p.object_type != b"commit" {
        return Err(bad("type"));
    }
    if p.object != target_git {
        return Err(bad("target"));
    }
    if p.name != tag.name {
        return Err(bad("name"));
    }
    if p.tagger != tag.tagger {
        return Err(bad("tagger"));
    }
    if p.message != tag.message {
        return Err(bad("message"));
    }
    if p.gpgsig.as_deref() != tag.extra.get(&EXTRA_GIT_GPGSIG.to_vec()).map(Vec::as_slice) {
        return Err(bad("gpgsig"));
    }
    Ok(())
}

/// The bytes git has for this tag: `git.tag` if present (after [`verify_raw_tag`]),
/// else the canonical render. As for [`export_commit`], `git.tag` is import-only.
pub fn export_tag(tag: &TagRecord, target_git: GitOid) -> Result<Vec<u8>, GitError> {
    match tag.extra.get(&EXTRA_GIT_TAG.to_vec()) {
        Some(raw) => {
            verify_raw_tag(raw, tag, target_git)?;
            Ok(raw.clone())
        }
        None => Ok(render_tag(tag, target_git)),
    }
}

/// A git tag split into the fields asc models.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedTag {
    pub object: GitOid,
    /// The `type` header (`commit`, `tree`, ...).
    pub object_type: Vec<u8>,
    pub name: Vec<u8>,
    pub tagger: Option<Signature>,
    pub message: Vec<u8>,
    /// Trailing signature block, verbatim.
    pub gpgsig: Option<Vec<u8>>,
}

/// Parses a tag in git's order: `object`, `type`, `tag`, optional `tagger`; nothing else.
pub fn parse_tag(raw: &[u8]) -> Result<ParsedTag, GitError> {
    let (h, body) = split_headers(raw);
    let bad = GitError::Tag;
    let mut it = h.iter();
    let mut next = |key: &[u8], missing| match it.next() {
        Some((k, v)) if *k == key => Ok(v),
        _ => Err(bad(missing)),
    };
    let object = parse_oid(next(b"object", "missing object")?, "bad object oid").map_err(bad)?;
    let object_type = next(b"type", "missing type")?.clone();
    let name = next(b"tag", "missing tag name")?.clone();
    let tagger = match it.next() {
        None => None,
        Some((k, v)) if *k == b"tagger" && it.next().is_none() => {
            Some(parse_signature(v, "bad tagger").map_err(bad)?)
        }
        Some(_) => return Err(bad("duplicate or unexpected header (git-fidelity Q-8)")),
    };
    let (message, gpgsig) = split_tag_signature(body);
    Ok(ParsedTag {
        object,
        object_type,
        name,
        tagger,
        message: message.to_vec(),
        gpgsig: gpgsig.map(<[u8]>::to_vec),
    })
}

/// Tag counterpart of [`canonicalise_commit`]; stores `git.tag = raw` when the render differs.
pub fn canonicalise_tag(raw: &[u8], target: ChangesetId) -> Result<TagRecord, GitError> {
    let p = parse_tag(raw)?;
    let mut extra = Vec::new();
    if let Some(sig) = p.gpgsig {
        extra.push((EXTRA_GIT_GPGSIG.to_vec(), sig));
    }
    let mut tag = TagRecord {
        target,
        name: p.name,
        tagger: p.tagger,
        message: p.message,
        extra: SortedMap::new(extra)?,
    };
    if render_tag(&tag, p.object) != raw {
        let mut items = tag.extra.as_slice().to_vec();
        items.push((EXTRA_GIT_TAG.to_vec(), raw.to_vec()));
        tag.extra = SortedMap::new(items)?;
    }
    Ok(tag)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;

    use super::*;
    use crate::object::Entry;
    use crate::path::Name;

    fn golden(name: &str) -> Vec<u8> {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden/git")
            .join(name);
        std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    /// `(kind, file, sha, flag)` rows from `expected.txt`.
    fn expected(kind: &str) -> Vec<(String, GitOid, String)> {
        String::from_utf8(golden("expected.txt"))
            .unwrap()
            .lines()
            .map(|l| l.split(' ').collect::<Vec<_>>())
            .filter(|f| f[0] == kind)
            .map(|f| (f[1].to_owned(), f[2].parse().unwrap(), f[3].to_owned()))
            .collect()
    }

    fn cid(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }

    #[test]
    fn golden_blobs() {
        for (file, sha, _) in expected("blob") {
            let data = golden(&file);
            assert_eq!(blob_oid(&data), sha, "{file}");
            let mut h = BlobOidHasher::new(data.len() as u64);
            for chunk in data.chunks(3) {
                h.update(chunk);
            }
            assert_eq!(h.finalize().unwrap(), sha, "{file} streamed");
        }
    }

    #[test]
    fn blob_hasher_checks_size() {
        let mut h = BlobOidHasher::new(5);
        h.update(b"abc");
        assert_eq!(
            h.finalize(),
            Err(GitError::BlobSize {
                declared: 5,
                hashed: 3
            })
        );
    }

    /// Reads a raw git tree back into a `Tree` with placeholder asc ids.
    fn tree_from_git(mut raw: &[u8]) -> Tree {
        let mut entries = Vec::new();
        while !raw.is_empty() {
            let sp = raw.iter().position(|&c| c == b' ').unwrap();
            let nul = raw.iter().position(|&c| c == 0).unwrap();
            let kind = match &raw[..sp] {
                b"100644" => Kind::File,
                b"100755" => Kind::Exec,
                b"120000" => Kind::Symlink,
                b"40000" => Kind::Dir,
                b"160000" => Kind::Gitlink,
                m => panic!("mode {m:?}"),
            };
            let git = GitOid::from_bytes(raw[nul + 1..nul + 21].try_into().unwrap());
            entries.push(Entry {
                name: Name::new(&raw[sp + 1..nul]).unwrap(),
                kind,
                id: [0; 32],
                size: 0,
                git,
            });
            raw = &raw[nul + 21..];
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Tree::new(entries, true).unwrap()
    }

    #[test]
    fn golden_trees() {
        for (file, sha, _) in expected("tree") {
            let raw = golden(&file);
            let tree = tree_from_git(&raw);
            assert_eq!(render_tree(&tree), raw, "{file}");
            assert_eq!(tree_oid(&tree), sha, "{file}");
        }
    }

    #[test]
    fn dirs_sort_as_name_slash() {
        let raw = golden("tree_mixed.bin");
        let names: Vec<_> = tree_from_git(&raw)
            .entries()
            .iter()
            .map(|e| e.name.as_bytes().to_vec())
            .collect();
        let rendered = render_tree(&tree_from_git(&raw));
        let pos = |n: &[u8]| {
            rendered
                .windows(n.len() + 1)
                .position(|w| w[..n.len()] == *n && w[n.len()] == 0)
                .unwrap()
        };
        assert!(names.contains(&b"foo".to_vec()));
        assert!(pos(b"foo-bar") < pos(b"foo.txt") && pos(b"foo.txt") < pos(b"foo"));
    }

    fn write_a() -> Vec<(RepoPath, Change)> {
        let w = Change::Write {
            kind: Kind::File,
            file: crate::id::FileId::of(b"x"),
            size: 1,
            git: blob_oid(b"x"),
            copy_from: None,
        };
        vec![(RepoPath::new("a").unwrap(), w)]
    }

    #[test]
    fn golden_commits() {
        for (file, sha, flag) in expected("commit") {
            let raw = golden(&file);
            assert_eq!(object_oid(ObjectKind::Commit, &raw), sha, "{file}");
            let p = parse_commit(&raw).unwrap();
            let parents: Vec<_> = (1..).map(cid).take(p.parents.len()).collect();
            let cs =
                canonicalise_commit(&raw, TreeId::from_bytes([9; 32]), parents, write_a()).unwrap();
            assert_eq!(
                cs.extra.get(&EXTRA_GIT_COMMIT.to_vec()).is_some(),
                flag == "raw",
                "{file}"
            );
            let exported = export_commit(&cs, p.tree, &p.parents).unwrap();
            assert_eq!(exported, raw, "{file}");
            assert_eq!(object_oid(ObjectKind::Commit, &exported), sha, "{file}");
        }
    }

    #[test]
    fn golden_commit_details() {
        let tz = parse_commit(&golden("commit_tz_0530.bin")).unwrap();
        assert_eq!(tz.author.tz_min(), 330);
        let neg = parse_commit(&golden("commit_tz_neg.bin")).unwrap();
        assert_eq!(neg.author.tz_min(), -(8 * 60 + 45));
        assert_eq!(
            parse_commit(&golden("commit_merge.bin"))
                .unwrap()
                .parents
                .len(),
            2
        );
        let latin = parse_commit(&golden("commit_non_utf8.bin")).unwrap();
        assert_eq!(latin.author.name(), b"Jos\xe9");
        let signed = parse_commit(&golden("commit_signed.bin")).unwrap();
        let sig = signed.gpgsig.unwrap();
        assert!(
            sig.starts_with(b"-----BEGIN SSH SIGNATURE-----\n")
                && sig.ends_with(b"-----END SSH SIGNATURE-----")
        );
        let asc = canonicalise_commit(
            &golden("commit_asc.bin"),
            TreeId::from_bytes([9; 32]),
            vec![],
            write_a(),
        )
        .unwrap();
        match &asc.changes.as_slice()[0].1 {
            Change::Write { copy_from, .. } => {
                assert_eq!(copy_from.as_ref().unwrap().as_bytes(), b"b")
            }
            other => panic!("{other:?}"),
        }
    }

    fn commit_with_copies(n: usize) -> (Vec<u8>, Vec<(RepoPath, Change)>) {
        let w = Change::Write {
            kind: Kind::File,
            file: crate::id::FileId::of(b"x"),
            size: 1,
            git: blob_oid(b"x"),
            copy_from: None,
        };
        let changes: Vec<_> = (0..n)
            .map(|i| (RepoPath::new(format!("f{i:07}")).unwrap(), w.clone()))
            .collect();
        let copies: Vec<_> = changes
            .iter()
            .map(|(p, _)| (p.clone(), RepoPath::new("src").unwrap()))
            .collect();
        let asc = BASE64.encode(
            &AscGitExtra {
                copies: SortedMap::new(copies).unwrap(),
                snapshot: false,
            }
            .encode(),
        );
        let t = "tree 1111111111111111111111111111111111111111
";
        (
            format!(
                "{t}{A}{C}asc {asc}

m"
            )
            .into_bytes(),
            changes,
        )
    }

    #[test]
    fn many_copies_apply_in_linear_time() {
        let n = 100_000;
        let (raw, changes) = commit_with_copies(n);
        let t = std::time::Instant::now();
        let cs = canonicalise_commit(&raw, TreeId::from_bytes([9; 32]), vec![], changes).unwrap();
        assert!(t.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(cs.changes.as_slice().len(), n);
    }

    #[test]
    fn more_copies_than_changes_rejected() {
        let (raw, mut changes) = commit_with_copies(50);
        changes.truncate(10);
        assert!(canonicalise_commit(&raw, TreeId::from_bytes([9; 32]), vec![], changes).is_err());
    }

    #[test]
    fn golden_tags() {
        for (file, sha, flag) in expected("tag") {
            let raw = golden(&file);
            assert_eq!(object_oid(ObjectKind::Tag, &raw), sha, "{file}");
            let p = parse_tag(&raw).unwrap();
            let tag = canonicalise_tag(&raw, cid(1)).unwrap();
            assert_eq!(
                tag.extra.get(&EXTRA_GIT_TAG.to_vec()).is_some(),
                flag == "raw",
                "{file}"
            );
            assert_eq!(export_tag(&tag, p.object).unwrap(), raw, "{file}");
        }
        let signed = parse_tag(&golden("tag_signed.bin")).unwrap();
        assert_eq!(signed.message, b"v2 signed\n");
        assert!(
            signed
                .gpgsig
                .unwrap()
                .starts_with(b"-----BEGIN SSH SIGNATURE-----\n")
        );
    }

    // fidelity: Q-8
    #[test]
    fn non_canonical_tag_keeps_raw() {
        let raw = b"object 1111111111111111111111111111111111111111\ntype commit\ntag t\ntagger A<a@x> 1 +0000\n\nmsg\n";
        let tag = canonicalise_tag(raw, cid(1)).unwrap();
        assert_eq!(
            tag.extra.get(&EXTRA_GIT_TAG.to_vec()).map(Vec::as_slice),
            Some(&raw[..])
        );
    }

    fn sample_commit() -> (Vec<u8>, GitOid, Vec<GitOid>, Changeset) {
        let (t, p1, p2) = (
            GitOid::from_bytes([1; 20]),
            GitOid::from_bytes([2; 20]),
            GitOid::from_bytes([3; 20]),
        );
        let raw = format!(
            "tree {t}\nparent {p1}\nparent {p2}\nauthor A <a@x> 10 +0000\ncommitter C <c@x> 20 +0100\nencoding ISO-8859-1\n\nmsg\n"
        )
        .into_bytes();
        let cs = canonicalise_commit(
            &raw,
            TreeId::from_bytes([9; 32]),
            vec![cid(1), cid(2)],
            write_a(),
        )
        .unwrap();
        (raw, t, vec![p1, p2], cs)
    }

    #[test]
    fn rejects_header_injection() {
        let inj = b"A\ntree 0000000000000000000000000000000000000000";
        assert!(Signature::new(inj.to_vec(), b"a@x".to_vec(), 0, 0).is_err());
        assert!(Signature::new(b"A".to_vec(), b"a>\nparent x".to_vec(), 0, 0).is_err());
        assert!(Signature::new(b"A <x".to_vec(), b"a@x".to_vec(), 0, 0).is_err());
        // An imported ident carrying such bytes (via a continuation line) is refused, not rendered.
        let raw = b"tree 1111111111111111111111111111111111111111\nauthor A\n tree x <a@x> 1 +0000\ncommitter C <c@x> 1 +0000\n\nm";
        assert!(parse_commit(raw).is_err());
    }

    #[test]
    fn non_canonical_import_round_trips() {
        let (raw, t, parents, cs) = sample_commit();
        assert!(cs.extra.get(&EXTRA_GIT_COMMIT.to_vec()).is_some());
        assert_eq!(export_commit(&cs, t, &parents).unwrap(), raw);
    }

    #[test]
    fn mismatched_raw_commit_rejected() {
        let (raw, t, parents, cs) = sample_commit();
        assert!(verify_raw_commit(&raw, &cs, t, &parents).is_ok());
        let other = GitOid::from_bytes([7; 20]);
        assert!(matches!(
            export_commit(&cs, other, &parents),
            Err(GitError::RawMismatch("tree"))
        ));
        let swapped = [parents[1], parents[0]];
        assert!(matches!(
            export_commit(&cs, t, &swapped),
            Err(GitError::RawMismatch("parents"))
        ));
        let mut edited = cs.clone();
        edited.message = b"other\n".to_vec();
        assert!(matches!(
            export_commit(&edited, t, &parents),
            Err(GitError::RawMismatch("message"))
        ));
        let mut who = cs;
        who.author = Signature::new(b"Eve".to_vec(), b"e@x".to_vec(), 10, 0).unwrap();
        assert!(matches!(
            export_commit(&who, t, &parents),
            Err(GitError::RawMismatch("author"))
        ));
    }

    #[test]
    fn mismatched_raw_tag_rejected() {
        let target = GitOid::from_bytes([1; 20]);
        let raw = format!("object {target}\ntype commit\ntag t\ntagger A<a@x> 1 +0000\n\nmsg\n");
        let tag = canonicalise_tag(raw.as_bytes(), cid(1)).unwrap();
        assert_eq!(export_tag(&tag, target).unwrap(), raw.as_bytes());
        assert!(export_tag(&tag, GitOid::from_bytes([2; 20])).is_err());
        let mut renamed = tag.clone();
        renamed.name = b"u".to_vec();
        assert!(export_tag(&renamed, target).is_err());
        let mut edited = tag;
        edited.message = b"changed\n".to_vec();
        assert!(export_tag(&edited, target).is_err());
    }

    const A: &str = "author A <a@x> 10 +0000\n";
    const C: &str = "committer C <c@x> 20 +0000\n";

    fn asc_header(snapshot: bool) -> String {
        BASE64.encode(
            &AscGitExtra {
                copies: SortedMap::new(vec![]).unwrap(),
                snapshot,
            }
            .encode(),
        )
    }

    // fidelity: Q-5
    #[test]
    fn strict_commit_grammar() {
        let t = "tree 1111111111111111111111111111111111111111\n";
        let p = "parent 2222222222222222222222222222222222222222\n";
        let ok = format!("{t}{p}{A}{C}encoding x\n\nm");
        assert!(parse_commit(ok.as_bytes()).is_ok());
        let sig = "gpgsig a\n b\n";
        let asc = format!("asc {}\n", asc_header(true));
        let cases = [
            format!("{p}{t}{A}{C}\nm"),
            format!("{t}{t}{A}{C}\nm"),
            format!("{t}{A}{p}{C}\nm"),
            format!("{t}{A}{C}{p}\nm"),
            format!("{t}{p}{A}{p}{C}\nm"),
            format!("{t}{C}{A}\nm"),
            format!("{t}{A}{A}{C}\nm"),
            format!("{t}{A}{C}{C}\nm"),
            format!("{t}{A}{C}{A}\nm"),
            format!("{t}{A}{C}{t}\nm"),
            format!("{t}{A}{C}encoding x\nencoding y\n\nm"),
            format!("{t}{A}{C}{sig}{sig}\nm"),
            format!("{t}{A}{C}{asc}{asc}\nm"),
        ];
        for c in &cases {
            assert!(parse_commit(c.as_bytes()).is_err(), "{c}");
        }
    }

    // fidelity: Q-5
    #[test]
    fn verify_commit_checks_gpgsig_and_asc() {
        let (raw, t, parents, cs) = sample_commit();
        let mut signed = cs.clone();
        signed.extra = SortedMap::new(vec![(EXTRA_GIT_GPGSIG.to_vec(), b"sig".to_vec())]).unwrap();
        assert!(matches!(
            verify_raw_commit(&raw, &signed, t, &parents),
            Err(GitError::RawMismatch("gpgsig"))
        ));
        let with_sig =
            String::from_utf8(raw.clone())
                .unwrap()
                .replacen("encoding", "gpgsig sig\nencoding", 1);
        assert!(matches!(
            verify_raw_commit(with_sig.as_bytes(), &cs, t, &parents),
            Err(GitError::RawMismatch("gpgsig"))
        ));
        assert!(verify_raw_commit(with_sig.as_bytes(), &signed, t, &parents).is_ok());
        let with_asc = String::from_utf8(raw).unwrap().replacen(
            "encoding",
            &format!("asc {}\nencoding", asc_header(true)),
            1,
        );
        assert!(matches!(
            verify_raw_commit(with_asc.as_bytes(), &cs, t, &parents),
            Err(GitError::RawMismatch("asc"))
        ));
        let mut snap = cs;
        snap.extra = SortedMap::new(vec![(EXTRA_SNAPSHOT.to_vec(), Vec::new())]).unwrap();
        assert!(verify_raw_commit(with_asc.as_bytes(), &snap, t, &parents).is_ok());
    }

    // fidelity: Q-8
    #[test]
    fn strict_tag_grammar_and_checks() {
        let o = "object 1111111111111111111111111111111111111111\n";
        let ty = "type commit\n";
        let tg = "tag t\n";
        let tagger = "tagger A <a@x> 1 +0000\n";
        assert!(parse_tag(format!("{o}{ty}{tg}{tagger}\nm").as_bytes()).is_ok());
        let cases = [
            format!("{ty}{o}{tg}\nm"),
            format!("{o}{tg}{ty}\nm"),
            format!("{o}{o}{ty}{tg}\nm"),
            format!("{o}{ty}{ty}{tg}\nm"),
            format!("{o}{ty}{tg}{tg}\nm"),
            format!("{o}{ty}{tg}{tagger}{tagger}\nm"),
            format!("{o}{ty}{tagger}{tg}\nm"),
            format!("{o}{ty}{tg}{tagger}x y\n\nm"),
        ];
        for c in &cases {
            assert!(parse_tag(c.as_bytes()).is_err(), "{c}");
        }
        let target = GitOid::from_bytes([0x11; 20]);
        let tree_tag = format!("{o}type tree\n{tg}\nm");
        let rec = canonicalise_tag(tree_tag.as_bytes(), cid(1)).unwrap();
        assert!(matches!(
            verify_raw_tag(tree_tag.as_bytes(), &rec, target),
            Err(GitError::RawMismatch("type"))
        ));
        let sig = "-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----\n";
        let raw = format!("{o}{ty}{tg}\nm\n{sig}");
        let mut rec = canonicalise_tag(raw.as_bytes(), cid(1)).unwrap();
        assert!(verify_raw_tag(raw.as_bytes(), &rec, target).is_ok());
        rec.extra = SortedMap::new(vec![]).unwrap();
        assert!(matches!(
            verify_raw_tag(raw.as_bytes(), &rec, target),
            Err(GitError::RawMismatch("gpgsig"))
        ));
    }

    #[test]
    fn tz_round_trip() {
        assert_eq!(tz_to_string(0), "+0000");
        assert_eq!(tz_to_string(330), "+0530");
        assert_eq!(tz_to_string(-30), "-0030");
        for tz in -5999..=5999i16 {
            assert_eq!(parse_tz(tz_to_string(tz).as_bytes()), Some(tz));
        }
        for bad in ["", "0100", "+100", "+01000", "+0160", "+01a0", "*0100"] {
            assert_eq!(parse_tz(bad.as_bytes()), None, "{bad}");
        }
    }

    fn arb_sig() -> impl Strategy<Value = Signature> {
        let name = prop::collection::vec(
            prop::sample::select(
                (1u8..=255)
                    .filter(|c| !b"<>\n".contains(c))
                    .collect::<Vec<_>>(),
            ),
            0..12,
        );
        let email = prop::collection::vec(
            prop::sample::select(
                (1u8..=255)
                    .filter(|c| !b"<>\n".contains(c))
                    .collect::<Vec<_>>(),
            ),
            0..12,
        );
        (name, email, any::<i64>(), -5999i16..=5999).prop_map(|(name, email, time, tz_min)| {
            Signature::new(name, email, time, tz_min).unwrap()
        })
    }

    fn arb_path() -> impl Strategy<Value = RepoPath> {
        "[a-z]{1,3}(/[a-z]{1,3}){0,2}".prop_map(|s| RepoPath::new(s).unwrap())
    }

    fn arb_changeset() -> impl Strategy<Value = (Changeset, GitOid, Vec<GitOid>)> {
        let oid = any::<[u8; 20]>().prop_map(GitOid::from_bytes);
        let sigblock = prop::option::of(prop::collection::vec(
            prop::collection::vec(any::<u8>().prop_filter("no LF", |c| *c != b'\n'), 0..20),
            1..4,
        ));
        (
            prop::collection::vec((arb_path(), prop::option::of(arb_path())), 0..5),
            arb_sig(),
            arb_sig(),
            prop::collection::vec(any::<u8>(), 0..64),
            sigblock,
            any::<bool>(),
            oid.clone(),
            prop::collection::vec(oid, 0..3),
            any::<[u8; 32]>(),
        )
            .prop_map(
                |(
                    files,
                    author,
                    committer,
                    message,
                    sig,
                    snapshot,
                    root_git,
                    parent_gits,
                    root,
                )| {
                    let files: std::collections::BTreeMap<_, _> = files.into_iter().collect();
                    let changes = files
                        .into_iter()
                        .map(|(p, copy_from)| {
                            let w = Change::Write {
                                kind: Kind::File,
                                file: crate::id::FileId::of(p.as_bytes()),
                                size: 1,
                                git: root_git,
                                copy_from,
                            };
                            (p, w)
                        })
                        .collect();
                    let mut extra = Vec::new();
                    if snapshot {
                        extra.push((EXTRA_SNAPSHOT.to_vec(), Vec::new()));
                    }
                    if let Some(lines) = sig {
                        extra.push((EXTRA_GIT_GPGSIG.to_vec(), lines.join(&b'\n')));
                    }
                    let cs = Changeset {
                        parents: (0..parent_gits.len() as u8).map(cid).collect(),
                        root: TreeId::from_bytes(root),
                        author,
                        committer,
                        message,
                        extra: SortedMap::new(extra).unwrap(),
                        changes: SortedMap::new(changes).unwrap(),
                    };
                    (cs, root_git, parent_gits)
                },
            )
    }

    proptest! {
        /// canonical render -> parse -> canonicalise is the identity.
        #[test]
        fn commit_round_trip((cs, root_git, parent_gits) in arb_changeset()) {
            let raw = render_commit(&cs, root_git, &parent_gits);
            let mut stripped = cs.changes.as_slice().to_vec();
            for (_, c) in &mut stripped {
                if let Change::Write { copy_from, .. } = c { *copy_from = None; }
            }
            let back = canonicalise_commit(&raw, cs.root, cs.parents.clone(), stripped).unwrap();
            prop_assert_eq!(back, cs);
        }

        #[test]
        fn tag_round_trip(
            name in prop::collection::vec(any::<u8>().prop_filter("no LF", |c| *c != b'\n'), 1..10),
            tagger in prop::option::of(arb_sig()),
            message in prop::collection::vec(any::<u8>(), 0..40),
            sig in any::<bool>(),
            oid in any::<[u8; 20]>(),
        ) {
            let extra = if sig { vec![(EXTRA_GIT_GPGSIG.to_vec(), b"-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----\n".to_vec())] } else { vec![] };
            let tag = TagRecord { target: cid(3), name, tagger, message, extra: SortedMap::new(extra).unwrap() };
            let raw = render_tag(&tag, GitOid::from_bytes(oid));
            let back = canonicalise_tag(&raw, cid(3)).unwrap();
            // A message that itself holds a signature marker splits differently but renders the same.
            prop_assert_eq!(render_tag(&back, GitOid::from_bytes(oid)), raw);
            prop_assert!(back.extra.get(&EXTRA_GIT_TAG.to_vec()).is_none());
        }
    }
}
