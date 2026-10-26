//! `asc upgrade [--check] [--from <dir>]` (SPEC §16.2): signature-verified, side-by-side upgrade.
//!
//! Layout: `<prefix>/<ver>/asc[.exe]` and `<prefix>/<ver>/shim/` (always created; `git[.exe]`
//! is linked into it only when the running version had the shim), with `<prefix>/current` a
//! directory junction (symlink on Unix) to the active version. An upgrade verifies the minisign
//! signature before it unpacks anything, writes the new directory beside the old one, relinks
//! the shim, switches `current`, then asks the running daemon to `Shutdown`. Old directories
//! are pruned at the next upgrade (Windows cannot delete a running executable).
//!
//! The release source is `asc.releasesUrl` (https only), else the `ASC_DOWNLOAD_BASE_URL`
//! environment variable (the same value the installers read), or a directory given with `--from`:
//! `latest/version` (a version), `<ver>/asc-<ver>-<target>.zip` and `<ver>/asc-<ver>-<target>.zip.minisig`.
//! Everything fetched is untrusted and size-bounded; the public key is baked in at build time
//! (`ASC_RELEASE_PUBKEY`), so a build without it refuses to upgrade.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use minisign_verify::{PublicKey, Signature};

use asc_core::transport_policy::{Policy, Transport, require_network};

use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "upgrade",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    /// Only look for a newer version and say so.
    #[arg(long)]
    pub check: bool,
    /// A directory laid out like the releases URL (offline and fleet installs).
    #[arg(long, value_name = "DIR")]
    pub from: Option<PathBuf>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "upgrade",
    flags: &["--check", "--from"],
    placeholders: &[],
}];

const PUBKEY: Option<&str> = option_env!("ASC_RELEASE_PUBKEY");
const MAX_ARTEFACT: u64 = 256 << 20;
const MAX_UNPACKED: u64 = 512 << 20;
const MAX_ENTRIES: usize = 64;
const MAX_SMALL: u64 = 16 << 10;
const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
const EXE: &str = if cfg!(windows) { "asc.exe" } else { "asc" };
const GIT_FILE: &str = if cfg!(windows) { "git.exe" } else { "git" };

const TARGET: &str = if cfg!(all(windows, target_arch = "x86_64")) {
    "x86_64-pc-windows-msvc"
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    "x86_64-unknown-linux-gnu"
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    "aarch64-unknown-linux-gnu"
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    "aarch64-apple-darwin"
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    "x86_64-apple-darwin"
} else {
    "unknown"
};

#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

type UResult<T> = std::result::Result<T, UpgradeError>;

fn refuse<T>(text: impl Into<String>) -> UResult<T> {
    Err(UpgradeError::Refused(text.into()))
}

/// A `MAJOR.MINOR.PATCH` version. Nothing else is accepted, because a version names a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u32, u32, u32);

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let mut next = || {
            let p = parts.next()?;
            let ok = !p.is_empty()
                && p.len() <= 9
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p == "0" || !p.starts_with('0'));
            if ok { p.parse::<u32>().ok() } else { None }
        };
        let v = Self(next()?, next()?, next()?);
        parts.next().is_none().then_some(v)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The `CLIENT_TOO_OLD` message for a server whose `Version.min_client` is newer (SPEC §16.2).
pub fn client_too_old(client: &str, min_client: &str) -> Option<String> {
    let (c, m) = (Version::parse(client)?, Version::parse(min_client)?);
    (c < m).then(|| {
        format!("CLIENT_TOO_OLD: asc {c} is older than the server's minimum {m}; run `asc upgrade`")
    })
}

fn artefact_name(v: Version) -> String {
    format!("asc-{v}-{TARGET}.zip")
}

/// A minisign-verified artefact. Only [`verify`] makes one.
pub struct Verified(Vec<u8>);

/// Verifies `bytes` against `sig` with `key`. The trusted comment must name `file:<expected>`,
/// so a validly signed older artefact cannot be replayed as another version.
pub fn verify(
    bytes: &[u8],
    sig: &str,
    key: &str,
    expected: &str,
    allow_legacy: bool,
) -> UResult<Verified> {
    let key = PublicKey::from_base64(key)
        .map_err(|e| UpgradeError::Refused(format!("bad release key: {e}")))?;
    let sig = Signature::decode(sig)
        .map_err(|e| UpgradeError::Refused(format!("bad signature file: {e}")))?;
    key.verify(bytes, &sig, allow_legacy).map_err(|e| {
        UpgradeError::Refused(format!(
            "signature check failed, nothing was installed: {e}"
        ))
    })?;
    let named = sig
        .trusted_comment()
        .split(['\t', ' '])
        .any(|t| t.strip_prefix("file:") == Some(expected));
    if !named {
        return refuse(format!("the signature is not for {expected}"));
    }
    Ok(Verified(bytes.to_vec()))
}

