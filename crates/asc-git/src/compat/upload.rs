//! The upload-pack side of the git protocol layer that needs no HTTP framework (SPEC §25.2,
//! §25.4, §25.5, §25.8 item 1).
//!
//! `asc-serve::git_http` owns the routes, the authorisation and the streaming; this module owns
//! the rules that decide what a request may *be*:
//!
//! - [`RepoPath`]: the strict repository name of `/<owner>/<name>[.git]/…` (a name outside
//!   `[A-Za-z0-9._-]{1,100}`, a leading dot, a trailing dot, a `%`, a NUL or a backslash is
//!   refused, so URL text never reaches the file system);
//! - [`Endpoint`]: the three suffixes that are served, nothing else;
//! - the media types of the three message kinds and the `Content-Type` check;
//! - [`prepare_upload_body`]: the negotiation body of a fetch, inflated under the cap (a
//!   `Content-Encoding: gzip` body that expands past it is cut, never buffered whole), and the
//!   `0000` probe recognised before any child starts.
//!
//! Nothing here starts a process or touches the file system.

use std::io::Read;

use super::pktline::{GitProtocol, ProtocolVersion, Service, is_probe, service_advert_prefix};

/// Longest repository or owner name (SPEC §25.8 item 1).
pub const MAX_NAME_BYTES: usize = 100;

/// Hidden from every advertisement (`transfer.hideRefs`): pull, user and snapshot refs.
pub const HIDDEN_REF_PREFIXES: [&str; 3] = ["refs/pull", "refs/users", "refs/snapshots"];

/// Whether `name` is under a hidden prefix (`refs/pull/1/head`, `refs/users/7/heads/x`, …).
pub fn is_hidden_ref(name: &str) -> bool {
    HIDDEN_REF_PREFIXES.iter().any(|p| {
        name.strip_prefix(p)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// The route suffixes that are served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// `GET …/info/refs?service=…`.
    InfoRefs,
    /// `POST …/git-upload-pack`.
    UploadPack,
    /// `POST …/git-receive-pack`.
    ReceivePack,
}

impl Endpoint {
    /// `info/refs`, `git-upload-pack` or `git-receive-pack`, exactly. Anything else (the dumb
    /// protocol's `HEAD` and `objects/…`, `git-upload-archive`) does not parse.
    pub fn parse(suffix: &str) -> Option<Self> {
        match suffix {
            "info/refs" => Some(Self::InfoRefs),
            "git-upload-pack" => Some(Self::UploadPack),
            "git-receive-pack" => Some(Self::ReceivePack),
            _ => None,
        }
    }

    /// The pack service a POST endpoint names.
    pub fn service(self) -> Option<Service> {
        match self {
            Self::InfoRefs => None,
            Self::UploadPack => Some(Service::UploadPack),
            Self::ReceivePack => Some(Service::ReceivePack),
        }
    }
}

/// Why a repository path is refused. The caller answers every one of them `404`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("a path segment is empty")]
    Empty,
    #[error("a path segment is longer than {MAX_NAME_BYTES} bytes")]
    TooLong,
    #[error("a path segment has a character outside [A-Za-z0-9._-]")]
    BadCharacter,
    #[error("a path segment starts or ends with a dot")]
    DotEdge,
}

/// The repository of a git URL, `owner/name`, validated.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoPath {
    owner: String,
    name: String,
}

fn segment(s: &str) -> Result<(), PathError> {
    if s.is_empty() {
        return Err(PathError::Empty);
    }
    if s.len() > MAX_NAME_BYTES {
        return Err(PathError::TooLong);
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(PathError::BadCharacter);
    }
    if s.starts_with('.') || s.ends_with('.') {
        return Err(PathError::DotEdge);
    }
    Ok(())
}

impl RepoPath {
    /// Validates the two path segments after the router decoded them once. One trailing `.git`
    /// is taken off `repo` (the suffix is optional on the URL). A `%` that is still there after
    /// the one decoding is a `BadCharacter`, so `%252e%252e` never means `..`.
    pub fn parse(owner: &str, repo: &str) -> Result<Self, PathError> {
        let name = repo.strip_suffix(".git").unwrap_or(repo);
        segment(owner)?;
        segment(name)?;
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `owner/name`, the string the `Authorizer` and the registry take.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// `application/x-git-upload-pack-advertisement` for `info/refs`.
pub fn advertisement_type(service: Service) -> String {
    format!("application/x-{}-advertisement", service.name())
}

/// `application/x-git-upload-pack-request`: the type a POST must carry.
pub fn request_type(service: Service) -> String {
    format!("application/x-{}-request", service.name())
}

/// `application/x-git-upload-pack-result`: the type of the response to a POST.
pub fn result_type(service: Service) -> String {
    format!("application/x-{}-result", service.name())
}

/// Whether the `Content-Type` header names `expected` (case-insensitive, parameters ignored).
pub fn content_type_matches(header: Option<&str>, expected: &str) -> bool {
    header
        .and_then(|h| h.split(';').next())
        .is_some_and(|m| m.trim().eq_ignore_ascii_case(expected))
}

/// The bytes that go before the child's advertisement on `info/refs`: `# service=…` and a flush
/// for protocol v0 and v1, nothing for v2 (SPEC §25.1).
pub fn advertisement_prefix(service: Service, protocol: Option<&GitProtocol>) -> Vec<u8> {
    let v2 = protocol.is_some_and(|p| p.version() == ProtocolVersion::V2);
    if v2 {
        Vec::new()
    } else {
        service_advert_prefix(service)
    }
}

/// How a POST body is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyEncoding {
    Identity,
    Gzip,
}

/// Why a request body was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BodyError {
    /// `Content-Encoding` other than `gzip` or `identity` (a `415`).
    #[error("content encoding is not supported")]
    UnsupportedEncoding,
    /// More than the cap, before or after inflating (a `413`).
    #[error("request body is over the limit")]
    TooLarge,
    /// The gzip stream is damaged (a `400`).
    #[error("request body is not valid gzip")]
    Corrupt,
    /// No bytes at all (a `400`).
    #[error("request body is empty")]
    Empty,
}

