//! Git object rendering and parsing (SPEC §2.9).
//!
//! Pure functions over bytes. Bodies are returned without the `<kind> <len>\0`
//! header; [`object_oid`] adds it when hashing.

use data_encoding::BASE64;
use sha1::{Digest, Sha1};

use crate::id::{ChangesetId, GitOid, TreeId};
use crate::object::{AscGitExtra, Change, Changeset, Kind, LfsRef, ObjectError, SortedMap, TagRecord, Tree};
use crate::path::RepoPath;
use crate::sig::Signature;

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
        Self { sha: start(ObjectKind::Blob, size), declared: size, seen: 0 }
    }

    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.sha.update(bytes);
        self.seen += bytes.len() as u64;
        self
    }

    /// Fails if the bytes fed in do not add up to the declared size.
    pub fn finalize(self) -> Result<GitOid, GitError> {
        if self.seen != self.declared {
            return Err(GitError::BlobSize { declared: self.declared, hashed: self.seen });
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
    order.sort_by(|a, b| sort_key(a.kind, a.name.as_bytes()).cmp(sort_key(b.kind, b.name.as_bytes())));
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
    name.iter().copied().chain((kind == Kind::Dir).then_some(b'/'))
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
    let [sign @ (b'+' | b'-'), d @ ..] = s else { return None };
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
    out.extend_from_slice(&s.name);
    out.extend_from_slice(b" <");
    out.extend_from_slice(&s.email);
    out.extend_from_slice(format!("> {} {}", s.time, tz_to_string(s.tz_min)).as_bytes());
}

fn parse_signature(b: &[u8], what: &'static str) -> Result<Signature, &'static str> {
    let gt = b.iter().rposition(|&c| c == b'>').ok_or(what)?;
    let lt = b[..gt].iter().rposition(|&c| c == b'<').ok_or(what)?;
    let name = &b[..lt];
    let name = name.strip_suffix(b" ").unwrap_or(name);
    let rest = b[gt + 1..].strip_prefix(b" ").ok_or(what)?;
    let sp = rest.iter().position(|&c| c == b' ').ok_or(what)?;
    let time = std::str::from_utf8(&rest[..sp]).ok().and_then(|t| t.parse().ok()).ok_or(what)?;
    let tz_min = parse_tz(&rest[sp + 1..]).ok_or(what)?;
    Ok(Signature { name: name.to_vec(), email: b[lt + 1..gt].to_vec(), time, tz_min })
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
        let end = raw[pos..].iter().position(|&c| c == b'\n').map_or(raw.len(), |i| pos + i);
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

fn header<'a>(h: &'a Headers, key: &[u8]) -> Option<&'a [u8]> {
    h.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_slice())
}