/// Where releases come from.
pub enum Source {
    Dir(PathBuf),
    Url(String),
}

impl Source {
    fn url(base: &str) -> UResult<Self> {
        Self::url_under(base, Policy::current())
    }

    /// https; plain http to the local machine only under the transport policy's opt-in (U89).
    fn url_under(base: &str, policy: Policy) -> UResult<Self> {
        match require_network(base, policy) {
            Ok(Transport::Https | Transport::LoopbackHttp) => {}
            Ok(_) => return refuse("asc.releasesUrl must be an https URL"),
            Err(why) => return refuse(format!("asc.releasesUrl: {why}")),
        }
        Ok(Self::Url(base.trim_end_matches('/').to_owned()))
    }

    /// Reads `rel` (built only from validated parts), at most `max` bytes.
    async fn fetch(&self, rel: &str, max: u64) -> UResult<Vec<u8>> {
        match self {
            Self::Dir(d) => {
                let p = d.join(rel);
                let meta = std::fs::symlink_metadata(&p)?;
                if !meta.is_file() || meta.len() > max {
                    return refuse(format!(
                        "{} is not a regular file within {max} bytes",
                        p.display()
                    ));
                }
                let mut out = Vec::new();
                std::fs::File::open(&p)?
                    .take(max + 1)
                    .read_to_end(&mut out)?;
                Ok(out)
            }
            Self::Url(base) => {
                // A plain-http source passed `url_under` as loopback: it follows no redirect.
                let plain = base
                    .get(..5)
                    .is_some_and(|s| s.eq_ignore_ascii_case("http:"));
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(300))
                    .https_only(!plain)
                    .redirect(if plain {
                        reqwest::redirect::Policy::none()
                    } else {
                        reqwest::redirect::Policy::limited(5)
                    })
                    .build()
                    .map_err(|e| UpgradeError::Refused(format!("http client: {e}")))?;
                let mut resp = client
                    .get(format!("{base}/{rel}"))
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                    .map_err(|e| UpgradeError::Refused(format!("fetching {rel}: {e}")))?;
                if resp.content_length().is_some_and(|n| n > max) {
                    return refuse(format!("{rel} is larger than {max} bytes"));
                }
                let mut out = Vec::new();
                while let Some(chunk) = resp
                    .chunk()
                    .await
                    .map_err(|e| UpgradeError::Refused(format!("fetching {rel}: {e}")))?
                {
                    if out.len() as u64 + chunk.len() as u64 > max {
                        return refuse(format!("{rel} is larger than {max} bytes"));
                    }
                    out.extend_from_slice(&chunk);
                }
                Ok(out)
            }
        }
    }

    async fn latest(&self) -> UResult<Version> {
        let text = self.fetch("latest/version", 64).await?;
        let text = String::from_utf8_lossy(&text);
        Version::parse(text.trim()).map_or_else(
            || refuse("the releases latest/version file is not a version"),
            Ok,
        )
    }
}

/// A file name that is one plain component.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// Unpacks a verified zip into the new directory `dest` (flat files only, bounded).
fn unpack(zip_bytes: &Verified, dest: &Path) -> UResult<()> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&zip_bytes.0))
        .map_err(|e| UpgradeError::Refused(format!("bad archive: {e}")))?;
    if zip.len() > MAX_ENTRIES {
        return refuse("the archive has too many entries");
    }
    std::fs::create_dir_all(dest)?;
    let (mut total, mut has_exe) = (0u64, false);
    for i in 0..zip.len() {
        let mut f = zip
            .by_index(i)
            .map_err(|e| UpgradeError::Refused(format!("bad archive: {e}")))?;
        let name = String::from_utf8_lossy(f.name_raw()).into_owned();
        if !f.is_file() || f.is_symlink() || !plain_name(&name) {
            return refuse(format!("the archive entry {name:?} is not a plain file"));
        }
        total = total.saturating_add(f.size());
        if total > MAX_UNPACKED {
            return refuse("the archive unpacks to too much data");
        }
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest.join(&name))?;
        // The declared size is not trusted: the copy itself is bounded.
        let size = f.size();
        let n = std::io::copy(&mut (&mut f).take(size + 1), &mut out)?;
        if n != size {
            return refuse(format!(
                "the archive entry {name:?} is not the size it declares"
            ));
        }
        drop(out);
        #[cfg(unix)]
        if name == EXE {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dest.join(&name), std::fs::Permissions::from_mode(0o755))?;
        }
        has_exe |= name == EXE;
    }
    if !has_exe {
        return refuse(format!("the archive has no {EXE}"));
    }
    Ok(())
}

