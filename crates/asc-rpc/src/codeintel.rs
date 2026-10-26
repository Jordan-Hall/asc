//! Conversions for `codeintel.proto` (SPEC §23.4.1).
//!
//! Requests become checked domain values: every string, list and number is bounded, because the
//! peer is untrusted. Responses are built by the server (I16), so only [`ResolvedScope`] has a
//! decoding direction here, for clients that cite an immutable scope.

use asc_core::change::ChangeId;
use asc_core::id::{ChangesetId, TreeId};
use asc_core::path::RepoPath;
use bytes::Bytes;

use crate::convert::{ConvertError, IdMode, ModeId, Result, WireId};
use crate::pb;

/// Longest accepted search pattern or semantic query.
pub const MAX_PATTERN: usize = 16 * 1024;
/// Longest accepted name, revspec, pathspec, language, kind or page token.
pub const MAX_TEXT: usize = 4096;
/// Most entries in any repeated request field (paths, excludes, langs, kinds).
pub const MAX_LIST: usize = 256;
/// Most context lines around a match.
pub const MAX_CONTEXT: u32 = 50;
/// Most semantic results.
pub const MAX_K: u32 = 500;
/// Largest `max_results` a client may ask for.
pub const MAX_RESULTS: u32 = 10_000;
/// Largest `max_bytes` a client may ask for.
pub const MAX_BYTES: u32 = 16 * 1024 * 1024;
/// Largest `max_tokens` a client may ask for.
pub const MAX_TOKENS: u32 = 200_000;

fn text(s: &str, max: usize, what: &'static str) -> Result<String> {
    if s.len() > max {
        Err(ConvertError::OutOfRange(what))
    } else {
        Ok(s.to_owned())
    }
}

fn texts(v: &[String], what: &'static str) -> Result<Vec<String>> {
    if v.len() > MAX_LIST {
        return Err(ConvertError::OutOfRange(what));
    }
    v.iter().map(|s| text(s, MAX_TEXT, what)).collect()
}

fn capped(v: u32, max: u32, what: &'static str) -> Result<u32> {
    if v > max {
        Err(ConvertError::OutOfRange(what))
    } else {
        Ok(v)
    }
}

fn opt_path(b: &[u8]) -> Result<Option<RepoPath>> {
    if b.is_empty() {
        Ok(None)
    } else {
        Ok(Some(RepoPath::new(b)?))
    }
}

fn path_to_wire(p: Option<&RepoPath>) -> Bytes {
    p.map_or_else(Bytes::new, |p| Bytes::copy_from_slice(p.as_bytes()))
}

fn opt_mode<T: WireId>(b: &[u8]) -> Result<Option<ModeId<T>>> {
    if b.is_empty() {
        Ok(None)
    } else {
        ModeId::from_wire(b).map(Some)
    }
}

fn mode_to_wire<T: WireId>(id: Option<&ModeId<T>>) -> Bytes {
    id.map_or_else(Bytes::new, ModeId::to_wire)
}

macro_rules! wire_enum {
    ($(#[$m:meta])* $name:ident, $pb:ident { $($v:ident),+ }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub enum $name {
            #[default]
            $($v),+
        }

        impl $name {
            /// Decodes the raw enum number.
            pub fn from_wire(v: i32) -> Result<Self> {
                match pb::$pb::try_from(v) {
                    $(Ok(pb::$pb::$v) => Ok(Self::$v),)+
                    Err(_) => Err(ConvertError::BadEnum { what: stringify!($name), value: v }),
                }
            }

            /// The raw enum number.
            pub fn to_wire(self) -> i32 {
                match self {
                    $(Self::$v => pb::$pb::$v as i32,)+
                }
            }
        }
    };
}

// The first variant is the default: it is the proto zero value.
wire_enum!(
    /// How `Search` reads its pattern.
    SearchMode, SearchMode { Literal, Regex, Word }
);
wire_enum!(
    /// Which symbol query `Symbols` runs.
    SymbolOp, SymbolOp { Def, Refs, Callers, Outline, Find }
);
wire_enum!(
    /// Whether `Semantic` fuses BM25 and symbol hits with the vector list.
    SemanticMode, SemanticMode { Hybrid, Vector }
);

// ----------------------------------------------------------------- Budget

/// Output limits; zero means the server default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    pub max_results: u32,
    pub max_bytes: u32,
    pub max_tokens: u32,
}

impl TryFrom<&pb::Budget> for Budget {
    type Error = ConvertError;