fn parse_oid(v: &[u8], what: &'static str) -> Result<GitOid, &'static str> {
    std::str::from_utf8(v).ok().and_then(|s| s.parse().ok()).ok_or(what)
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
            Change::Write { copy_from: Some(from), .. } => Some((p.clone(), from.clone())),
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

/// The bytes git has for this commit: `git.commit` if present, else the canonical render.
pub fn export_commit(cs: &Changeset, root_git: GitOid, parent_gits: &[GitOid]) -> Vec<u8> {
    match cs.extra.get(&EXTRA_GIT_COMMIT.to_vec()) {
        Some(raw) => raw.clone(),
        None => render_commit(cs, root_git, parent_gits),
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

pub fn parse_commit(raw: &[u8]) -> Result<ParsedCommit, GitError> {
    let (h, message) = split_headers(raw);
    let bad = GitError::Commit;
    let sig = |key: &[u8], what| {
        let v = header(&h, key).ok_or(bad(what))?;
        parse_signature(v, what).map_err(bad)
    };
    Ok(ParsedCommit {
        tree: parse_oid(header(&h, b"tree").ok_or(bad("missing tree"))?, "bad tree oid").map_err(bad)?,
        parents: h
            .iter()
            .filter(|(k, _)| *k == b"parent")
            .map(|(_, v)| parse_oid(v, "bad parent oid").map_err(bad))
            .collect::<Result<_, _>>()?,
        author: sig(b"author", "bad author")?,
        committer: sig(b"committer", "bad committer")?,
        asc: header(&h, b"asc")
            .and_then(|v| BASE64.decode(v).ok())
            .and_then(|b| AscGitExtra::decode(&b).ok()),
        gpgsig: header(&h, b"gpgsig").map(<[u8]>::to_vec),
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
        return Err(GitError::Parents { expected: parents.len(), found: p.parents.len() });
    }
    let mut extra = Vec::new();
    if let Some(asc) = &p.asc {
        for (path, from) in asc.copies.as_slice() {
            if let Some((_, Change::Write { copy_from, .. })) = changes.iter_mut().find(|(q, _)| q == path) {
                *copy_from = Some(from.clone());
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
const SIG_MARKERS: [&[u8]; 3] =
    [b"-----BEGIN PGP SIGNATURE-----", b"-----BEGIN SSH SIGNATURE-----", b"-----BEGIN SIGNED MESSAGE-----"];

/// Splits a tag body after the headers into message and trailing signature block.
fn split_tag_signature(body: &[u8]) -> (&[u8], Option<&[u8]>) {
    let at_line_start = std::iter::once(0).chain(body.iter().enumerate().filter(|(_, c)| **c == b'\n').map(|(i, _)| i + 1));
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

/// The bytes git has for this tag: `git.tag` if present, else the canonical render.
pub fn export_tag(tag: &TagRecord, target_git: GitOid) -> Vec<u8> {
    match tag.extra.get(&EXTRA_GIT_TAG.to_vec()) {
        Some(raw) => raw.clone(),
        None => render_tag(tag, target_git),
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

pub fn parse_tag(raw: &[u8]) -> Result<ParsedTag, GitError> {
    let (h, body) = split_headers(raw);
    let bad = GitError::Tag;
    let (message, gpgsig) = split_tag_signature(body);
    Ok(ParsedTag {
        object: parse_oid(header(&h, b"object").ok_or(bad("missing object"))?, "bad object oid").map_err(bad)?,
        object_type: header(&h, b"type").ok_or(bad("missing type"))?.to_vec(),
        name: header(&h, b"tag").ok_or(bad("missing tag name"))?.to_vec(),
        tagger: header(&h, b"tagger").map(|v| parse_signature(v, "bad tagger").map_err(bad)).transpose()?,
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

// ------------------------------------------------------------------ LFS

const LFS_MAX_POINTER: usize = 1024;
const LFS_VERSIONS: [&str; 2] = ["https://git-lfs.github.com/spec/v1", "https://hawser.github.com/spec/v1"];

/// Parses a git-LFS pointer file: `version` first, then `oid sha256:<hex>` and
/// `size <n>`; other keys are ignored. `None` if the bytes are not a pointer.
pub fn parse_lfs_pointer(bytes: &[u8]) -> Option<LfsRef> {
    if bytes.len() > LFS_MAX_POINTER {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?.strip_suffix('\n')?;
    let mut lines = text.split('\n');
    let version = lines.next()?.strip_prefix("version ")?;
    if !LFS_VERSIONS.contains(&version) {
        return None;
    }
    let (mut oid, mut size) = (None, None);
    for line in lines {
        let (key, value) = line.split_once(' ')?;
        match key {
            "oid" => {
                let hex = value.strip_prefix("sha256:")?;
                let raw = data_encoding::HEXLOWER.decode(hex.as_bytes()).ok()?;
                oid = Some(<[u8; 32]>::try_from(raw).ok()?);
            }
            "size" => size = Some(value.parse::<u64>().ok().filter(|_| value.bytes().all(|c| c.is_ascii_digit()))?),
            _ => {}
        }
    }
    Some(LfsRef { sha256: oid?, size: size? })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;

    use super::*;
    use crate::object::Entry;
    use crate::path::Name;

    fn golden(name: &str) -> Vec<u8> {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/git").join(name);
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
        assert_eq!(h.finalize(), Err(GitError::BlobSize { declared: 5, hashed: 3 }));
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
            entries.push(Entry { name: Name::new(&raw[sp + 1..nul]).unwrap(), kind, id: [0; 32], size: 0, git, lfs: None });
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
        let names: Vec<_> = tree_from_git(&raw).entries().iter().map(|e| e.name.as_bytes().to_vec()).collect();
        let rendered = render_tree(&tree_from_git(&raw));
        let pos = |n: &[u8]| rendered.windows(n.len() + 1).position(|w| w[..n.len()] == *n && w[n.len()] == 0).unwrap();
        assert!(names.contains(&b"foo".to_vec()));
        assert!(pos(b"foo-bar") < pos(b"foo.txt") && pos(b"foo.txt") < pos(b"foo"));
    }

    fn write_a() -> Vec<(RepoPath, Change)> {
        let w = Change::Write {
            kind: Kind::File,
            file: crate::id::FileId::of(b"x"),
            size: 1,
            git: blob_oid(b"x"),
            lfs: None,
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
            let cs = canonicalise_commit(&raw, TreeId::from_bytes([9; 32]), parents, write_a()).unwrap();
            assert_eq!(cs.extra.get(&EXTRA_GIT_COMMIT.to_vec()).is_some(), flag == "raw", "{file}");
            let exported = export_commit(&cs, p.tree, &p.parents);
            assert_eq!(exported, raw, "{file}");
            assert_eq!(object_oid(ObjectKind::Commit, &exported), sha, "{file}");
        }
    }

    #[test]
    fn golden_commit_details() {
        let tz = parse_commit(&golden("commit_tz_0530.bin")).unwrap();
        assert_eq!(tz.author.tz_min, 330);
        let neg = parse_commit(&golden("commit_tz_neg.bin")).unwrap();
        assert_eq!(neg.author.tz_min, -(8 * 60 + 45));
        assert_eq!(parse_commit(&golden("commit_merge.bin")).unwrap().parents.len(), 2);
        let latin = parse_commit(&golden("commit_non_utf8.bin")).unwrap();
        assert_eq!(latin.author.name, b"Jos\xe9");
        let signed = parse_commit(&golden("commit_signed.bin")).unwrap();
        let sig = signed.gpgsig.unwrap();
        assert!(sig.starts_with(b"-----BEGIN SSH SIGNATURE-----\n") && sig.ends_with(b"-----END SSH SIGNATURE-----"));
        let asc = canonicalise_commit(&golden("commit_asc.bin"), TreeId::from_bytes([9; 32]), vec![], write_a()).unwrap();
        match &asc.changes.as_slice()[0].1 {
            Change::Write { copy_from, .. } => assert_eq!(copy_from.as_ref().unwrap().as_bytes(), b"b"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn golden_tags() {
        for (file, sha, flag) in expected("tag") {
            let raw = golden(&file);
            assert_eq!(object_oid(ObjectKind::Tag, &raw), sha, "{file}");
            let p = parse_tag(&raw).unwrap();
            let tag = canonicalise_tag(&raw, cid(1)).unwrap();
            assert_eq!(tag.extra.get(&EXTRA_GIT_TAG.to_vec()).is_some(), flag == "raw", "{file}");
            assert_eq!(export_tag(&tag, p.object), raw, "{file}");
        }
        let signed = parse_tag(&golden("tag_signed.bin")).unwrap();
        assert_eq!(signed.message, b"v2 signed\n");
        assert!(signed.gpgsig.unwrap().starts_with(b"-----BEGIN SSH SIGNATURE-----\n"));
    }

    #[test]
    fn non_canonical_tag_keeps_raw() {
        let raw = b"object 1111111111111111111111111111111111111111\ntype tree\ntag t\n\nmsg\n";
        let tag = canonicalise_tag(raw, cid(1)).unwrap();
        assert_eq!(tag.extra.get(&EXTRA_GIT_TAG.to_vec()).map(Vec::as_slice), Some(&raw[..]));
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

    #[test]
    fn lfs_pointer() {
        let hex = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let ok = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{hex}\nsize 12345\n");
        let r = parse_lfs_pointer(ok.as_bytes()).unwrap();
        assert_eq!((r.size, data_encoding::HEXLOWER.encode(&r.sha256)), (12345, hex.to_owned()));
        let with_ext = format!("version https://git-lfs.github.com/spec/v1\next-0-foo sha256:{hex}\noid sha256:{hex}\nsize 1\n");
        assert!(parse_lfs_pointer(with_ext.as_bytes()).is_some());
        for bad in [
            format!("oid sha256:{hex}\nsize 1\nversion https://git-lfs.github.com/spec/v1\n"),
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{hex}\nsize 1"),
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{hex}\n"),
            format!("version https://git-lfs.github.com/spec/v1\noid sha1:{hex}\nsize 1\n"),
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 1\n", hex.to_uppercase()),
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{hex}\nsize +1\n"),
            format!("version https://example.com/v9\noid sha256:{hex}\nsize 1\n"),
            "just a file\n".to_owned(),
        ] {
            assert!(parse_lfs_pointer(bad.as_bytes()).is_none(), "{bad}");
        }
        let big = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{hex}\nsize 1\nx {}\n", "y".repeat(1100));
        assert!(parse_lfs_pointer(big.as_bytes()).is_none());
    }

    fn arb_sig() -> impl Strategy<Value = Signature> {
        let name = prop::collection::vec(prop::sample::select((1u8..=255).filter(|c| !b"\n".contains(c)).collect::<Vec<_>>()), 0..12);
        let email = prop::collection::vec(prop::sample::select((1u8..=255).filter(|c| !b"<>\n".contains(c)).collect::<Vec<_>>()), 0..12);
        (name, email, any::<i64>(), -5999i16..=5999)
            .prop_map(|(name, email, time, tz_min)| Signature { name, email, time, tz_min })
    }

    fn arb_path() -> impl Strategy<Value = RepoPath> {
        "[a-z]{1,3}(/[a-z]{1,3}){0,2}".prop_map(|s| RepoPath::new(s).unwrap())
    }

    fn arb_changeset() -> impl Strategy<Value = (Changeset, GitOid, Vec<GitOid>)> {
        let oid = any::<[u8; 20]>().prop_map(GitOid::from_bytes);
        let sigblock = prop::option::of(prop::collection::vec(prop::collection::vec(any::<u8>().prop_filter("no LF", |c| *c != b'\n'), 0..20), 1..4));
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
            .prop_map(|(files, author, committer, message, sig, snapshot, root_git, parent_gits, root)| {
                let files: std::collections::BTreeMap<_, _> = files.into_iter().collect();
                let changes = files
                    .into_iter()
                    .map(|(p, copy_from)| {
                        let w = Change::Write { kind: Kind::File, file: crate::id::FileId::of(p.as_bytes()), size: 1, git: root_git, lfs: None, copy_from };
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
            })
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
