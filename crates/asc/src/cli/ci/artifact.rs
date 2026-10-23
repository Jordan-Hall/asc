//! `ci artifact`: a stub until its unit lands (U63 changed/affected/warm, U75 cache/artifact).

use std::process::ExitCode;

use crate::cli::common::{CliError, Result};
use crate::msg::Msg;

/// Nothing is served until the owning unit lands.
pub fn run() -> Result<ExitCode> {
    Err(CliError::Msg(Msg::AscNotYet {
        cmd: "ci artifact".to_owned(),
    }))
}
