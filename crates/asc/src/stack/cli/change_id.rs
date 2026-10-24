//! `asc change-id [--short] [<rev>]` (SPEC §20.7): prints the change id of a commit; nothing
//! and exit 1 for a commit without one.

use std::io::Write;
use std::process::ExitCode;

use asc_engine::change::ChangeRows;
use serde::Serialize;

use super::sl::{clean, fatal};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::native::NativeSpec;
use crate::stack::json::{VERSION, line};

#[derive(clap::Parser, Debug)]
#[command(name = "change-id", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The 12-letter form.
    #[arg(long)]
    pub short: bool,
    #[arg(long)]
    pub json: bool,
    /// The commit; default HEAD.
    pub rev: Option<String>,
}

pub const NATIVE: &[NativeSpec] =
    &[NativeSpec { cmd: "change-id", flags: &["--short", "--json"], placeholders: &[] }];

super::verb!(CHANGE_ID, "change-id");

#[derive(Serialize)]
struct Out {
    version: u32,
    change: Option<String>,
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let word = a.rev.as_deref().unwrap_or("HEAD");
    let mut found = crate::cli::restore::resolve_commit(ctx, word).await?;
    if found.is_none()
        && word.len() == 40
        && let Ok(oid) = word.parse()
    {
        // The engine's revision resolver does not know git ids; the id map does.
        found = crate::cli::conflict::changeset_of(ctx.ws()?, oid).await.ok();
    }
    let cs = found.ok_or_else(|| CliError::Fatal(format!("bad revision '{}'", clean(word))))?;
    let rows = crate::stack::change_rows(ctx)?;
    let change = rows
        .get(cs)
        .await
        .map_err(fatal)?
        .map(|r| if a.short { r.change.short() } else { r.change.to_string() });
    let mut out = Output::open(ctx, "change-id", PagerWant::Never)?;
    if a.json {
        let text = line(&Out { version: VERSION, change: change.clone() }).map_err(fatal)?;
        out.write_all(text.as_bytes())?;
    } else if let Some(c) = &change {
        writeln!(out, "{c}")?;
    }
    out.finish()?;
    Ok(if change.is_some() { ExitCode::SUCCESS } else { ExitCode::from(1) })
}
