//! The server's rules for a push that need no HTTP framework or database (SPEC §25.2 "Writes",
//! §25.5, §25.6, §25.8; PLAN U82).
//!
//! - [`validate_quarantine`]: the hook forwards the directories git imports a push from; the
//!   server reads them with `gix`, so it first proves that they are git's quarantine of *this*
//!   request's view and nothing else.
//! - [`check_stats`]: `max_push_objects` and `max_object_bytes`, from the quarantine, before any
//!   import.
//! - [`PushOptions`]: `asc.ttl=<d>` and `land`, the two push options with a meaning to asc.
//!
//! The messages are the lines the user sees as `remote: …`.

use std::path::{Path, PathBuf};

use super::hook::{HookError, HookRequest};
use super::limits::GitLimits;
use super::server_git::QuarantineStats;

/// Most alternate object directories a hook may name (git itself names one).
pub const MAX_ALTERNATES: usize = 4;
/// The prefixes of git's quarantine directory under `objects/`: git names it
/// `tmp_objdir-incoming-XXXXXX` (the names of its temporary object directories end in `incoming-`
/// for a push); `incoming-*` is accepted as well.
pub const QUARANTINE_PREFIXES: [&str; 2] = ["tmp_objdir-incoming-", "incoming-"];
/// Longest `asc.ttl` accepted: 400 days.
pub const MAX_TTL_SECONDS: i64 = 400 * 24 * 60 * 60;
const SECONDS_PER_UNIT: [(char, i64); 5] = [
    ('s', 1),
    ('m', 60),
    ('h', 3600),
    ('d', 86_400),
    ('w', 604_800),
];

/// A push the server refuses; `Display` is the text for `remote:`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReceiveError {
    #[error("asc: the push does not name git's quarantine for this repository")]
    NotTheQuarantine,
    #[error("asc: push has {found} objects, the limit is {limit}")]
    TooManyObjects { found: u64, limit: u64 },
    #[error("asc: push has an object of {found} bytes, the limit is {limit}")]
    ObjectTooLarge { found: u64, limit: u64 },
    #[error("asc: -o land needs the asc client")]
    LandNeedsAsc,
    #[error("asc: asc.ttl is not a duration like 30m, 12h or 1d (at most 400d)")]
    BadTtl,
    #[error("asc: the push is malformed")]
    Malformed,
}

impl From<HookError> for ReceiveError {
    fn from(_: HookError) -> Self {
        Self::Malformed
    }
}

/// The directories to import a push from, proven to be the quarantine of one view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinePaths {
    /// `<view>/objects/tmp_objdir-incoming-*`.
    pub object_dir: PathBuf,
    /// Object directories git reads through: the view's own `objects`.
    pub alternate_dirs: Vec<PathBuf>,
}

/// Strips the `\\?\` prefix `canonicalize` adds on Windows (but not for UNC paths): the paths go
/// into an alternates file that `gix` reads.
pub fn simplify(p: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && !rest.starts_with("UNC\\")
        {
            return PathBuf::from(rest);
        }
    }
    p
}

fn canonical(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok().map(simplify)
}

