//! `asc release`: releases of a repository, their notes, provenance and asset checksums (`Releases`).
//!
//! ```text
//! asc release list [--channel stable|pre-release|draft] [--limit N] [--page TOKEN]
//! asc release show <tag>
//! asc release next
//! asc release notes [--from REV] [--to REV]
//! asc release create <tag> [--title T] [--notes TEXT | --notes-file FILE | --generate-notes]
//!                    [--pre-release] [--draft] [--target REV] [--build-source S --build-run ID]
//!                    [--asset FILE[=URL]]...
//! asc release publish <tag>
//! asc release download <tag> [--asset NAME] [-o DIR]
//! ```
//!
//! The server keeps the record, writes the provenance and decides who may do what. `create` reads each
//! `--asset` file to give the server its size and SHA-256; the bytes are not sent (they are served from
//! the address after `=`, when there is one). `download` fetches an asset from its https address and
//! refuses a file whose size or SHA-256 differs from the record.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_rpc::server::wire;
use asc_rpc::services::releases_client::ReleasesClient;
use sha2::{Digest, Sha256};
use tonic::transport::Channel;

use super::agent::conn::{Conn, MAX_MESSAGE};
use super::codeintel::{clean_str, emit_json, lines, with_stdout};
use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;
use crate::remote::server::Session;

pub(crate) fn client(s: &Session) -> ReleasesClient<Channel> {
    ReleasesClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

/// A usage failure with the text of the command.
fn bad(text: impl Into<String>) -> CliError {
    CliError::Usage(format!("asc release: {}", text.into()))
}

fn fatal(text: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {text}"))
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "release",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Sub,
}

#[derive(clap::Subcommand, Debug)]
pub enum Sub {
    /// Lists releases, newest first.
    List(ListArgs),
    /// Shows one release with its provenance and assets.
    Show(TagArgs),
    /// Shows the changes since the latest release and proposes the next tag.
    Next(PlainArgs),
    /// Writes release notes from the changes landed since a revision.
    Notes(NotesArgs),
    /// Records a release.
    Create(CreateArgs),
    /// Publishes a draft.
    Publish(TagArgs),
    /// Downloads an asset and checks its size and SHA-256.
    Download(DownloadArgs),
}

#[derive(clap::Args, Debug)]
pub struct PlainArgs {
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct TagArgs {
    tag: String,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// stable, pre-release or draft; all the caller may see when absent.
    #[arg(long)]
    channel: Option<String>,
    #[arg(long)]
    limit: Option<u32>,
    #[arg(long)]
    page: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct NotesArgs {
    /// A tag, branch or changeset; the newest published release when absent.
    #[arg(long)]
    from: Option<String>,
    /// The default branch when absent.
    #[arg(long)]
    to: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct CreateArgs {
    tag: String,
    #[arg(long)]
    title: Option<String>,
    /// The notes text.
    #[arg(long, conflicts_with_all = ["notes_file", "generate_notes"])]
    notes: Option<String>,
    /// Read the notes from a file (`-` is standard input).
    #[arg(long = "notes-file", conflicts_with = "generate_notes")]
    notes_file: Option<PathBuf>,
    /// Write the notes from the changes since the previous release.
    #[arg(long = "generate-notes")]
    generate_notes: bool,
    #[arg(long = "pre-release")]
    pre_release: bool,
    #[arg(long)]
    draft: bool,
    /// The revision to cut the release from; the default branch when absent.
    #[arg(long)]
    target: Option<String>,
    /// The CI source of the run that built the release (with --build-run).
    #[arg(long = "build-source")]
    build_source: Option<String>,
    #[arg(long = "build-run")]
    build_run: Option<String>,
    /// A file to record, as FILE or FILE=https://address; repeat.
    #[arg(long = "asset")]
    assets: Vec<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct DownloadArgs {
    tag: String,
    /// One asset by name; every asset when absent.
    #[arg(long)]
    asset: Option<String>,
    /// The directory to write into; the current one when absent.
    #[arg(short = 'o', long = "output")]
    output: Option<PathBuf>,
    #[arg(long)]
    remote: Option<String>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "release",
    flags: &[
        "--remote",
        "--json",
        "--channel",
        "--limit",
        "--page",
        "--from",
        "--to",
        "--title",
        "--notes",
        "--notes-file",
        "--generate-notes",
        "--pre-release",
        "--draft",
        "--target",
        "--build-source",
        "--build-run",
        "--asset",
        "--output",
        "-o",
    ],
    placeholders: &[],
}];

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    match a.cmd {
        Sub::List(x) => list(ctx, x).await,
        Sub::Show(x) => show(ctx, x).await,
        Sub::Next(x) => next(ctx, x).await,
        Sub::Notes(x) => notes(ctx, x).await,
        Sub::Create(x) => create(ctx, x).await,
        Sub::Publish(x) => publish(ctx, x).await,
        Sub::Download(x) => download(ctx, x).await,
    }
}

// ------------------------------------------------------------------------------ pure helpers

pub(crate) fn channel_of(s: &str) -> Result<i32> {
    Ok(match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "stable" => wire::ReleaseChannel::Stable,
        "pre-release" | "prerelease" | "pre" => wire::ReleaseChannel::PreRelease,
        "draft" => wire::ReleaseChannel::Draft,
        _ => return Err(bad("--channel is stable, pre-release or draft")),
    } as i32)
}

pub(crate) fn tag_ok(tag: &str) -> Result<()> {
    let ok = !tag.is_empty()
        && tag.len() <= 128
        && !tag.starts_with(['-', '/', '.'])
        && !tag.ends_with(['/', '.'])
        && !tag.contains("//")
        && !tag.contains("..")
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b'/'));
    if ok {
        Ok(())
    } else {
        Err(bad(format!(
            "{tag:?} is not a tag: 1 to 128 characters of letters, digits and ._+-/"
        )))
    }
}

