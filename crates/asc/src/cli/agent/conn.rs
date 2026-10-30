//! The server connection the `asc agent` and `asc workflow` commands share: the remote, the
//! credential, the retry after a refused credential, and the text of a refusal.
//!
//! Everything the server sends back is text of a repository or of a model, so it is escaped
//! before it reaches a terminal (`clean_str`). A refusal is printed as its typed code and the
//! server's message; neither holds a secret.

use std::future::Future;
use std::sync::Arc;

use asc_rpc::server::wire;
use tonic::{Code, Status};
use uuid::Uuid;

use crate::cli::codeintel::clean_str;
use crate::cli::common::{CliError, Ctx, Result};
use crate::remote::server::{ServerRemote, Session, open_for};

/// Largest message a reply may carry.
pub const MAX_MESSAGE: usize = asc_rpc::server::MAX_MESSAGE;

/// An open server.
pub struct Conn {
    server: Arc<ServerRemote>,
}

fn status_text(s: &Status) -> String {
    let typed = asc_rpc::server::status_rpc_error(s).map(|e| e.code().to_owned());
    let hint = if matches!(s.code(), Code::Unavailable | Code::DeadlineExceeded) {
        " (the server cannot be reached)"
    } else {
        ""
    };
    let message = clean_str(&s.message().chars().take(512).collect::<String>());
    match typed {
        Some(code) if !message.starts_with(&code) => format!("{code}: {message}{hint}"),
        Some(_) => format!("{message}{hint}"),
        None => format!("{:?}: {message}{hint}", s.code()),
    }
}

/// A refusal as the command's failure.
pub fn status_err(s: &Status) -> CliError {
    CliError::Fatal(format!("asc: {}", status_text(s)))
}

impl Conn {
    /// Opens the server of `remote` (a configured remote or a URL; `origin` when `None`).
    pub async fn open(ctx: &Ctx, remote: Option<&str>) -> Result<Self> {
        Ok(Self {
            server: open_for(ctx, remote).await?,
        })
    }

    /// The repository the remote names, as `owner/name`.
    pub fn repo(&self) -> String {
        self.server.target().repo.clone()
    }

    pub fn repo_ref(&self) -> wire::RepoRef {
        wire::RepoRef {
            name: self.repo(),
            id_mode: wire::IdMode::Asc as i32,
        }
    }

    /// Runs `f` against the session; after a refused credential asks for a new one, once.
    /// `mutating` gives the call an operation id so a retry does not repeat it.
    pub async fn call<T, F, Fut>(&self, mutating: bool, mut f: F) -> Result<T>
    where
        F: FnMut(Arc<Session>, Option<Uuid>) -> Fut,
        Fut: Future<Output = std::result::Result<T, Status>>,
    {
        let op = mutating.then(Uuid::now_v7);
        let mut attempt = 0;
        loop {
            let session = self.server.session();
            match f(session.clone(), op).await {
                Ok(v) => {
                    crate::credential::report_result(&session.resolved, true);
                    return Ok(v);
                }
                Err(s) if s.code() == Code::Unauthenticated && attempt == 0 => {
                    attempt += 1;
                    self.server
                        .reauth(&session)
                        .await
                        .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
                }
                Err(s) => {
                    if s.code() == Code::Unauthenticated {
                        crate::credential::report_result(&session.resolved, false);
                    }
                    return Err(status_err(&s));
                }
            }
        }
    }
}

/// Multi-line text of a repository or a command for a terminal: each line cleaned on its own, so
/// the line breaks stay and nothing else that moves a cursor gets through.
pub fn clean_text(s: &str) -> String {
    s.lines()
        .map(|l| clean_str(l.trim_end_matches('\r')))
        .collect::<Vec<_>>()
        .join("\n")
        .to_owned()
}

/// `secs` since the epoch as `YYYY-MM-DD HH:MM:SS` (UTC); `-` for zero.
pub fn when(secs: i64) -> String {
    if secs <= 0 {
        return "-".to_owned();
    }
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The lower-case short name of a proto enum value: `AGENT_RUN_STATE_RUNNING` is `running`.
pub fn short(name: &str, prefix: &str) -> String {
    name.strip_prefix(prefix)
        .unwrap_or(name)
        .to_ascii_lowercase()
}

/// Micro-USD as dollars: `2500000` is `$2.50`.
pub fn dollars(micro: u64) -> String {
    format!("${}.{:06}", micro / 1_000_000, micro % 1_000_000)
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

/// Parses an amount of dollars (`2.5`, `0.000125`) to micro-USD; at most six decimals.
pub fn parse_dollars(s: &str) -> Option<u64> {
    let s = s.trim().trim_start_matches('$');
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if whole.is_empty() && frac.is_empty() || frac.len() > 6 {
        return None;
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let w: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let f: u64 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<6}").parse().ok()?
    };
    w.checked_mul(1_000_000)?.checked_add(f)
}

/// Parses a duration: seconds, or a number with `s`, `m`, `h` or `d`.
pub fn parse_seconds(s: &str) -> Option<u32> {
    let s = s.trim();
    let (n, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&s[..i], &s[i..]),
        None => (s, "s"),
    };
    let n: u64 = n.parse().ok()?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    u32::try_from(n.checked_mul(mult)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_and_durations_parse_exactly() {
        assert_eq!(parse_dollars("2.5"), Some(2_500_000));
        assert_eq!(parse_dollars("$0.000125"), Some(125));
        assert_eq!(parse_dollars("10"), Some(10_000_000));
        assert_eq!(parse_dollars(".5"), Some(500_000));
        for bad in ["", ".", "1.0000001", "-1", "1e3", "a", "1,5"] {
            assert_eq!(parse_dollars(bad), None, "{bad:?}");
        }
        assert_eq!(dollars(2_500_000), "$2.5");
        assert_eq!(dollars(125), "$0.000125");
        assert_eq!(dollars(0), "$0");
        assert_eq!(parse_seconds("90"), Some(90));
        assert_eq!(parse_seconds("15m"), Some(900));
        assert_eq!(parse_seconds("2h"), Some(7200));
        assert_eq!(parse_seconds("7d"), Some(604_800));
        for bad in ["", "m", "1w", "-5", "99999999999d"] {
            assert_eq!(parse_seconds(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn times_and_names_are_plain_text() {
        assert_eq!(when(0), "-");
        assert_eq!(when(1_700_000_000), "2023-11-14 22:13:20");
        assert_eq!(
            short("AGENT_RUN_STATE_WAITING_INPUT", "AGENT_RUN_STATE_"),
            "waiting_input"
        );
    }
}
