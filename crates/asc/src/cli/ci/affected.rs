//! `asc ci affected --repo R --base B --head H [--rules <path>|--rules-file <file>]` (SPEC
//! §12.2): `asc affected --format gha` plus the `{ projects = [...] }` switches of `.asc/ci.toml`.
//! The decisions live in [`asc_ci::affected`]; the server calls are [`super::changed::Cli`].
//! On a server failure the output is `all=true` with every switch on, and a warning.

use std::ffi::OsString;
use std::process::ExitCode;

use crate::cli::common::{Ctx, Result};

use super::changed::{self, Args, Cli};

pub async fn run(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let a: Args = changed::parse("ci affected", rest)?;
    let backend = Cli::new(ctx, changed::remote_url(&a.repo, a.server.as_deref())?)?;
    let plan = asc_ci::affected::affected(&backend, &changed::input(ctx, &a)?)
        .await
        .map_err(changed::plan_err)?;
    changed::emit(&plan)
}
