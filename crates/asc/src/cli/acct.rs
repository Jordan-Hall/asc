//! What the account and administration commands (`asc backup`, `asc usage`, `asc sessions`,
//! `asc data-export`, `asc ai`) share: owner arguments, JSON output and byte sizes. They are thin
//! wrappers over the `Backups`, `Usage`, `Sessions`, `DataExport` and `AiKeyShares` services; the
//! server decides who may do what.

use std::process::ExitCode;

use asc_rpc::server::wire;

use super::agent::conn::MAX_MESSAGE;
use super::codeintel::{emit_json, lines, with_stdout};
use super::common::{CliError, Result};

/// A usage failure with the text of the command.
pub(crate) fn bad(verb: &str, text: impl Into<String>) -> CliError {
    CliError::Usage(format!("asc {verb}: {}", text.into()))
}

/// `user:NAME`, `org:NAME`, `repo:OWNER/NAME` or `instance`.
pub(crate) fn parse_owner(verb: &str, s: &str) -> Result<wire::OwnerRef> {
    let (kind, name) = match s.split_once(':') {
        Some(("user", n)) => (wire::OwnerKind::User, n),
        Some(("org", n)) => (wire::OwnerKind::Org, n),
        Some(("repo", n)) => (wire::OwnerKind::Repo, n),
        None if s == "instance" => (wire::OwnerKind::Instance, ""),
        _ => {
            return Err(bad(
                verb,
                "an owner is user:NAME, org:NAME, repo:OWNER/NAME or instance",
            ));
        }
    };
    if (kind != wire::OwnerKind::Instance && name.is_empty()) || name.len() > 200 {
        return Err(bad(verb, "the owner has no name"));
    }
    Ok(wire::OwnerRef {
        kind: kind as i32,
        name: name.to_owned(),
    })
}

/// `1.5 GB` style sizes for a terminal.
pub(crate) fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    #[allow(clippy::cast_precision_loss)]
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Prints `msg` as one JSON line of the named wire type.
pub(crate) fn json<M: prost_reflect::prost::Message>(type_name: &str, msg: &M) -> Result<ExitCode> {
    with_stdout(|out| {
        emit_json(&mut lines(out), type_name, msg)?;
        Ok(ExitCode::SUCCESS)
    })
}

pub(crate) const LIMIT: usize = MAX_MESSAGE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owners_and_sizes_parse_and_print() {
        let o = parse_owner("usage", "org:acme").unwrap();
        assert_eq!(
            (o.kind, o.name.as_str()),
            (wire::OwnerKind::Org as i32, "acme")
        );
        assert_eq!(
            parse_owner("usage", "instance").unwrap().kind,
            wire::OwnerKind::Instance as i32
        );
        for bad in ["", "acme", "org:", "team:x", "instance:x"] {
            assert!(parse_owner("usage", bad).is_err(), "{bad:?}");
        }
        assert_eq!(size(999), "999 B");
        assert_eq!(size(18_400_000_000), "18.4 GB");
    }
}
