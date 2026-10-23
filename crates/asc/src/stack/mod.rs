//! The stack verbs (SPEC §20.7): the CLI frame (`cli`), the D5 precedence rules
//! (`precedence`), the `--json` shapes (`json`) and the adapter that gives the engine its
//! change index over `state.db`.

pub mod cli;
pub mod json;
pub mod opstate;
pub mod precedence;

use asc_state::changes::StateChanges;

use crate::cli::common::{Ctx, Result};

/// The engine's [`ChangeRows`](asc_engine::change::ChangeRows) over the workspace's `state.db`
/// (SPEC §20.3): `asc-state` implements the trait, this binds it to the repository of the
/// current workspace. Fails outside a workspace.
pub fn change_rows(ctx: &Ctx) -> Result<StateChanges> {
    let ws = ctx.ws()?;
    Ok(StateChanges::new(ws.db.clone(), ws.repo.key))
}