/// The encoding named by a `Content-Encoding` header. git sends `gzip` for a large negotiation;
/// nothing else is accepted.
pub fn body_encoding(header: Option<&str>) -> Result<BodyEncoding, BodyError> {
    match header.map(|h| h.trim().to_ascii_lowercase()) {
        None => Ok(BodyEncoding::Identity),
        Some(h) if h.is_empty() || h == "identity" => Ok(BodyEncoding::Identity),
        Some(h) if h == "gzip" || h == "x-gzip" => Ok(BodyEncoding::Gzip),
        Some(_) => Err(BodyError::UnsupportedEncoding),
    }
}

/// Inflates a gzip body, stopping one byte past `cap` so a bomb costs `cap` bytes of memory and
/// no more.
pub fn inflate_gzip(body: &[u8], cap: u64) -> Result<Vec<u8>, BodyError> {
    let mut out = Vec::new();
    let mut dec = flate2::read::GzDecoder::new(body).take(cap.saturating_add(1));
    dec.read_to_end(&mut out).map_err(|_| BodyError::Corrupt)?;
    if out.len() as u64 > cap {
        return Err(BodyError::TooLarge);
    }
    Ok(out)
}

/// What a `git-upload-pack` POST asks of the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadBody {
    /// The bare `0000` that git sends first to trigger authentication before it uploads a large
    /// request. Answered without a child (SPEC §25.5).
    Probe,
    /// The negotiation to feed the child, inflated.
    Request(Vec<u8>),
}