/// Checks the object directories a hook forwarded against `view` (the view's git directory):
///
/// - `GIT_QUARANTINE_PATH` is required, is a real directory (not a link) named like git's
///   quarantine ([`QUARANTINE_PREFIXES`]) directly under `<view>/objects`;
/// - `GIT_OBJECT_DIRECTORY`, if forwarded, is that same directory;
/// - every entry of `GIT_ALTERNATE_OBJECT_DIRECTORIES` is `<view>/objects`.
///
/// Anything else (another repository's quarantine, a path elsewhere on the disk, a link) is
/// refused, so a hook that was handed a token cannot make the server read directories of its
/// choosing. Both sides are compared after resolving links.
///
/// A push that only deletes refs carries no pack, and git sets no quarantine for it: `Ok(None)`,
/// and only when no object directory variable is forwarded either. Any command that sets a ref
/// needs its quarantine.
pub fn validate_quarantine(
    view: &Path,
    req: &HookRequest,
) -> Result<Option<QuarantinePaths>, ReceiveError> {
    let refuse = ReceiveError::NotTheQuarantine;
    let objects = canonical(&view.join("objects")).ok_or(refuse.clone())?;
    if req.env_value("GIT_QUARANTINE_PATH").is_none() {
        let deletes_only = !req.commands.is_empty() && req.commands.iter().all(|c| c.new.is_none());
        let no_dirs = req.env_value("GIT_OBJECT_DIRECTORY").is_none()
            && req.env_value("GIT_ALTERNATE_OBJECT_DIRECTORIES").is_none();
        return if deletes_only && no_dirs {
            Ok(None)
        } else {
            Err(refuse)
        };
    }
    let raw = req
        .env_value("GIT_QUARANTINE_PATH")
        .filter(|v| !v.is_empty())
        .ok_or(refuse.clone())?;
    let raw = Path::new(raw);
    if !raw.is_absolute() {
        return Err(refuse);
    }
    // A link in the last component would let the name pass and the content come from elsewhere.
    let meta = std::fs::symlink_metadata(raw).map_err(|_| refuse.clone())?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(refuse);
    }
    let quarantine = canonical(raw).ok_or(refuse.clone())?;
    let named = quarantine
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| QUARANTINE_PREFIXES.iter().any(|p| n.starts_with(p)));
    if quarantine.parent() != Some(objects.as_path()) || !named {
        return Err(refuse);
    }
    if let Some(dir) = req.env_value("GIT_OBJECT_DIRECTORY")
        && canonical(Path::new(dir)).as_deref() != Some(quarantine.as_path())
    {
        return Err(refuse);
    }
    let mut alternate_dirs = Vec::new();
    if let Some(list) = req.env_value("GIT_ALTERNATE_OBJECT_DIRECTORIES") {
        for dir in std::env::split_paths(list) {
            if alternate_dirs.len() == MAX_ALTERNATES {
                return Err(refuse);
            }
            if canonical(&dir).as_deref() != Some(objects.as_path()) {
                return Err(refuse);
            }
            alternate_dirs.push(objects.clone());
        }
    }
    Ok(Some(QuarantinePaths {
        object_dir: quarantine,
        alternate_dirs,
    }))
}

/// The view's own `objects` directory, resolved: what an import of a push without a pack is given
/// as its (unread) object directory.
pub fn view_objects(view: &Path) -> PathBuf {
    canonical(&view.join("objects")).unwrap_or_else(|| view.join("objects"))
}

/// SPEC §25.5: `max_push_objects` and `max_object_bytes`, checked from the quarantine before
/// import.
pub fn check_stats(stats: &QuarantineStats, limits: &GitLimits) -> Result<(), ReceiveError> {
    if stats.objects > limits.max_push_objects {
        return Err(ReceiveError::TooManyObjects {
            found: stats.objects,
            limit: limits.max_push_objects,
        });
    }
    if stats.largest > limits.max_object_bytes {
        return Err(ReceiveError::ObjectTooLarge {
            found: stats.largest,
            limit: limits.max_object_bytes,
        });
    }
    Ok(())
}

/// The push options asc gives a meaning to (SPEC §25.6); the rest are forwarded untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushOptions {
    /// Every option as the client sent it, in order (`GitPushOptions` for Forgejo and
    /// `UpdateRefs`).
    pub all: Vec<String>,
    /// `asc.ttl`, in seconds; zero when absent.
    pub ttl_seconds: i64,
}

/// `30m`, `12h`, `1d`, `2w`, `90s` or a bare number of seconds, positive and at most
/// [`MAX_TTL_SECONDS`].
pub fn parse_ttl(text: &str) -> Option<i64> {
    let (digits, unit) = match text.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (&text[..i], c),
        Some(_) => (text, 's'),
        None => return None,
    };
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    let (_, per) = SECONDS_PER_UNIT.iter().find(|(u, _)| *u == unit)?;
    let secs = n.checked_mul(*per)?;
    (1..=MAX_TTL_SECONDS).contains(&secs).then_some(secs)
}

