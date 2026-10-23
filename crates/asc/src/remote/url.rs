//! Remote URLs (SPEC §6.2): what a user-typed URL names and how a bucket URL is normalised.
//!
//! A URL is untrusted text. Parsing never touches the network; only the classification of an
//! absolute local path looks at the file system (a git directory is a git remote, anything else
//! a bucket-layout directory).

use std::path::Path;

use asc_core::transport_policy::{TransportError, check};

/// Longest URL accepted.
pub const MAX_URL: usize = 8192;

/// Who holds a bucket's refs (SPEC §3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefsMode {
    /// `--refs local`: single writer, the refs live in this machine's `state.db`.
    Local,
    /// `BucketRefs` (U30): refs live in the bucket as a create-only log.
    Shared,
}

/// What a remote URL names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteSpec {
    /// `s3://…`, `file:///…` or an absolute bucket-layout directory. `url` is normalised.
    Bucket { url: String, refs: RefsMode },
    /// `git+…`, `git@host:path`, `ssh://…`, a relative or git-directory path.
    Git { url: String },
    /// `asc+https://…`: `url` is the transport URL without the `asc+` prefix.
    Server { url: String },
    /// `https://…`: discovery (`/.well-known/asc`) is stubbed to git until U50.
    Discover { url: String },
}

impl RemoteSpec {
    /// The kind word `asc remote -v` and error messages use.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Bucket { .. } => "bucket",
            Self::Git { .. } => "git",
            Self::Server { .. } => "server",
            Self::Discover { .. } => "discover",
        }
    }
}

/// Why a URL was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UrlError {
    #[error("the remote URL is empty")]
    Empty,
    #[error("the remote URL is longer than {MAX_URL} bytes")]
    TooLong,
    #[error("the remote URL contains a control character")]
    Control,
    #[error("the remote URL '{0}' looks like an option")]
    LooksLikeOption(String),
    #[error("'{0}' is a network share; use an asc server or an s3:// bucket")]
    NetworkShare(String),
    #[error("invalid bucket URL '{0}': {1}")]
    BadBucket(String, String),
    /// Plain http, `git://` or another transport asc does not speak (U89).
    #[error(transparent)]
    Transport(#[from] TransportError),
}

fn is_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

fn is_absolute_path(s: &str) -> bool {
    s.starts_with('/') || is_drive_path(s) || s.starts_with("\\\\")
}

fn is_unc(s: &str) -> bool {
    s.starts_with("\\\\") || s.starts_with("//")
}

/// Whether `dir` is a git repository (work tree or bare), by its layout.
fn is_git_dir(dir: &Path) -> bool {
    let git = dir.join(".git");
    if std::fs::symlink_metadata(&git).is_ok() {
        return true;
    }
    dir.join("HEAD").is_file() && dir.join("objects").is_dir()
}

/// The normalised `file:///…` URL of an absolute local path.
fn file_url(path: &str) -> Result<String, UrlError> {
    if is_unc(path) {
        return Err(UrlError::NetworkShare(crate::cli::url_display::anonymize(
            path,
        )));
    }
    let p = Path::new(path);
    url::Url::from_file_path(p)
        .map(|u| u.to_string().trim_end_matches('/').to_owned())
        .map_err(|()| {
            UrlError::BadBucket(
                crate::cli::url_display::anonymize(path),
                "not an absolute path".into(),
            )
        })
}

/// Normalises `file://` and `s3://` URLs: no trailing slash, no query on file URLs.
fn normalise_bucket(raw: &str) -> Result<String, UrlError> {
    let bad = |why: String| UrlError::BadBucket(crate::cli::url_display::anonymize(raw), why);
    let mut parsed = url::Url::parse(raw).map_err(|e| bad(e.to_string()))?;
    match parsed.scheme() {
        "file" => {
            match parsed.host_str() {
                None | Some("") | Some("localhost") => {}
                Some(_) => {
                    return Err(UrlError::NetworkShare(crate::cli::url_display::anonymize(
                        raw,
                    )));
                }
            }
            let path = parsed.path();
            if path.is_empty() || path == "/" {
                return Err(bad("the path is empty".into()));
            }
            let decoded = url::Url::from_file_path(
                parsed
                    .to_file_path()
                    .map_err(|()| bad("not a local path".into()))?,
            )
            .map_err(|()| bad("not a local path".into()))?;
            parsed = decoded;
            parsed.set_query(None);
            parsed.set_fragment(None);
        }
        "s3" => {
            if parsed.host_str().is_none_or(str::is_empty) {
                return Err(bad("the bucket name is missing".into()));
            }
            parsed.set_fragment(None);
        }
        other => return Err(bad(format!("unsupported scheme '{other}'"))),
    }
    let text = parsed.to_string();
    Ok(match text.strip_suffix('/') {
        Some(t) if !t.ends_with(':') && !t.ends_with('/') => t.to_owned(),
        _ => text,
    })
}