/// Decodes and bounds the body of a fetch POST. `raw` is what arrived (the caller stops reading
/// at `cap` bytes), `cap` is `fetch_body_bytes`.
pub fn prepare_upload_body(
    raw: Vec<u8>,
    encoding: BodyEncoding,
    cap: u64,
) -> Result<UploadBody, BodyError> {
    if raw.len() as u64 > cap {
        return Err(BodyError::TooLarge);
    }
    let body = match encoding {
        BodyEncoding::Identity => raw,
        BodyEncoding::Gzip => inflate_gzip(&raw, cap)?,
    };
    if body.is_empty() {
        return Err(BodyError::Empty);
    }
    if is_probe(&body) {
        return Ok(UploadBody::Probe);
    }
    Ok(UploadBody::Request(body))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn plain_names_parse_with_or_without_the_git_suffix() {
        let p = RepoPath::parse("acme", "web.git").unwrap();
        assert_eq!(p.full_name(), "acme/web");
        assert_eq!(RepoPath::parse("acme", "web").unwrap(), p);
        assert_eq!(
            RepoPath::parse("a.b-c_d", "x.git.git").unwrap().name(),
            "x.git"
        );
        let longest = "n".repeat(MAX_NAME_BYTES);
        assert!(RepoPath::parse("acme", &longest).is_ok());
    }

    #[test]
    fn hostile_names_are_refused() {
        let long = "n".repeat(MAX_NAME_BYTES + 1);
        for (owner, repo) in [
            ("acme", ".."),
            ("..", "web"),
            ("acme", "."),
            ("acme", ".hidden"),
            ("acme", "web."),
            ("acme", "we b"),
            ("acme", "web "),
            ("acme", "w%2eb"),
            ("acme", "w%00b"),
            ("acme", "w\0b"),
            ("acme", "w\\b"),
            ("acme", "w/b"),
            ("acme", ""),
            ("acme", ".git"),
            ("", "web"),
            ("acme", long.as_str()),
            ("acme", "web..git"),
            ("ac:me", "web"),
        ] {
            assert!(RepoPath::parse(owner, repo).is_err(), "{owner:?} {repo:?}");
        }
    }

    #[test]
    fn only_the_three_suffixes_are_served() {
        assert_eq!(Endpoint::parse("info/refs"), Some(Endpoint::InfoRefs));
        assert_eq!(
            Endpoint::parse("git-upload-pack"),
            Some(Endpoint::UploadPack)
        );
        assert_eq!(
            Endpoint::parse("git-receive-pack"),
            Some(Endpoint::ReceivePack)
        );
        for other in [
            "git-upload-archive",
            "HEAD",
            "objects/info/packs",
            "info/refs/",
            "Info/refs",
            "",
        ] {
            assert_eq!(Endpoint::parse(other), None, "{other}");
        }
    }

    #[test]
    fn hidden_refs_are_the_pull_user_and_snapshot_trees() {
        for hidden in [
            "refs/pull/1/head",
            "refs/users/7/heads/x",
            "refs/snapshots/a",
            "refs/pull",
        ] {
            assert!(is_hidden_ref(hidden), "{hidden}");
        }
        for shown in [
            "refs/heads/master",
            "refs/tags/v1",
            "refs/pullrequests/1",
            "refs/heads/refs/pull",
        ] {
            assert!(!is_hidden_ref(shown), "{shown}");
        }
    }

    #[test]
    fn content_types_match_ignoring_case_and_parameters() {
        let want = request_type(Service::UploadPack);
        assert_eq!(want, "application/x-git-upload-pack-request");
        assert!(content_type_matches(Some(&want), &want));
        assert!(content_type_matches(
            Some("Application/X-Git-Upload-Pack-Request; charset=x"),
            &want
        ));
        assert!(!content_type_matches(Some("text/plain"), &want));
        assert!(!content_type_matches(None, &want));
        assert_eq!(
            advertisement_type(Service::ReceivePack),
            "application/x-git-receive-pack-advertisement"
        );
        assert_eq!(
            result_type(Service::UploadPack),
            "application/x-git-upload-pack-result"
        );
    }

    #[test]
    fn the_service_prefix_is_for_v0_and_v1_only() {
        let v2 = GitProtocol::parse("version=2");
        let v1 = GitProtocol::parse("version=1");
        assert!(advertisement_prefix(Service::UploadPack, v2.as_ref()).is_empty());
        let expected = b"001e# service=git-upload-pack\n0000".to_vec();
        assert_eq!(
            advertisement_prefix(Service::UploadPack, v1.as_ref()),
            expected
        );
        assert_eq!(advertisement_prefix(Service::UploadPack, None), expected);
    }

    #[test]
    fn encodings_are_gzip_or_identity() {
        assert_eq!(body_encoding(None), Ok(BodyEncoding::Identity));
        assert_eq!(body_encoding(Some("identity")), Ok(BodyEncoding::Identity));
        assert_eq!(body_encoding(Some("GZIP")), Ok(BodyEncoding::Gzip));
        for bad in ["br", "deflate", "gzip, br", "zstd"] {
            assert_eq!(
                body_encoding(Some(bad)),
                Err(BodyError::UnsupportedEncoding),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_probe_is_recognised_plain_or_gzipped_and_a_request_passes_through() {
        let cap = 1024;
        assert_eq!(
            prepare_upload_body(b"0000".to_vec(), BodyEncoding::Identity, cap),
            Ok(UploadBody::Probe)
        );
        assert_eq!(
            prepare_upload_body(gzip(b"0000"), BodyEncoding::Gzip, cap),
            Ok(UploadBody::Probe)
        );
        let want = b"0032want 0123456789012345678901234567890123456789\n00000009done\n".to_vec();
        assert_eq!(
            prepare_upload_body(gzip(&want), BodyEncoding::Gzip, cap),
            Ok(UploadBody::Request(want.clone()))
        );
        assert_eq!(
            prepare_upload_body(want.clone(), BodyEncoding::Identity, cap),
            Ok(UploadBody::Request(want))
        );
        assert_eq!(
            prepare_upload_body(Vec::new(), BodyEncoding::Identity, cap),
            Err(BodyError::Empty)
        );
    }

    #[test]
    fn a_gzip_bomb_is_cut_at_the_cap() {
        let cap = 64 * 1024;
        let bomb = gzip(&vec![0u8; 64 * 1024 * 1024]);
        assert!(bomb.len() < 200 * 1024, "the bomb is small on the wire");
        assert_eq!(
            prepare_upload_body(bomb, BodyEncoding::Gzip, cap),
            Err(BodyError::TooLarge)
        );
        // Exactly the cap is fine; one more byte is not.
        assert!(prepare_upload_body(gzip(&[b'a'; 64]), BodyEncoding::Gzip, 64).is_ok());
        assert_eq!(
            prepare_upload_body(gzip(&[b'a'; 65]), BodyEncoding::Gzip, 64),
            Err(BodyError::TooLarge)
        );
        assert_eq!(
            prepare_upload_body(vec![b'a'; 65], BodyEncoding::Identity, 64),
            Err(BodyError::TooLarge)
        );
    }

    #[test]
    fn damaged_gzip_is_corrupt() {
        assert_eq!(
            prepare_upload_body(b"not gzip at all".to_vec(), BodyEncoding::Gzip, 1024),
            Err(BodyError::Corrupt)
        );
        let mut cut = gzip(&vec![b'z'; 4096]);
        cut.truncate(cut.len() / 2);
        assert_eq!(
            prepare_upload_body(cut, BodyEncoding::Gzip, 1 << 20),
            Err(BodyError::Corrupt)
        );
    }
}