/// The SHA-256 of `bytes` as lowercase hex.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize()
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// An asset name that is safe to write: a plain file name.
pub(crate) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':'])
        && !name.chars().any(char::is_control)
}

/// `FILE` or `FILE=https://address`.
pub(crate) fn split_asset(arg: &str) -> Result<(&str, &str)> {
    let (file, url) = match arg.find("=https://") {
        Some(i) => (&arg[..i], &arg[i + 1..]),
        None => (arg, ""),
    };
    if file.is_empty() {
        return Err(bad("--asset is FILE or FILE=https://address"));
    }
    Ok((file, url))
}

fn asset_of(arg: &str) -> Result<wire::ReleaseAsset> {
    let (file, url) = split_asset(arg)?;
    let path = Path::new(file);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| safe_name(n))
        .ok_or_else(|| bad(format!("{file:?} has no usable file name")))?
        .to_owned();
    let bytes = std::fs::read(path).map_err(|e| fatal(format!("cannot read {file}: {e}")))?;
    Ok(wire::ReleaseAsset {
        name,
        size: bytes.len() as u64,
        sha256: sha256_hex(&bytes),
        url: url.to_owned(),
        content_type: String::new(),
    })
}

fn channel_name(n: i32) -> &'static str {
    match wire::ReleaseChannel::try_from(n) {
        Ok(wire::ReleaseChannel::PreRelease) => "pre-release",
        Ok(wire::ReleaseChannel::Draft) => "draft",
        _ => "stable",
    }
}

fn release_lines(out: &mut dyn std::io::Write, r: &wire::Release) -> std::io::Result<()> {
    writeln!(
        out,
        "{}\t{}\t{}\t{}",
        clean_str(&r.tag),
        if r.draft {
            "draft"
        } else {
            channel_name(r.channel)
        },
        r.changes,
        clean_str(&r.title)
    )
}

fn release_detail(out: &mut dyn std::io::Write, r: &wire::Release) -> std::io::Result<()> {
    let p = r.provenance.clone().unwrap_or_default();
    writeln!(
        out,
        "{} ({})",
        clean_str(&r.tag),
        if r.draft {
            "draft"
        } else {
            channel_name(r.channel)
        }
    )?;
    writeln!(out, "title: {}", clean_str(&r.title))?;
    writeln!(out, "by: {} at {}", clean_str(&r.created_by), r.created)?;
    writeln!(
        out,
        "from: {} ({} changes since {})",
        clean_str(&p.built_from),
        r.changes,
        if r.previous_tag.is_empty() {
            "the start"
        } else {
            &r.previous_tag
        }
    )?;
    if !p.build_run.is_empty() {
        writeln!(
            out,
            "built by: {} run {} ({} of {} checks passed) {}",
            clean_str(&p.build_source),
            clean_str(&p.build_run),
            p.checks_passed,
            p.checks_total,
            clean_str(&p.build_url)
        )?;
    }
    if p.signed_by.is_empty() {
        writeln!(out, "signed: no")?;
    } else {
        writeln!(
            out,
            "signed: {} {} ({})",
            clean_str(&p.signed_by),
            clean_str(&p.signing_key),
            if p.signature_valid {
                "valid"
            } else {
                "not valid"
            }
        )?;
    }
    for a in &r.assets {
        writeln!(
            out,
            "asset: {}\t{} bytes\tsha256:{}\t{}",
            clean_str(&a.name),
            a.size,
            a.sha256,
            clean_str(&a.url)
        )?;
    }
    if !r.notes.is_empty() {
        writeln!(out)?;
        writeln!(out, "{}", clean_str(&r.notes))?;
    }
    Ok(())
}