/// Parses `input` (SPEC §6.2). `refs` is the `remote.<n>.ascRefs` setting (default `Local`).
pub fn parse(input: &str, refs: RefsMode) -> Result<RemoteSpec, UrlError> {
    if input.is_empty() {
        return Err(UrlError::Empty);
    }
    if input.len() > MAX_URL {
        return Err(UrlError::TooLong);
    }
    if input.chars().any(char::is_control) {
        return Err(UrlError::Control);
    }
    if input.starts_with('-') {
        return Err(UrlError::LooksLikeOption(
            crate::cli::url_display::anonymize(input),
        ));
    }
    let lower = input.to_ascii_lowercase();
    if lower.starts_with("s3://") || lower.starts_with("file://") {
        return Ok(RemoteSpec::Bucket {
            url: normalise_bucket(input)?,
            refs,
        });
    }
    // asc talks https or ssh: plain http (loopback only, and only under the opt-in), `git://` and
    // every other network scheme are refused before anything is stored or contacted (U89).
    let network = ["asc+", "git+", "ssh://", "git://", "https://", "http://"]
        .iter()
        .any(|p| lower.starts_with(p));
    if network {
        check(input)?;
    }
    for prefix in ["asc+https://", "asc+http://"] {
        if lower.starts_with(prefix) {
            return Ok(RemoteSpec::Server {
                url: input[3..].to_owned(),
            });
        }
    }
    if lower.starts_with("git+") || lower.starts_with("ssh://") || lower.starts_with("git://") {
        return Ok(RemoteSpec::Git {
            url: input.to_owned(),
        });
    }
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return Ok(RemoteSpec::Discover {
            url: input.to_owned(),
        });
    }
    if is_absolute_path(input) {
        if is_unc(input) {
            return Err(UrlError::NetworkShare(crate::cli::url_display::anonymize(
                input,
            )));
        }
        if is_git_dir(Path::new(input)) {
            return Ok(RemoteSpec::Git {
                url: input.to_owned(),
            });
        }
        return Ok(RemoteSpec::Bucket {
            url: file_url(input)?,
            refs,
        });
    }
    // `git@host:path` and every relative path are git remotes.
    Ok(RemoteSpec::Git {
        url: input.to_owned(),
    })
}

