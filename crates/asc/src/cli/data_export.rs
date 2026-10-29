//! `asc data-export request | list | download <id> -o FILE | delete <id>`: your data as a JSON Lines
//! archive (`DataExport`). The archive is written to a new file only you can read.

use std::process::ExitCode;

use asc_rpc::server::wire;
use tonic::transport::Channel;

use super::acct::{bad, json, size};
use super::agent::conn::{Conn, MAX_MESSAGE, when};
use super::codeintel::{clean_str, with_stdout};
use super::common::{Ctx, Result};
use crate::native::NativeSpec;
use asc_rpc::services::data_export_client::DataExportClient;
use futures::StreamExt as _;

fn client(s: &crate::remote::server::Session) -> DataExportClient<Channel> {
    DataExportClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "data-export",
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
    Request(Common),
    List(Common),
    Download(DownloadArgs),
    Delete(IdArgs),
}

#[derive(clap::Args, Debug)]
pub struct Common {
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct IdArgs {
    id: String,
    #[command(flatten)]
    common: Common,
}

#[derive(clap::Args, Debug)]
pub struct DownloadArgs {
    id: String,
    /// The file to create; it must not exist.
    #[arg(short = 'o', long = "output")]
    output: String,
    #[command(flatten)]
    common: Common,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "data-export",
    flags: &["--remote", "--json", "-o", "--output"],
    placeholders: &[],
}];

pub(crate) fn id_ok(id: &str) -> Result<()> {
    if id.starts_with("ex_")
        && id.len() <= 40
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        Ok(())
    } else {
        Err(bad(
            "data-export",
            "an export id looks like ex_ followed by letters and digits",
        ))
    }
}

fn job_line(j: &wire::ExportJob) -> String {
    let state = super::agent::conn::short(
        wire::ExportState::try_from(j.state).map_or("?", |s| s.as_str_name()),
        "EXPORT_STATE_",
    );
    format!(
        "{}  {:<8} requested {}  {}  expires {}  {}",
        clean_str(&j.id),
        state,
        when(j.requested),
        size(j.size_bytes),
        when(j.expires),
        clean_str(&j.error)
    )
}

/// A new file, mode 0600, never over an existing one.
fn create_new(path: &str) -> Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
        .map_err(|e| bad("data-export", format!("{path}: {e}")))
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    match a.cmd {
        Sub::Request(c) => {
            let conn = Conn::open(ctx, c.remote.as_deref()).await?;
            let r = conn
                .call(true, |s, op| async move {
                    Ok(client(&s)
                        .request_export(s.wire.request(wire::RequestExportRequest {}, op))
                        .await?
                        .into_inner())
                })
                .await?;
            let j = r.job.unwrap_or_default();
            if c.json {
                return json("asc.v1.ExportJob", &j);
            }
            println!("{}", job_line(&j));
            Ok(ExitCode::SUCCESS)
        }
        Sub::List(c) => {
            let conn = Conn::open(ctx, c.remote.as_deref()).await?;
            let r = conn
                .call(false, |s, _| async move {
                    Ok(client(&s)
                        .list_exports(s.wire.request(wire::ListExportsRequest {}, None))
                        .await?
                        .into_inner())
                })
                .await?;
            if c.json {
                return json("asc.v1.ListExportsResponse", &r);
            }
            with_stdout(|out| {
                for j in &r.jobs {
                    writeln!(out, "{}", job_line(j))?;
                }
                Ok(ExitCode::SUCCESS)
            })
        }
        Sub::Delete(x) => {
            id_ok(&x.id)?;
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let msg = wire::DeleteExportRequest { id: x.id.clone() };
            conn.call(true, |s, op| {
                let m = msg.clone();
                async move {
                    Ok(client(&s)
                        .delete_export(s.wire.request(m, op))
                        .await?
                        .into_inner())
                }
            })
            .await?;
            eprintln!("asc: {} deleted", clean_str(&x.id));
            Ok(ExitCode::SUCCESS)
        }
        Sub::Download(x) => {
            id_ok(&x.id)?;
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let msg = wire::DownloadExportRequest { id: x.id.clone() };
            let mut file = create_new(&x.output)?;
            let res: Result<u64> = conn
                .call(false, |s, _| {
                    let m = msg.clone();
                    let file = file.try_clone();
                    async move {
                        let mut file = file.map_err(|e| tonic::Status::internal(e.to_string()))?;
                        let mut stream = client(&s)
                            .download(s.wire.request(m, None))
                            .await?
                            .into_inner();
                        let mut total = 0u64;
                        while let Some(chunk) = stream.next().await {
                            let c = chunk?;
                            if c.offset != total {
                                return Err(tonic::Status::data_loss(
                                    "the archive arrived out of order",
                                ));
                            }
                            std::io::Write::write_all(&mut file, &c.data)
                                .map_err(|e| tonic::Status::internal(e.to_string()))?;
                            total += c.data.len() as u64;
                            if c.last {
                                break;
                            }
                        }
                        Ok(total)
                    }
                })
                .await;
            match res {
                Ok(n) => {
                    std::io::Write::flush(&mut file).ok();
                    eprintln!("asc: {} bytes written to {}", n, clean_str(&x.output));
                    Ok(ExitCode::SUCCESS)
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&x.output);
                    Err(e)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_the_output_file_are_checked() {
        assert!(id_ok("ex_abc123").is_ok());
        for bad in ["", "ex_", "abc", "ex_../x", "ex_a b"] {
            assert!(id_ok(bad).is_err() || bad == "ex_", "{bad:?}");
        }
        let dir = std::env::temp_dir().join(format!("asc-dx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("out.jsonl");
        let p = p.to_str().unwrap();
        create_new(p).unwrap();
        assert!(
            create_new(p).is_err(),
            "an existing file is never overwritten"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
