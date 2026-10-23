//! `asc ci artifact upload|download|list` (SPEC §12.7, PLAN U75). The store and its HTTP client
//! are shared with `ci cache` (`cache.rs`).

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use asc_ci::artifact::{self, Uploaded};
use clap::{Parser, Subcommand};

use super::cache::{connect, emit, fatal, root_of};
use crate::cli::common::{CliError, Ctx, Result};

/// Every flag of `ci artifact`, for the native table.
pub const FLAGS: &[&str] = &["--root", "--remote", "--overwrite", "--path"];

#[derive(Parser, Debug)]
#[command(
    name = "ci artifact",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Uploads the files named by `paths` as artifact `name`.
    Upload {
        name: String,
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long, env = "ASC_REMOTE")]
        remote: Option<String>,
        /// Replace an artifact of the same name.
        #[arg(long)]
        overwrite: bool,
        paths: Vec<String>,
    },
    /// Downloads artifact `name` into `--path` (default: the current directory).
    Download {
        name: String,
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long, env = "ASC_REMOTE")]
        remote: Option<String>,
    },
    /// Lists artifact names (newest first), optionally those starting with `prefix`.
    List {
        prefix: Option<String>,
        #[arg(long, env = "ASC_REMOTE")]
        remote: Option<String>,
    },
}

pub async fn run(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let a = Args::try_parse_from(
        std::iter::once(OsString::from("ci artifact")).chain(rest.iter().cloned()),
    )
    .map_err(|e| CliError::Usage(e.to_string()))?;
    match a.cmd {
        Cmd::Upload {
            name,
            root,
            remote,
            overwrite,
            paths,
        } => {
            let root = root_of(ctx, root.as_deref())?;
            let c = connect(ctx, remote.as_deref()).await?;
            let out = artifact::upload(&c.api, &*c.objects, &root, &name, &paths, overwrite)
                .await
                .map_err(fatal)?;
            match out {
                Uploaded::Exists => {
                    return Err(CliError::Fatal(format!(
                        "asc: artifact '{name}' already exists; pass --overwrite to replace it"
                    )));
                }
                Uploaded::Empty => eprintln!("no file matched; nothing uploaded"),
                Uploaded::Uploaded { entries, stats } => {
                    eprintln!(
                        "uploaded {entries} files ({} bytes, {} new objects) as {name}",
                        stats.bytes, stats.new_objects
                    );
                    emit(&[("artifact-name", name)]);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Download { name, path, remote } => {
            let dest = root_of(ctx, path.as_deref())?;
            let c = connect(ctx, remote.as_deref()).await?;
            match artifact::download(&c.api, &*c.objects, &dest, &name)
                .await
                .map_err(fatal)?
            {
                Some(s) => {
                    eprintln!("downloaded {} files ({} bytes)", s.files, s.bytes);
                    emit(&[("download-path", dest.display().to_string())]);
                    Ok(ExitCode::SUCCESS)
                }
                None => Err(CliError::Fatal(format!("asc: no artifact named '{name}'"))),
            }
        }
        Cmd::List { prefix, remote } => {
            let c = connect(ctx, remote.as_deref()).await?;
            let rows = artifact::list(&c.api, prefix.as_deref().unwrap_or(""))
                .await
                .map_err(fatal)?;
            let mut out = String::new();
            for r in rows {
                // One name per line; a name with control characters cannot exist.
                out.push_str(&format!("{}\t{}\t{}\n", r.key, r.size, r.at));
            }
            let _ = std::io::stdout().write_all(out.as_bytes());
            Ok(ExitCode::SUCCESS)
        }
    }
}