    fn try_from(b: &pb::Budget) -> Result<Self> {
        Ok(Self {
            max_results: capped(b.max_results, MAX_RESULTS, "Budget.max_results")?,
            max_bytes: capped(b.max_bytes, MAX_BYTES, "Budget.max_bytes")?,
            max_tokens: capped(b.max_tokens, MAX_TOKENS, "Budget.max_tokens")?,
        })
    }
}

impl From<&Budget> for pb::Budget {
    fn from(b: &Budget) -> Self {
        Self {
            max_results: b.max_results,
            max_bytes: b.max_bytes,
            max_tokens: b.max_tokens,
        }
    }
}

fn budget(b: Option<&pb::Budget>) -> Result<Budget> {
    b.map_or_else(|| Ok(Budget::default()), Budget::try_from)
}

// ------------------------------------------------------------------ Scope

/// Where a query looks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum At {
    /// Any revspec.
    Rev(String),
    /// A change, resolved to its current head.
    Change(ChangeId),
    /// The top change of a stack.
    StackTop(ChangeId),
    /// The working copy; accepted only over the daemon IPC.
    Workspace,
}

/// The scope of a query (`asc.v1.Scope`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    /// Repository name, `owner/repo`.
    pub repo: String,
    pub id_mode: IdMode,
    pub at: At,
    pub stack_diff_only: bool,
    pub paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub langs: Vec<String>,
    pub include_excluded: bool,
}

impl TryFrom<&pb::Scope> for Scope {
    type Error = ConvertError;

    fn try_from(s: &pb::Scope) -> Result<Self> {
        let repo = s.repo.as_ref().ok_or(ConvertError::Missing("Scope.repo"))?;
        let at = match s.at.as_ref().ok_or(ConvertError::Missing("Scope.at"))? {
            pb::scope::At::Rev(r) if r.is_empty() => {
                return Err(ConvertError::Missing("Scope.rev"));
            }
            pb::scope::At::Rev(r) => At::Rev(text(r, MAX_TEXT, "Scope.rev")?),
            pb::scope::At::Change(c) => At::Change(ChangeId::from_slice(c)?),
            pb::scope::At::StackTop(c) => At::StackTop(ChangeId::from_slice(c)?),
            pb::scope::At::Workspace(true) => At::Workspace,
            pb::scope::At::Workspace(false) => return Err(ConvertError::Missing("Scope.at")),
        };
        Ok(Self {
            repo: text(&repo.name, MAX_TEXT, "RepoRef.name")?,
            id_mode: IdMode::from_wire(repo.id_mode)?,
            at,
            stack_diff_only: s.stack_diff_only,
            paths: texts(&s.paths, "Scope.paths")?,
            exclude_paths: texts(&s.exclude_paths, "Scope.exclude_paths")?,
            langs: texts(&s.langs, "Scope.langs")?,
            include_excluded: s.include_excluded,
        })
    }
}

impl From<&Scope> for pb::Scope {
    fn from(s: &Scope) -> Self {
        let at = match &s.at {
            At::Rev(r) => pb::scope::At::Rev(r.clone()),
            At::Change(c) => pb::scope::At::Change(Bytes::copy_from_slice(c.as_slice())),
            At::StackTop(c) => pb::scope::At::StackTop(Bytes::copy_from_slice(c.as_slice())),
            At::Workspace => pb::scope::At::Workspace(true),
        };
        Self {
            repo: Some(pb::RepoRef {
                name: s.repo.clone(),
                id_mode: s.id_mode.to_wire(),
            }),
            at: Some(at),
            stack_diff_only: s.stack_diff_only,
            paths: s.paths.clone(),
            exclude_paths: s.exclude_paths.clone(),
            langs: s.langs.clone(),
            include_excluded: s.include_excluded,
        }
    }
}

fn scope(s: Option<&pb::Scope>) -> Result<Scope> {
    Scope::try_from(s.ok_or(ConvertError::Missing("scope"))?)
}

// --------------------------------------------------------- ResolvedScope

/// The immutable scope a response was computed against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedScope {
    pub changeset: Option<ModeId<ChangesetId>>,
    pub root: Option<ModeId<TreeId>>,
    pub generation: u64,
    /// Fraction of the scope covered by the index, 0 to 1.
    pub coverage: f32,
}

impl TryFrom<&pb::ResolvedScope> for ResolvedScope {
    type Error = ConvertError;

    fn try_from(r: &pb::ResolvedScope) -> Result<Self> {
        if !(0.0..=1.0).contains(&r.coverage) {
            return Err(ConvertError::OutOfRange("ResolvedScope.coverage"));
        }
        Ok(Self {
            changeset: opt_mode(&r.changeset)?,
            root: opt_mode(&r.root)?,
            generation: r.generation,
            coverage: r.coverage,
        })
    }
}