/// Points `<dir>/git[.exe]` at `exe`, replacing a stale link atomically.
fn link_git(exe: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".git-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    #[cfg(windows)]
    std::fs::hard_link(exe, &tmp)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(exe, &tmp)?;
    std::fs::rename(&tmp, dir.join(GIT_FILE)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The version directory `current` points at.
fn current_version(prefix: &Path) -> Option<Version> {
    let target = std::fs::read_link(prefix.join("current")).ok()?;
    Version::parse(target.file_name()?.to_str()?)
}

/// Switches `<prefix>/current` to `<prefix>/<ver>`.
fn switch_current(prefix: &Path, ver: Version) -> UResult<()> {
    let cur = prefix.join("current");
    #[cfg(unix)]
    {
        let tmp = prefix.join(format!(".current-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(ver.to_string(), &tmp)?;
        std::fs::rename(&tmp, &cur).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let target = prefix.join(ver.to_string());
        let (c, t) = (cur.to_string_lossy(), target.to_string_lossy());
        if c.contains(['%', '"', '^', '&']) || t.contains(['%', '"', '^', '&']) {
            return refuse("the install prefix has characters that cannot be used for a junction");
        }
        // A junction is removed without touching its target.
        if let Ok(m) = std::fs::symlink_metadata(&cur) {
            if m.is_dir() || m.file_type().is_symlink() {
                std::fs::remove_dir(&cur)?;
            } else {
                return refuse(format!("{} exists and is not a junction", cur.display()));
            }
        }
        let status = std::process::Command::new("cmd")
            .raw_arg(format!("/D /C mklink /J \"{c}\" \"{t}\""))
            .stdout(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            return refuse("cannot create the `current` junction");
        }
        Ok(())
    }
}

/// Installs a verified archive for `ver` beside the others and switches `current` to it.
/// Relinks the git shim when the previously active version had it. Returns the version
/// directory.
pub fn install(prefix: &Path, ver: Version, archive: &Verified) -> UResult<PathBuf> {
    std::fs::create_dir_all(prefix)?;
    let had_shim = prefix.join("current").join("shim").join(GIT_FILE).exists();
    let dir = prefix.join(ver.to_string());
    if dir.exists() {
        return refuse(format!("{} already exists", dir.display()));
    }
    let stage = prefix.join(format!(".{ver}.new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    let built = unpack(archive, &stage).and_then(|()| {
        std::fs::create_dir_all(stage.join("shim"))?;
        Ok(())
    });
    if let Err(e) = built.and_then(|()| std::fs::rename(&stage, &dir).map_err(Into::into)) {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(e);
    }
    if had_shim {
        link_git(&dir.join(EXE), &dir.join("shim"))?;
    }
    switch_current(prefix, ver)?;
    Ok(dir)
}

/// Removes version directories that are neither the active one nor `keep` (the one just
/// replaced). Best effort: a directory in use is left for the next time.
pub fn prune(prefix: &Path, keep: Option<Version>) {
    let active = current_version(prefix);
    let listing = std::fs::read_dir(prefix); // lint:allow-worktree-fs: the install prefix is not a work tree
    let Ok(rd) = listing else {
        return;
    };
    for e in rd.flatten() {
        let Some(v) = e.file_name().to_str().and_then(Version::parse) else {
            continue;
        };
        if Some(v) != active && Some(v) != keep {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// The install prefix of the running executable: `<prefix>/<ver>/asc`.
fn running_prefix() -> UResult<(PathBuf, Version)> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe.parent().and_then(Path::parent).map(Path::to_owned);
    let ver = exe
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|n| n.to_str())
        .and_then(Version::parse);
    match (dir, ver) {
        (Some(p), Some(v)) => Ok((p, v)),
        _ => refuse(
            "this asc is not in a versioned install (<prefix>/<version>/asc); update it with the tool that installed it, or reinstall with install.sh, install.ps1 or the MSI",
        ),
    }
}

/// Whether the daily check is due, given the time of the last one.
pub fn check_due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|l| now.saturating_sub(l) >= CHECK_EVERY.as_secs())
}

fn stamp_path(home: &Path) -> PathBuf {
    home.join("update-check")
}

/// Records a check: `<unix secs> <latest version>`.
pub fn record_check(home: &Path, now: u64, latest: Version) -> std::io::Result<()> {
    std::fs::write(stamp_path(home), format!("{now} {latest}\n"))
}

/// The last check's time and result.
pub fn last_check(home: &Path) -> Option<(u64, Version)> {
    let mut f = std::fs::File::open(stamp_path(home)).ok()?.take(MAX_SMALL);
    let mut text = String::new();
    f.read_to_string(&mut text).ok()?;
    let (t, v) = text.trim().split_once(' ')?;
    Some((t.parse().ok()?, Version::parse(v)?))
}

/// The hint `asc status` prints when the last check found a newer version.
pub fn hint(home: &Path, current: &str) -> Option<String> {
    let (_, latest) = last_check(home)?;
    (latest > Version::parse(current)?)
        .then(|| format!("hint: asc {latest} is available (asc upgrade)"))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {e}"))
}

/// The environment variable the installers and `asc upgrade` share for the releases location.
pub const DOWNLOAD_BASE_ENV: &str = "ASC_DOWNLOAD_BASE_URL";

/// Picks the release source: `--from`, then `asc.releasesUrl`, then `ASC_DOWNLOAD_BASE_URL`.
fn pick_source(
    from: Option<&Path>,
    configured: Option<String>,
    env_url: Option<String>,
) -> UResult<Source> {
    let nonempty = |u: Option<String>| u.map(|u| u.trim().to_owned()).filter(|u| !u.is_empty());
    match (from, nonempty(configured), nonempty(env_url)) {
        (Some(d), _, _) => Ok(Source::Dir(d.to_owned())),
        (None, Some(u), _) | (None, None, Some(u)) => Source::url(&u),
        (None, None, None) => refuse(format!(
            "no release source: set asc.releasesUrl or {DOWNLOAD_BASE_ENV}, or pass --from <dir>"
        )),
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let source = pick_source(
        args.from.as_deref(),
        ctx.config.get("asc.releasesUrl"),
        std::env::var(DOWNLOAD_BASE_ENV).ok(),
    )
    .map_err(fatal)?;
    let current =
        Version::parse(env!("CARGO_PKG_VERSION")).ok_or_else(|| fatal("bad build version"))?;
    let latest = source.latest().await.map_err(fatal)?;
    if let Ok(home) = asc_state::AscHome::locate() {
        let _ = record_check(home.root(), now(), latest);
    }
    if latest <= current {
        println!("asc {current} is up to date");
        return Ok(ExitCode::SUCCESS);
    }
    if args.check {
        println!("hint: asc {latest} is available (asc upgrade)");
        return Ok(ExitCode::SUCCESS);
    }
    let Some(key) = PUBKEY else {
        return Err(fatal(
            "this build has no release key (ASC_RELEASE_PUBKEY); it cannot verify an upgrade",
        ));
    };
    let (prefix, running) = running_prefix().map_err(fatal)?;
    let name = artefact_name(latest);
    let bytes = source
        .fetch(&format!("{latest}/{name}"), MAX_ARTEFACT)
        .await
        .map_err(fatal)?;
    let sig = source
        .fetch(&format!("{latest}/{name}.minisig"), MAX_SMALL)
        .await
        .map_err(fatal)?;
    let sig = String::from_utf8(sig).map_err(|_| fatal("the signature file is not text"))?;
    let verified = verify(&bytes, &sig, key, &name, false).map_err(fatal)?;
    prune(&prefix, Some(running));
    install(&prefix, latest, &verified).map_err(fatal)?;
    println!(
        "asc {latest} installed; {} is the new current",
        prefix.display()
    );
    // The upgrade is done; a daemon that will not stop only delays the switch to the next start.
    if !crate::daemon::lifecycle::disabled(&|k: &str| std::env::var_os(k))
        && let Ok((cfg, proc)) = super::daemon::system()
        && let Err(e) = crate::daemon::lifecycle::stop(&cfg, &proc).await
    {
        eprintln!("warning: the old daemon was not stopped: {e}");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // minisign-verify's own vector: key, a legacy signature over b"test".
    const KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const SIG: &str = "untrusted comment: signature from minisign secret key\nRWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=\ntrusted comment: timestamp:1555779966\tfile:test\nQtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==";

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn zip_of(files: &[(&str, &[u8])]) -> Verified {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut buf);
        for (n, b) in files {
            w.start_file(*n, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(b).unwrap();
        }
        w.finish().unwrap();
        Verified(buf.into_inner())
    }

    #[test]
    fn versions_are_strict() {
        assert!(v("1.2.3") < v("1.10.0"));
        for bad in [
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.x",
            "../1.2",
            "1.2.3-rc1",
            "",
            "1..3",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn upgrade_accepts_a_good_signature() {
        verify(b"test", SIG, KEY, "test", true).unwrap();
    }

    #[test]
    fn upgrade_refuses_a_tampered_artefact() {
        let e = verify(b"Test", SIG, KEY, "test", true).err().unwrap();
        assert!(e.to_string().contains("signature check failed"), "{e}");
    }

    #[test]
    fn upgrade_refuses_a_signature_for_another_file() {
        let e = verify(b"test", SIG, KEY, "asc-9.9.9-x.zip", true)
            .err()
            .unwrap();
        assert!(e.to_string().contains("not for"), "{e}");
    }

    #[test]
    fn upgrade_refuses_legacy_signatures_in_production() {
        assert!(verify(b"test", SIG, KEY, "test", false).is_err());
    }

    #[test]
    fn upgrade_refuses_unsafe_archives() {
        let t = tempfile::tempdir().unwrap();
        for name in ["../asc", "a/asc", "sub/", "C:asc", "a\\asc"] {
            assert!(
                unpack(&zip_of(&[(name, b"x")]), &t.path().join("d")).is_err(),
                "{name}"
            );
            let _ = std::fs::remove_dir_all(t.path().join("d"));
        }
        assert!(
            unpack(&zip_of(&[("readme", b"x")]), &t.path().join("e")).is_err(),
            "no asc"
        );
    }

    #[test]
    fn side_by_side_relinks_helpers() {
        let t = tempfile::tempdir().unwrap();
        let prefix = t.path();
        let one = zip_of(&[(EXE, b"one"), ("asc-helper.dll", b"h1")]);
        let d1 = install(prefix, v("1.0.0"), &one).unwrap();
        assert!(d1.join("shim").is_dir(), "the shim dir always exists");
        assert!(
            !d1.join("shim").join(GIT_FILE).exists(),
            "the shim is optional"
        );
        // Opt in to the shim, as the MSI or `install-shim` does.
        link_git(&d1.join(EXE), &d1.join("shim")).unwrap();

        let two = zip_of(&[(EXE, b"two"), ("asc-helper.dll", b"h2")]);
        let d2 = install(prefix, v("1.1.0"), &two).unwrap();
        assert_eq!(
            std::fs::read(d2.join("shim").join(GIT_FILE)).unwrap(),
            b"two",
            "relinked to the new exe"
        );
        assert_eq!(
            std::fs::read(d1.join("shim").join(GIT_FILE)).unwrap(),
            b"one",
            "the old side is untouched"
        );
        assert_eq!(
            std::fs::read(prefix.join("current").join(EXE)).unwrap(),
            b"two"
        );
        assert_eq!(current_version(prefix), Some(v("1.1.0")));

        let three = zip_of(&[(EXE, b"three")]);
        install(prefix, v("1.2.0"), &three).unwrap();
        prune(prefix, Some(v("1.1.0")));
        assert!(!d1.exists(), "older versions are pruned");
        assert!(
            d2.exists(),
            "the replaced version stays until the next upgrade"
        );
        assert!(install(prefix, v("1.2.0"), &three).is_err(), "no overwrite");
    }

    #[test]
    fn the_daily_check_and_hint() {
        assert!(check_due(None, 5));
        assert!(!check_due(Some(100), 100 + 3600));
        assert!(check_due(Some(100), 100 + 86_400));
        let t = tempfile::tempdir().unwrap();
        assert_eq!(hint(t.path(), "1.0.0"), None);
        record_check(t.path(), 7, v("1.2.0")).unwrap();
        assert_eq!(
            hint(t.path(), "1.0.0").unwrap(),
            "hint: asc 1.2.0 is available (asc upgrade)"
        );
        assert_eq!(hint(t.path(), "1.2.0"), None);
    }

    #[test]
    fn min_client_message() {
        assert!(
            client_too_old("0.9.0", "1.0.0")
                .unwrap()
                .starts_with("CLIENT_TOO_OLD")
        );
        assert!(client_too_old("1.0.0", "1.0.0").is_none());
        assert!(client_too_old("junk", "1.0.0").is_none());
    }

    #[test]
    fn only_https_sources() {
        assert!(Source::url("http://example.com").is_err());
        assert!(Source::url("https://example.com/r/").is_ok());
        // Plain http, even to the local machine, needs the opt-in (U89).
        let local = "http://127.0.0.1:8080/r";
        assert!(Source::url_under(local, Policy::STRICT).is_err());
        assert!(Source::url_under(local, Policy::LOOPBACK_HTTP).is_ok());
        assert!(Source::url_under("http://example.com/r", Policy::LOOPBACK_HTTP).is_err());
        let why = Source::url_under("http://u:hunter2@example.com:81/r?t=abc", Policy::STRICT)
            .err()
            .unwrap()
            .to_string();
        assert!(why.contains("http://example.com:81"), "{why}");
        assert!(!why.contains("hunter2") && !why.contains("abc"), "{why}");
    }

    #[test]
    fn the_source_is_from_then_config_then_environment() {
        let dir = std::path::Path::new("/releases");
        let url = |s: &str| Some(s.to_owned());
        assert!(matches!(
            pick_source(
                Some(dir),
                url("https://a.example/r"),
                url("https://b.example/r")
            ),
            Ok(Source::Dir(_))
        ));
        match pick_source(
            None,
            url("https://a.example/r/"),
            url("https://b.example/r"),
        ) {
            Ok(Source::Url(u)) => assert_eq!(u, "https://a.example/r"),
            _ => panic!("the config value wins over the environment"),
        }
        match pick_source(None, url("  "), url("https://b.example/r")) {
            Ok(Source::Url(u)) => assert_eq!(u, "https://b.example/r"),
            _ => panic!("a blank config value falls through to the environment"),
        }
        let none = pick_source(None, None, None).err().unwrap().to_string();
        assert!(
            none.contains(DOWNLOAD_BASE_ENV) && none.contains("--from"),
            "{none}"
        );
        // The environment value is held to the same https rule as the config value.
        assert!(pick_source(None, None, url("http://example.com/r")).is_err());
    }

    #[test]
    fn a_binary_outside_a_versioned_install_says_what_to_do() {
        // The test executable lives in target/<profile>/deps, which is not a version directory.
        let why = running_prefix().err().unwrap().to_string();
        assert!(
            why.contains("tool that installed it") && why.contains("install.sh"),
            "{why}"
        );
    }

    #[test]
    fn only_the_executable_is_marked_executable() {
        let t = tempfile::tempdir().unwrap();
        let archive = zip_of(&[
            (EXE, b"bin"),
            ("LICENSE-MIT.txt", b"text"),
            ("LICENSE", b"text"),
        ]);
        let dest = t.path().join("d");
        unpack(&archive, &dest).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |n: &str| {
                std::fs::metadata(dest.join(n))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111
            };
            assert_ne!(mode(EXE), 0);
            assert_eq!(mode("LICENSE-MIT.txt"), 0);
            assert_eq!(mode("LICENSE"), 0, "a file without a dot is data too");
        }
    }

    #[test]
    fn packaging_wires_the_shim_before_git_cmd() {
        let wxs = include_str!("../../../../packaging/windows/main.wxs");
        let ps = include_str!("../../../../packaging/windows/register-task.ps1");
        // The optional shim feature runs the Shim action, which inserts before Git\cmd.
        assert!(wxs.contains("-Action Shim") && wxs.contains("Use asc for git commands"));
        assert!(
            wxs.contains("Name='shim'"),
            "the private shim dir is always created"
        );
        assert!(ps.contains(r"-like '*\Git\cmd'") && ps.contains("$before + @($ShimDir)"));
        assert!(
            ps.contains("'Machine'") || ps.contains("LocalMachine"),
            "machine PATH, not user PATH"
        );
    }
}