impl PushOptions {
    /// Reads the options of `req`. `land` (alone or `land=…`) is refused: a plain git push
    /// cannot land a stack (SPEC §25.6). `asc.ttl` must parse; the last one wins.
    pub fn from_request(req: &HookRequest) -> Result<Self, ReceiveError> {
        let all = req.push_options()?;
        let mut ttl_seconds = 0;
        for option in &all {
            let (key, value) = option.split_once('=').unwrap_or((option, ""));
            match key {
                "land" => return Err(ReceiveError::LandNeedsAsc),
                "asc.ttl" => ttl_seconds = parse_ttl(value).ok_or(ReceiveError::BadTtl)?,
                _ => {}
            }
        }
        Ok(Self { all, ttl_seconds })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(env: &[(&str, String)]) -> HookRequest {
        HookRequest {
            token: "0".repeat(64),
            env: env.iter().map(|(k, v)| ((*k).into(), v.clone())).collect(),
            commands: vec![],
        }
    }

    #[test]
    fn a_push_without_a_pack_is_only_ever_deletions() {
        use super::super::hook::HookCommand;
        let oid: asc_core::id::GitOid = "1".repeat(40).parse().unwrap();
        let cmd = |old, new| HookCommand {
            old,
            new,
            name: "refs/heads/x".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("objects")).unwrap();
        let with = |commands: Vec<HookCommand>, env: &[(&str, String)]| HookRequest {
            commands,
            ..req(env)
        };
        // A delete-only push has no quarantine.
        let del = with(vec![cmd(Some(oid), None)], &[]);
        assert_eq!(validate_quarantine(dir.path(), &del), Ok(None));
        // A command that sets a ref needs one, as does a push with no commands at all.
        let set = with(vec![cmd(None, Some(oid))], &[]);
        assert_eq!(
            validate_quarantine(dir.path(), &set),
            Err(ReceiveError::NotTheQuarantine)
        );
        let mixed = with(vec![cmd(Some(oid), None), cmd(None, Some(oid))], &[]);
        assert_eq!(
            validate_quarantine(dir.path(), &mixed),
            Err(ReceiveError::NotTheQuarantine)
        );
        assert_eq!(
            validate_quarantine(dir.path(), &req(&[])),
            Err(ReceiveError::NotTheQuarantine)
        );
        // Object directories without a quarantine are not a delete-only push.
        let odd = with(
            vec![cmd(Some(oid), None)],
            &[("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/elsewhere".into())],
        );
        assert_eq!(
            validate_quarantine(dir.path(), &odd),
            Err(ReceiveError::NotTheQuarantine)
        );
    }

    #[test]
    fn ttl_forms() {
        assert_eq!(parse_ttl("90s"), Some(90));
        assert_eq!(parse_ttl("30m"), Some(1800));
        assert_eq!(parse_ttl("12h"), Some(43_200));
        assert_eq!(parse_ttl("1d"), Some(86_400));
        assert_eq!(parse_ttl("2w"), Some(1_209_600));
        assert_eq!(parse_ttl("45"), Some(45));
        assert_eq!(parse_ttl("400d"), Some(MAX_TTL_SECONDS));
        for bad in [
            "",
            "d",
            "0d",
            "-1d",
            "1x",
            "401d",
            "1.5d",
            "1d1",
            "9999999999d",
            " 1d",
        ] {
            assert_eq!(parse_ttl(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn options_with_meaning() {
        let r = req(&[
            ("GIT_PUSH_OPTION_COUNT", "2".into()),
            ("GIT_PUSH_OPTION_0", "topic=t".into()),
            ("GIT_PUSH_OPTION_1", "asc.ttl=1d".into()),
        ]);
        let o = PushOptions::from_request(&r).unwrap();
        assert_eq!((o.ttl_seconds, o.all.len()), (86_400, 2));
        for land in ["land", "land=x"] {
            let r = req(&[
                ("GIT_PUSH_OPTION_COUNT", "1".into()),
                ("GIT_PUSH_OPTION_0", land.into()),
            ]);
            assert_eq!(
                PushOptions::from_request(&r),
                Err(ReceiveError::LandNeedsAsc)
            );
        }
        let r = req(&[
            ("GIT_PUSH_OPTION_COUNT", "1".into()),
            ("GIT_PUSH_OPTION_0", "asc.ttl=soon".into()),
        ]);
        assert_eq!(PushOptions::from_request(&r), Err(ReceiveError::BadTtl));
        assert_eq!(PushOptions::from_request(&req(&[])).unwrap().ttl_seconds, 0);
    }

    #[test]
    fn limits_are_checked_from_the_quarantine() {
        let l = GitLimits::default();
        let ok = QuarantineStats {
            objects: l.max_push_objects,
            largest: l.max_object_bytes,
        };
        assert_eq!(check_stats(&ok, &l), Ok(()));
        let many = QuarantineStats {
            objects: l.max_push_objects + 1,
            largest: 0,
        };
        assert_eq!(
            check_stats(&many, &l).unwrap_err().to_string(),
            "asc: push has 2000001 objects, the limit is 2000000"
        );
        let big = QuarantineStats {
            objects: 1,
            largest: l.max_object_bytes + 1,
        };
        assert!(matches!(
            check_stats(&big, &l),
            Err(ReceiveError::ObjectTooLarge { .. })
        ));
        let unknown = QuarantineStats {
            objects: u64::MAX,
            largest: 0,
        };
        assert!(check_stats(&unknown, &l).is_err());
    }

    #[test]
    fn only_the_views_own_quarantine_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let view = dir.path().join("1.git");
        let other = dir.path().join("2.git");
        for v in [&view, &other] {
            std::fs::create_dir_all(v.join("objects").join("incoming-abc")).unwrap();
            std::fs::create_dir_all(v.join("objects").join("pack")).unwrap();
        }
        let q = view.join("objects").join("incoming-abc");
        let objects = view.join("objects");
        let sep = if cfg!(windows) { ";" } else { ":" };
        let ok = req(&[
            ("GIT_QUARANTINE_PATH", q.to_string_lossy().into_owned()),
            ("GIT_OBJECT_DIRECTORY", q.to_string_lossy().into_owned()),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                objects.to_string_lossy().into_owned(),
            ),
        ]);
        let got = validate_quarantine(&view, &ok).unwrap().unwrap();
        assert!(got.object_dir.ends_with("incoming-abc"));
        assert_eq!(got.alternate_dirs.len(), 1);

        let with = |k: &str, v: String| {
            req(&[
                ("GIT_QUARANTINE_PATH", q.to_string_lossy().into_owned()),
                (k, v),
            ])
        };
        let refused = [
            // Another repository's quarantine.
            validate_quarantine(&other, &ok),
            // Missing, relative, outside, not named incoming-*, the objects dir itself.
            validate_quarantine(&view, &req(&[])),
            validate_quarantine(
                &view,
                &req(&[("GIT_QUARANTINE_PATH", "objects/incoming-abc".into())]),
            ),
            validate_quarantine(
                &view,
                &req(&[(
                    "GIT_QUARANTINE_PATH",
                    dir.path().to_string_lossy().into_owned(),
                )]),
            ),
            validate_quarantine(
                &view,
                &req(&[(
                    "GIT_QUARANTINE_PATH",
                    view.join("objects")
                        .join("pack")
                        .to_string_lossy()
                        .into_owned(),
                )]),
            ),
            validate_quarantine(
                &view,
                &req(&[(
                    "GIT_QUARANTINE_PATH",
                    objects.to_string_lossy().into_owned(),
                )]),
            ),
            // The quarantine of another repository named by the first one's token.
            validate_quarantine(
                &view,
                &req(&[(
                    "GIT_QUARANTINE_PATH",
                    other
                        .join("objects")
                        .join("incoming-abc")
                        .to_string_lossy()
                        .into_owned(),
                )]),
            ),
            // An object directory that is not the quarantine, an alternate that is not the view.
            validate_quarantine(
                &view,
                &with(
                    "GIT_OBJECT_DIRECTORY",
                    objects.to_string_lossy().into_owned(),
                ),
            ),
            validate_quarantine(
                &view,
                &with(
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    other.join("objects").to_string_lossy().into_owned(),
                ),
            ),
            validate_quarantine(
                &view,
                &with(
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    format!(
                        "{}{sep}{}",
                        objects.display(),
                        other.join("objects").display()
                    ),
                ),
            ),
        ];
        for (i, r) in refused.iter().enumerate() {
            assert_eq!(r, &Err(ReceiveError::NotTheQuarantine), "case {i}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_quarantine_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let view = dir.path().join("1.git");
        std::fs::create_dir_all(view.join("objects")).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let link = view.join("objects").join("incoming-x");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        let r = req(&[("GIT_QUARANTINE_PATH", link.to_string_lossy().into_owned())]);
        assert_eq!(
            validate_quarantine(&view, &r),
            Err(ReceiveError::NotTheQuarantine)
        );
    }
}