impl From<&ResolvedScope> for pb::ResolvedScope {
    fn from(r: &ResolvedScope) -> Self {
        Self {
            changeset: mode_to_wire(r.changeset.as_ref()),
            root: mode_to_wire(r.root.as_ref()),
            generation: r.generation,
            coverage: r.coverage,
        }
    }
}

/// Checks a `ResolvedScope` that may be absent.
///
/// # Errors
/// Fails when the present scope is malformed.
pub fn resolved(r: Option<&pb::ResolvedScope>) -> Result<Option<ResolvedScope>> {
    r.map(ResolvedScope::try_from).transpose()
}

// --------------------------------------------------------------- requests

/// A checked `Search` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchRequest {
    pub scope: Scope,
    pub pattern: String,
    pub mode: SearchMode,
    pub case_sensitive: bool,
    pub context_lines: u32,
    pub budget: Budget,
    pub page_token: String,
}

impl TryFrom<&pb::SearchRequest> for SearchRequest {
    type Error = ConvertError;

    fn try_from(r: &pb::SearchRequest) -> Result<Self> {
        if r.pattern.is_empty() {
            return Err(ConvertError::Missing("SearchRequest.pattern"));
        }
        Ok(Self {
            scope: scope(r.scope.as_ref())?,
            pattern: text(&r.pattern, MAX_PATTERN, "SearchRequest.pattern")?,
            mode: SearchMode::from_wire(r.mode)?,
            case_sensitive: r.case_sensitive,
            context_lines: capped(r.context_lines, MAX_CONTEXT, "SearchRequest.context_lines")?,
            budget: budget(r.budget.as_ref())?,
            page_token: text(&r.page_token, MAX_TEXT, "SearchRequest.page_token")?,
        })
    }
}

impl From<&SearchRequest> for pb::SearchRequest {
    fn from(r: &SearchRequest) -> Self {
        Self {
            scope: Some((&r.scope).into()),
            pattern: r.pattern.clone(),
            mode: r.mode.to_wire(),
            case_sensitive: r.case_sensitive,
            context_lines: r.context_lines,
            budget: Some((&r.budget).into()),
            page_token: r.page_token.clone(),
        }
    }
}

/// A checked `Symbols` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolsRequest {
    pub scope: Scope,
    pub op: SymbolOp,
    pub name: String,
    pub path: Option<RepoPath>,
    pub kinds: Vec<String>,
    pub def_path: Option<RepoPath>,
    pub def_line: u32,
    pub budget: Budget,
    pub page_token: String,
}

impl TryFrom<&pb::SymbolsRequest> for SymbolsRequest {
    type Error = ConvertError;

    fn try_from(r: &pb::SymbolsRequest) -> Result<Self> {
        let op = SymbolOp::from_wire(r.op)?;
        let path = opt_path(&r.path)?;
        if op == SymbolOp::Outline {
            if path.is_none() {
                return Err(ConvertError::Missing("SymbolsRequest.path"));
            }
        } else if r.name.is_empty() {
            return Err(ConvertError::Missing("SymbolsRequest.name"));
        }
        Ok(Self {
            scope: scope(r.scope.as_ref())?,
            op,
            name: text(&r.name, MAX_TEXT, "SymbolsRequest.name")?,
            path,
            kinds: texts(&r.kinds, "SymbolsRequest.kinds")?,
            def_path: opt_path(&r.def_path)?,
            def_line: r.def_line,
            budget: budget(r.budget.as_ref())?,
            page_token: text(&r.page_token, MAX_TEXT, "SymbolsRequest.page_token")?,
        })
    }
}

impl From<&SymbolsRequest> for pb::SymbolsRequest {
    fn from(r: &SymbolsRequest) -> Self {
        Self {
            scope: Some((&r.scope).into()),
            op: r.op.to_wire(),
            name: r.name.clone(),
            path: path_to_wire(r.path.as_ref()),
            kinds: r.kinds.clone(),
            def_path: path_to_wire(r.def_path.as_ref()),
            def_line: r.def_line,
            budget: Some((&r.budget).into()),
            page_token: r.page_token.clone(),
        }
    }
}

/// A checked `Semantic` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticRequest {
    pub scope: Scope,
    pub query: String,
    pub mode: SemanticMode,
    /// Zero means the server default.
    pub k: u32,
    pub budget: Budget,
    pub rerank: bool,
}