/// The directory name `clone` uses for `url`: the last path segment without `.git`.
pub fn default_dir_name(url: &str) -> String {
    let trimmed = url.trim_end_matches(['/', '\\']);
    let trimmed = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    let last = trimmed
        .rsplit(['/', '\\', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or("repo");
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() || name == "." || name == ".." {
        "repo".to_owned()
    } else {
        name.to_owned()
    }
}

/// The URL git prints after `From`/`To`: the stored URL without a trailing slash or `.git`
/// for fetch, as typed for push.
pub fn display_from(url: &str) -> String {
    let anon = crate::cli::url_display::anonymize(url);
    let t = anon.trim_end_matches('/');
    t.strip_suffix(".git").unwrap_or(t).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `file://` URL for the absolute `path` on this platform (Windows needs a drive).
    fn file(path: &str) -> String {
        if cfg!(windows) {
            format!("file:///C:{path}")
        } else {
            format!("file://{path}")
        }
    }

    #[test]
    fn classifies_the_spec_table() {
        let l = RefsMode::Local;
        assert!(matches!(
            parse("s3://b/p?endpoint=http://x", l),
            Ok(RemoteSpec::Bucket { .. })
        ));
        assert!(matches!(
            parse(&file("/tmp/b"), l),
            Ok(RemoteSpec::Bucket { .. })
        ));
        assert_eq!(
            parse("asc+https://h:7/o/r", l),
            Ok(RemoteSpec::Server {
                url: "https://h:7/o/r".into()
            })
        );
        assert!(matches!(
            parse("git+ssh://h/r", l),
            Ok(RemoteSpec::Git { .. })
        ));
        assert!(matches!(
            parse("git@host:o/r.git", l),
            Ok(RemoteSpec::Git { .. })
        ));
        assert!(matches!(
            parse("https://h/o/r.git", l),
            Ok(RemoteSpec::Discover { .. })
        ));
        assert!(matches!(
            parse("../elsewhere", l),
            Ok(RemoteSpec::Git { .. })
        ));
    }

    #[test]
    fn plain_http_and_the_git_protocol_are_refused() {
        // Non-loopback cases only: they are refused whatever the opt-in says.
        for bad in [
            "http://example.com/o/r",
            "asc+http://example.com",
            "git+http://example.com/o/r.git",
            "git://example.com/o/r.git",
            "git+git://example.com/o/r.git",
            "http://u:hunter2@example.com:81/o/r?t=abc",
        ] {
            let err = parse(bad, RefsMode::Local).expect_err(bad);
            assert!(matches!(err, UrlError::Transport(_)), "{bad}: {err}");
            let text = err.to_string();
            assert!(!text.contains("hunter2") && !text.contains("abc"), "{text}");
        }
        assert_eq!(
            parse("git@example.com:o/r.git", RefsMode::Local),
            Ok(RemoteSpec::Git {
                url: "git@example.com:o/r.git".into()
            })
        );
        assert!(parse("ssh://git@example.com/o/r.git", RefsMode::Local).is_ok());
        assert!(parse("git+ssh://example.com/o/r.git", RefsMode::Local).is_ok());
    }

    #[test]
    fn refuses_hostile_text() {
        let l = RefsMode::Local;
        assert_eq!(parse("", l), Err(UrlError::Empty));
        assert_eq!(parse("a\nb", l), Err(UrlError::Control));
        assert!(matches!(
            parse("--upload-pack=x", l),
            Err(UrlError::LooksLikeOption(_))
        ));
        assert!(matches!(
            parse("file://host/share/x", l),
            Err(UrlError::NetworkShare(_))
        ));
        assert!(matches!(
            parse("\\\\host\\share", l),
            Err(UrlError::NetworkShare(_))
        ));
        assert_eq!(parse(&"a".repeat(MAX_URL + 1), l), Err(UrlError::TooLong));
        assert!(matches!(parse("s3:///x", l), Err(UrlError::BadBucket(..))));
    }

    #[test]
    fn an_absolute_directory_is_a_bucket_unless_it_is_a_git_dir() {
        let t = tempfile::tempdir().unwrap();
        let bucket = t.path().join("bucket");
        std::fs::create_dir(&bucket).unwrap();
        let Ok(RemoteSpec::Bucket { url, refs }) = parse(bucket.to_str().unwrap(), RefsMode::Local)
        else {
            panic!("expected a bucket");
        };
        assert!(url.starts_with("file:///"), "{url}");
        assert_eq!(refs, RefsMode::Local);
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(matches!(
            parse(repo.to_str().unwrap(), RefsMode::Local),
            Ok(RemoteSpec::Git { .. })
        ));
    }

    #[test]
    fn bucket_urls_are_normalised() {
        let a = parse(&format!("{}/", file("/tmp/b")), RefsMode::Shared).unwrap();
        let b = parse(&file("/tmp/b"), RefsMode::Shared).unwrap();
        assert_eq!(a, b);
        assert_eq!(
            parse("s3://bucket/pre/", RefsMode::Local).unwrap(),
            RemoteSpec::Bucket {
                url: "s3://bucket/pre".into(),
                refs: RefsMode::Local
            }
        );
    }

    #[test]
    fn directory_names() {
        assert_eq!(default_dir_name(&file("/tmp/bucket")), "bucket");
        assert_eq!(default_dir_name("https://h/o/r.git"), "r");
        assert_eq!(default_dir_name("s3://b/"), "b");
        assert_eq!(default_dir_name("git@h:o/r.git/"), "r");
        assert_eq!(display_from("/x/y.git/"), "/x/y");
    }
}
