//! `asc workflow secret list | set | rm`: the repository secrets a command step may read.
//!
//! A value is read from a file, an environment variable or standard input and sent once. The
//! server seals it; a list shows names and, when the value is long enough that they reveal
//! nothing, its last four characters.

use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{bad, client};
use crate::cli::agent::conn::{Conn, when};
use crate::cli::agent::read_secret;
use crate::cli::codeintel::{clean_str, emit_json, lines, with_stdout};
use crate::cli::common::{Ctx, Result};

#[derive(clap::Args, Debug)]
pub struct SecretArgs {
    #[command(subcommand)]
    cmd: SecretSub,
}

#[derive(clap::Subcommand, Debug)]
enum SecretSub {
    /// Lists the secret names of the repository.
    List(ListArgs),
    /// Stores a secret; the value comes from a file, a variable or standard input.
    Set(SetArgs),
    /// Removes a secret.
    Rm(RmArgs),
}

#[derive(clap::Args, Debug)]
struct ListArgs {
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct SetArgs {
    /// The name a workflow refers to: 1 to 64 of A-Z, a-z, 0-9, `.`, `_` and `-`.
    name: String,
    #[arg(long = "key-file")]
    key_file: Option<String>,
    #[arg(long = "key-env")]
    key_env: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct RmArgs {
    name: String,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

/// Longest value, in bytes.
const MAX_VALUE: usize = 16_384;

fn name_ok(s: &str) -> Result<()> {
    if s.is_empty()
        || s.len() > 64
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(bad(format!(
            "a secret name is 1 to 64 of A-Z, a-z, 0-9, '.', '_' and '-', not {s:?}"
        )));
    }
    Ok(())
}

fn secret_line(s: &wire::WorkflowSecret) -> String {
    format!(
        "{}\t{}\t{}\t{}",
        clean_str(&s.name),
        if s.hint.is_empty() {
            "-".to_owned()
        } else {
            format!("...{}", clean_str(&s.hint))
        },
        when(s.updated),
        clean_str(&s.updated_by)
    )
}

pub async fn run(ctx: &Ctx, a: SecretArgs) -> Result<ExitCode> {
    match a.cmd {
        SecretSub::List(x) => list(ctx, x).await,
        SecretSub::Set(x) => set(ctx, x).await,
        SecretSub::Rm(x) => rm(ctx, x).await,
    }
}

async fn list(ctx: &Ctx, a: ListArgs) -> Result<ExitCode> {
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::ListWorkflowSecretsRequest {
                repo: Some(repo.clone()),
            };
            async move {
                Ok(client(&s)
                    .list_secrets(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ListWorkflowSecretsResponse", &r)?;
        } else {
            for s in &r.secrets {
                writeln!(out, "{}", secret_line(s))?;
            }
        }
        Ok(if r.secrets.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    })
}

async fn set(ctx: &Ctx, a: SetArgs) -> Result<ExitCode> {
    name_ok(&a.name)?;
    let value = read_secret(a.key_file.as_deref(), a.key_env.as_deref(), "secret value")?;
    if value.len() > MAX_VALUE {
        return Err(bad(format!("a secret value is at most {MAX_VALUE} bytes")));
    }
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::SetWorkflowSecretRequest {
        repo: Some(conn.repo_ref()),
        name: a.name.clone(),
        value,
    };
    let s = conn
        .call(true, |s, op| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .set_secret(s.wire.request(m, op))
                    .await?
                    .into_inner()
                    .secret
                    .unwrap_or_default())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.WorkflowSecret", &s)?;
        } else {
            writeln!(out, "{}", secret_line(&s))?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

async fn rm(ctx: &Ctx, a: RmArgs) -> Result<ExitCode> {
    name_ok(&a.name)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::DeleteWorkflowSecretRequest {
        repo: Some(conn.repo_ref()),
        name: a.name.clone(),
    };
    conn.call(false, |s, _| {
        let m = msg.clone();
        async move {
            client(&s)
                .delete_secret(s.wire.request(m, None))
                .await
                .map(|_| ())
        }
    })
    .await?;
    if a.json {
        println!("{{}}");
    } else {
        eprintln!("asc: secret {} removed", clean_str(&a.name));
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn a_secret_value_is_never_an_argument() {
        let ok = super::super::Args::try_parse_from([
            "workflow",
            "secret",
            "set",
            "DEPLOY_TOKEN",
            "--key-env",
            "TOKEN",
        ])
        .unwrap();
        let super::super::Sub::Secret(SecretArgs {
            cmd: SecretSub::Set(s),
        }) = ok.cmd
        else {
            panic!("set")
        };
        assert_eq!(s.key_env.as_deref(), Some("TOKEN"));
        for args in [
            &["workflow", "secret", "set", "X", "--value", "hunter2"][..],
            &["workflow", "secret", "set", "X", "hunter2"],
        ] {
            assert!(
                super::super::Args::try_parse_from(args).is_err(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn names_are_checked_and_a_listing_is_clean_text() {
        assert!(name_ok("DEPLOY_TOKEN.v2-x").is_ok());
        for bad in ["", "a b", "a/b", "$X", &"a".repeat(65)] {
            assert!(name_ok(bad).is_err(), "{bad:?}");
        }
        let line = secret_line(&wire::WorkflowSecret {
            name: "T\u{1b}[2J".into(),
            hint: "wxyz".into(),
            updated_by: "alice".into(),
            ..Default::default()
        });
        assert!(
            !line.contains('\u{1b}') && line.contains("...wxyz"),
            "{line:?}"
        );
        let none = secret_line(&wire::WorkflowSecret {
            name: "SHORT".into(),
            ..Default::default()
        });
        assert!(
            none.contains("\t-\t"),
            "no hint for a short value: {none:?}"
        );
    }
}