impl TryFrom<&pb::SemanticRequest> for SemanticRequest {
    type Error = ConvertError;

    fn try_from(r: &pb::SemanticRequest) -> Result<Self> {
        if r.query.is_empty() {
            return Err(ConvertError::Missing("SemanticRequest.query"));
        }
        Ok(Self {
            scope: scope(r.scope.as_ref())?,
            query: text(&r.query, MAX_PATTERN, "SemanticRequest.query")?,
            mode: SemanticMode::from_wire(r.mode)?,
            k: capped(r.k, MAX_K, "SemanticRequest.k")?,
            budget: budget(r.budget.as_ref())?,
            rerank: r.rerank,
        })
    }
}

impl From<&SemanticRequest> for pb::SemanticRequest {
    fn from(r: &SemanticRequest) -> Self {
        Self {
            scope: Some((&r.scope).into()),
            query: r.query.clone(),
            mode: r.mode.to_wire(),
            k: r.k,
            budget: Some((&r.budget).into()),
            rerank: r.rerank,
        }
    }
}

/// A checked `IndexStatus` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexStatusRequest {
    pub scope: Scope,
}

impl TryFrom<&pb::IndexStatusRequest> for IndexStatusRequest {
    type Error = ConvertError;

    fn try_from(r: &pb::IndexStatusRequest) -> Result<Self> {
        Ok(Self {
            scope: scope(r.scope.as_ref())?,
        })
    }
}