// ------------------------------------------------------------------------------ commands

async fn list(ctx: &Ctx, a: ListArgs) -> Result<ExitCode> {
    let channel = a
        .channel
        .as_deref()
        .map(channel_of)
        .transpose()?
        .unwrap_or(0);
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::ListReleasesRequest {
                repo: Some(repo.clone()),
                channel,
                limit: a.limit.unwrap_or(0),
                page_token: a.page.clone().unwrap_or_default(),
            };
            async move { Ok(client(&s).list(s.wire.request(m, None)).await?.into_inner()) }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ListReleasesResponse", &r)?;
        } else {
            if r.releases.is_empty() {
                writeln!(
                    out,
                    "no releases yet; create one with `asc release create v1.0.0`"
                )?;
            }
            for x in &r.releases {
                release_lines(out, x)?;
            }
            if !r.next_page_token.is_empty() {
                writeln!(out, "# more: --page {}", r.next_page_token)?;
            }
        }
        Ok(ExitCode::SUCCESS)
    })
}

async fn show(ctx: &Ctx, a: TagArgs) -> Result<ExitCode> {
    tag_ok(&a.tag)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::GetReleaseRequest {
                repo: Some(repo.clone()),
                tag: a.tag.clone(),
            };
            async move { Ok(client(&s).get(s.wire.request(m, None)).await?.into_inner()) }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.GetReleaseResponse", &r)?;
        } else if let Some(x) = &r.release {
            release_detail(out, x)?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

async fn next(ctx: &Ctx, a: PlainArgs) -> Result<ExitCode> {
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::NextReleaseRequest {
                repo: Some(repo.clone()),
            };
            async move { Ok(client(&s).next(s.wire.request(m, None)).await?.into_inner()) }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.NextReleaseResponse", &r)?;
        } else {
            writeln!(
                out,
                "{} changes since {}",
                r.changes_since,
                if r.latest_tag.is_empty() {
                    "the start"
                } else {
                    &r.latest_tag
                }
            )?;
            if !r.proposed_tag.is_empty() {
                writeln!(out, "next: {}", clean_str(&r.proposed_tag))?;
            }
            if !r.draft_tag.is_empty() {
                writeln!(out, "draft: {}", clean_str(&r.draft_tag))?;
            }
        }
        Ok(ExitCode::SUCCESS)
    })
}

async fn draft_notes(
    conn: &Conn,
    from: Option<String>,
    to: Option<String>,
) -> Result<wire::DraftReleaseNotesResponse> {
    let repo = conn.repo_ref();
    conn.call(false, |s, _| {
        let m = wire::DraftReleaseNotesRequest {
            repo: Some(repo.clone()),
            from: from.clone().unwrap_or_default(),
            to: to.clone().unwrap_or_default(),
        };
        async move {
            Ok(client(&s)
                .draft_notes(s.wire.request(m, None))
                .await?
                .into_inner())
        }
    })
    .await
}

async fn notes(ctx: &Ctx, a: NotesArgs) -> Result<ExitCode> {
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let r = draft_notes(&conn, a.from.clone(), a.to.clone()).await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.DraftReleaseNotesResponse", &r)?;
        } else {
            write!(out, "{}", clean_str(&r.markdown))?;
            if r.truncated {
                writeln!(out, "# more than 500 changes: the oldest are left out")?;
            }
        }
        Ok(ExitCode::SUCCESS)
    })
}