impl From<&IndexStatusRequest> for pb::IndexStatusRequest {
    fn from(r: &IndexStatusRequest) -> Self {
        Self {
            scope: Some((&r.scope).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_core::id::GitOid;

    fn ch(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    fn scope_at(at: At) -> Scope {
        Scope {
            repo: "o/r".into(),
            id_mode: IdMode::Git,
            at,
            stack_diff_only: true,
            paths: vec!["src".into()],
            exclude_paths: vec!["vendor".into()],
            langs: vec!["rust".into()],
            include_excluded: false,
        }
    }

    #[test]
    fn scope_round_trips_every_arm() {
        for at in [
            At::Rev("master~2".into()),
            At::Change(ch(3)),
            At::StackTop(ch(4)),
            At::Workspace,
        ] {
            let s = scope_at(at);
            let w = pb::Scope::from(&s);
            assert_eq!(Scope::try_from(&w), Ok(s));
        }
    }

    #[test]
    fn scope_rejects_bad_input() {
        let ok = pb::Scope::from(&scope_at(At::Workspace));
        let mut w = ok.clone();
        w.at = None;
        assert!(Scope::try_from(&w).is_err());
        w.at = Some(pb::scope::At::Workspace(false));
        assert!(Scope::try_from(&w).is_err());
        w.at = Some(pb::scope::At::Change(Bytes::from_static(&[1; 15])));
        assert!(Scope::try_from(&w).is_err());
        w.at = Some(pb::scope::At::Rev("x".repeat(MAX_TEXT + 1)));
        assert!(Scope::try_from(&w).is_err());
        let mut w = ok.clone();
        w.repo = None;
        assert!(Scope::try_from(&w).is_err());
        let mut w = ok.clone();
        w.paths = vec![String::new(); MAX_LIST + 1];
        assert!(Scope::try_from(&w).is_err());
        let mut w = ok;
        w.repo.as_mut().unwrap().id_mode = 99;
        assert!(Scope::try_from(&w).is_err());
    }

    #[test]
    fn search_request_round_trip_and_bounds() {
        let r = SearchRequest {
            scope: scope_at(At::Rev("master".into())),
            pattern: "fn main".into(),
            mode: SearchMode::Word,
            case_sensitive: true,
            context_lines: 2,
            budget: Budget {
                max_results: 10,
                max_bytes: 1000,
                max_tokens: 4000,
            },
            page_token: "tok".into(),
        };
        let w = pb::SearchRequest::from(&r);
        assert_eq!(SearchRequest::try_from(&w), Ok(r));
        let mut bad = w.clone();
        bad.pattern.clear();
        assert!(SearchRequest::try_from(&bad).is_err());
        let mut bad = w.clone();
        bad.context_lines = MAX_CONTEXT + 1;
        assert!(SearchRequest::try_from(&bad).is_err());
        let mut bad = w.clone();
        bad.mode = 9;
        assert!(SearchRequest::try_from(&bad).is_err());
        let mut bad = w.clone();
        bad.budget.as_mut().unwrap().max_tokens = MAX_TOKENS + 1;
        assert!(SearchRequest::try_from(&bad).is_err());
        let mut bad = w;
        bad.scope = None;
        assert!(SearchRequest::try_from(&bad).is_err());
    }

    #[test]
    fn symbols_request_round_trip_and_rules() {
        let r = SymbolsRequest {
            scope: scope_at(At::Change(ch(1))),
            op: SymbolOp::Refs,
            name: "run".into(),
            path: None,
            kinds: vec!["function".into()],
            def_path: Some(RepoPath::new(b"src/lib.rs").unwrap()),
            def_line: 7,
            budget: Budget::default(),
            page_token: String::new(),
        };
        let w = pb::SymbolsRequest::from(&r);
        assert_eq!(SymbolsRequest::try_from(&w), Ok(r));
        let mut bad = w.clone();
        bad.name.clear();
        assert!(SymbolsRequest::try_from(&bad).is_err());
        let mut outline = w.clone();
        outline.op = SymbolOp::Outline.to_wire();
        outline.name.clear();
        assert!(SymbolsRequest::try_from(&outline).is_err());
        outline.path = Bytes::from_static(b"src/lib.rs");
        assert!(SymbolsRequest::try_from(&outline).is_ok());
        let mut bad = w;
        bad.def_path = Bytes::from_static(b"../escape");
        assert!(SymbolsRequest::try_from(&bad).is_err());
    }

    #[test]
    fn semantic_and_status_requests() {
        let r = SemanticRequest {
            scope: scope_at(At::StackTop(ch(2))),
            query: "where is auth checked".into(),
            mode: SemanticMode::Vector,
            k: 10,
            budget: Budget::default(),
            rerank: true,
        };
        let w = pb::SemanticRequest::from(&r);
        assert_eq!(SemanticRequest::try_from(&w), Ok(r));
        let mut bad = w.clone();
        bad.k = MAX_K + 1;
        assert!(SemanticRequest::try_from(&bad).is_err());
        let mut bad = w;
        bad.query.clear();
        assert!(SemanticRequest::try_from(&bad).is_err());

        let s = IndexStatusRequest {
            scope: scope_at(At::Workspace),
        };
        let w = pb::IndexStatusRequest::from(&s);
        assert_eq!(IndexStatusRequest::try_from(&w), Ok(s));
        assert!(IndexStatusRequest::try_from(&pb::IndexStatusRequest { scope: None }).is_err());
    }

    #[test]
    fn resolved_scope_round_trip_and_bounds() {
        let r = ResolvedScope {
            changeset: Some(ModeId::Asc(ChangesetId::from_bytes([5; 32]))),
            root: Some(ModeId::Git(GitOid::from_bytes([6; 20]))),
            generation: 9,
            coverage: 0.5,
        };
        let w = pb::ResolvedScope::from(&r);
        assert_eq!(ResolvedScope::try_from(&w), Ok(r));
        assert_eq!(resolved(None), Ok(None));
        for c in [-0.1_f32, 1.5, f32::NAN] {
            let mut bad = w.clone();
            bad.coverage = c;
            assert!(ResolvedScope::try_from(&bad).is_err());
        }
        let mut bad = w;
        bad.root = Bytes::from_static(&[1; 7]);
        assert!(ResolvedScope::try_from(&bad).is_err());
    }

    #[test]
    fn enums_round_trip_and_reject_unknown_values() {
        assert!(SearchMode::from_wire(3).is_err());
        assert!(SymbolOp::from_wire(5).is_err());
        assert!(SemanticMode::from_wire(2).is_err());
        for m in [SearchMode::Literal, SearchMode::Regex, SearchMode::Word] {
            assert_eq!(SearchMode::from_wire(m.to_wire()), Ok(m));
        }
    }

    #[test]
    fn json_profile_prints_codeintel_messages() {
        use prost::Message as _;
        use prost_reflect::{DescriptorPool, DynamicMessage};
        let pool = DescriptorPool::decode(crate::DESCRIPTOR_SET).unwrap();
        let desc = pool.get_message_by_name("asc.v1.FileHit").unwrap();
        let hit = pb::FileHit {
            path: Bytes::from_static(b"src/a.rs"),
            file_id: Bytes::from_static(&[0xab; 32]),
            ..Default::default()
        };
        let msg = DynamicMessage::decode(desc, hit.encode_to_vec().as_slice()).unwrap();
        let v = crate::json::encode(&msg).unwrap();
        assert_eq!(v["path"], "src/a.rs");
        assert_eq!(v["file_id"], "ab".repeat(32));
    }
}