async fn create(ctx: &Ctx, a: CreateArgs) -> Result<ExitCode> {
    tag_ok(&a.tag)?;
    if a.build_source.is_some() != a.build_run.is_some() {
        return Err(bad("--build-source and --build-run go together"));
    }
    let mut assets = Vec::new();
    for x in &a.assets {
        assets.push(asset_of(x)?);
    }
    let mut notes = match (&a.notes, &a.notes_file) {
        (Some(n), _) => n.clone(),
        (None, Some(p)) if p.as_os_str() == "-" => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
                .map_err(|e| fatal(format!("cannot read standard input: {e}")))?;
            s
        }
        (None, Some(p)) => std::fs::read_to_string(p)
            .map_err(|e| fatal(format!("cannot read {}: {e}", p.display())))?,
        (None, None) => String::new(),
    };
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    if a.generate_notes {
        notes = draft_notes(&conn, None, a.target.clone()).await?.markdown;
    }
    let repo = conn.repo_ref();
    let r = conn
        .call(true, |s, op| {
            let m = wire::CreateReleaseRequest {
                repo: Some(repo.clone()),
                tag: a.tag.clone(),
                title: a.title.clone().unwrap_or_default(),
                notes: notes.clone(),
                channel: if a.pre_release {
                    wire::ReleaseChannel::PreRelease
                } else {
                    wire::ReleaseChannel::Stable
                } as i32,
                target: a.target.clone().unwrap_or_default(),
                build_source: a.build_source.clone().unwrap_or_default(),
                build_run: a.build_run.clone().unwrap_or_default(),
                assets: assets.clone(),
                draft: a.draft,
            };
            async move { Ok(client(&s).create(s.wire.request(m, op)).await?.into_inner()) }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.CreateReleaseResponse", &r)?;
        } else if let Some(x) = &r.release {
            release_lines(out, x)?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

async fn publish(ctx: &Ctx, a: TagArgs) -> Result<ExitCode> {
    tag_ok(&a.tag)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(true, |s, op| {
            let m = wire::PublishReleaseRequest {
                repo: Some(repo.clone()),
                tag: a.tag.clone(),
            };
            async move {
                Ok(client(&s)
                    .publish(s.wire.request(m, op))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.PublishReleaseResponse", &r)?;
        } else if let Some(x) = &r.release {
            release_lines(out, x)?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

// ------------------------------------------------------------------------------ download

/// Fetches `url` into `dest`, refusing more than `size` bytes or a different SHA-256, and moves the
/// file into place only when it is whole and right.
pub(crate) async fn fetch_verified(
    http: &reqwest::Client,
    url: &str,
    size: u64,
    sha256: &str,
    dest: &Path,
) -> Result<()> {
    let mut resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| fatal(format!("cannot fetch {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(fatal(format!("{url} answered {}", resp.status())));
    }
    let part = dest.with_extension("part");
    let mut file = std::fs::File::create(&part)
        .map_err(|e| fatal(format!("cannot write {}: {e}", part.display())))?;
    let mut hash = Sha256::new();
    let mut got = 0u64;
    let result = async {
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| fatal(format!("cannot read {url}: {e}")))?
        {
            got += chunk.len() as u64;
            if got > size {
                return Err(fatal(format!(
                    "{url} is larger than the {size} bytes recorded"
                )));
            }
            hash.update(&chunk);
            file.write_all(&chunk)
                .map_err(|e| fatal(format!("cannot write {}: {e}", part.display())))?;
        }
        if got != size {
            return Err(fatal(format!(
                "{url} gave {got} bytes, the record says {size}"
            )));
        }
        let hex = hash
            .finalize()
            .iter()
            .fold(String::with_capacity(64), |mut s, b| {
                use std::fmt::Write as _;
                let _ = write!(s, "{b:02x}");
                s
            });
        if hex != sha256 {
            return Err(fatal(format!(
                "{url} has SHA-256 {hex}, the record says {sha256}"
            )));
        }
        Ok(())
    }
    .await;
    drop(file);
    match result {
        Ok(()) => std::fs::rename(&part, dest)
            .map_err(|e| fatal(format!("cannot write {}: {e}", dest.display()))),
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

fn http_client() -> Result<reqwest::Client> {
    // Redirects stay on https, and there are at most five.
    let policy = reqwest::redirect::Policy::custom(|a| {
        if a.previous().len() >= 5 || a.url().scheme() != "https" {
            a.stop()
        } else {
            a.follow()
        }
    });
    reqwest::Client::builder()
        .redirect(policy)
        .build()
        .map_err(|e| fatal(format!("cannot start the HTTP client: {e}")))
}

async fn download(ctx: &Ctx, a: DownloadArgs) -> Result<ExitCode> {
    tag_ok(&a.tag)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::GetReleaseRequest {
                repo: Some(repo.clone()),
                tag: a.tag.clone(),
            };
            async move { Ok(client(&s).get(s.wire.request(m, None)).await?.into_inner()) }
        })
        .await?;
    let release = r.release.unwrap_or_default();
    let chosen: Vec<&wire::ReleaseAsset> = release
        .assets
        .iter()
        .filter(|x| a.asset.as_deref().is_none_or(|n| n == x.name))
        .collect();
    if chosen.is_empty() {
        return Err(bad(match &a.asset {
            Some(n) => format!("release {} has no asset {n:?}", a.tag),
            None => format!("release {} has no assets", a.tag),
        }));
    }
    let dir = a.output.clone().unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)
        .map_err(|e| fatal(format!("cannot create {}: {e}", dir.display())))?;
    let http = http_client()?;
    for x in chosen {
        if !safe_name(&x.name) {
            return Err(fatal(format!(
                "the asset name {:?} is not a file name",
                x.name
            )));
        }
        if !x.url.starts_with("https://") {
            return Err(fatal(format!(
                "asset {} has no https address",
                clean_str(&x.name)
            )));
        }
        let dest = dir.join(&x.name);
        fetch_verified(&http, &x.url, x.size, &x.sha256, &dest).await?;
        println!("{}\t{} bytes\tsha256:{}", dest.display(), x.size, x.sha256);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_tags_and_assets_are_checked_before_a_round_trip() {
        assert_eq!(
            channel_of("Pre_release").unwrap(),
            wire::ReleaseChannel::PreRelease as i32
        );
        assert_eq!(
            channel_of("draft").unwrap(),
            wire::ReleaseChannel::Draft as i32
        );
        assert!(channel_of("nightly").is_err());
        assert!(tag_ok("v1.2.3-rc.1").is_ok());
        for bad in ["", "a b", "-x", "a..b", "x/"] {
            assert!(tag_ok(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        for ok in ["asc.tar.gz", "a b.zip"] {
            assert!(safe_name(ok));
        }
        for no in ["", "..", "a/b", "a\\b", "c:x", "a\nb"] {
            assert!(!safe_name(no), "{no:?}");
        }
        assert_eq!(split_asset("dist/a.tar.gz").unwrap(), ("dist/a.tar.gz", ""));
        assert_eq!(
            split_asset("dist/a.tar.gz=https://dl.example/a.tar.gz").unwrap(),
            ("dist/a.tar.gz", "https://dl.example/a.tar.gz")
        );
        assert!(split_asset("=https://x").is_err());
    }

    #[test]
    fn an_asset_argument_reads_the_file_for_its_size_and_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("asc.tar.gz");
        std::fs::write(&f, b"abc").unwrap();
        let a = asset_of(&format!("{}=https://dl.example/asc.tar.gz", f.display())).unwrap();
        assert_eq!((a.name.as_str(), a.size), ("asc.tar.gz", 3));
        assert_eq!(a.sha256, sha256_hex(b"abc"));
        assert_eq!(a.url, "https://dl.example/asc.tar.gz");
        assert!(asset_of(&dir.path().join("missing").display().to_string()).is_err());
    }

    /// Serves `body` once over loopback HTTP.
    async fn serve_once(body: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&body).await;
                let _ = s.shutdown().await;
            }
        });
        format!("http://{addr}/asset")
    }

    #[tokio::test]
    async fn a_download_is_kept_only_when_its_size_and_checksum_match_the_record() {
        let http = reqwest::Client::new();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.bin");
        let url = serve_once(b"abc".to_vec()).await;
        fetch_verified(&http, &url, 3, &sha256_hex(b"abc"), &dest)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
        // A different checksum, a different size and a longer body keep nothing.
        for (body, size, sum) in [
            (b"abc".to_vec(), 3u64, sha256_hex(b"abd")),
            (b"abc".to_vec(), 4, sha256_hex(b"abc")),
            (b"abcd".to_vec(), 3, sha256_hex(b"abc")),
        ] {
            let other = dir.path().join("b.bin");
            let url = serve_once(body).await;
            let err = fetch_verified(&http, &url, size, &sum, &other)
                .await
                .unwrap_err();
            assert!(matches!(err, CliError::Fatal(_)), "{err:?}");
            assert!(!other.exists() && !other.with_extension("part").exists());
        }
    }
}
