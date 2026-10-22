//! Non-interactive `git credential fill` through real git.
//!
//! Libraries never call this; the CLI does, and only when the user opted in.

use std::fmt;
use std::io::Write as _;
use std::process::Stdio;

use crate::realgit::{RealGit, RealGitError};

/// Failures of a credential lookup.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// Real git could not be run.
    #[error(transparent)]
    RealGit(#[from] RealGitError),
    /// Spawning or talking to git failed.
    #[error("git credential: {0}")]
    Io(#[from] std::io::Error),
    /// The protocol or host is empty or contains NUL or a newline.
    #[error("git credential: invalid {0}")]
    Invalid(&'static str),
}

/// A username and password returned by a credential helper.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

fn clean(v: &str) -> bool {
    !v.is_empty() && !v.contains(['\0', '\n', '\r'])
}

/// The `git credential fill` request for `protocol` and `host`.
pub fn encode_request(protocol: &str, host: &str) -> Result<String, CredentialError> {
    if !clean(protocol) {
        return Err(CredentialError::Invalid("protocol"));
    }
    if !clean(host) {
        return Err(CredentialError::Invalid("host"));
    }
    Ok(format!("protocol={protocol}\nhost={host}\n\n"))
}

/// Parses helper output; `None` when username or password is missing or unusable.
pub fn parse_response(output: &str) -> Option<Credential> {
    let (mut user, mut pass) = (None, None);
    for line in output.lines() {
        match line.split_once('=') {
            Some(("username", v)) => user = Some(v),
            Some(("password", v)) => pass = Some(v),
            _ => {}
        }
    }
    let (username, password) = (user?, pass?);
    (clean(username) && clean(password)).then(|| Credential {
        username: username.to_owned(),
        password: password.to_owned(),
    })
}

/// Asks the credential helpers for `protocol`/`host`. Never prompts; a failing helper gives `None`.
pub fn fill(
    real: &RealGit,
    protocol: &str,
    host: &str,
) -> Result<Option<Credential>, CredentialError> {
    let request = encode_request(protocol, host)?;
    let mut cmd = real.command()?;
    let mut child = cmd
        .args(["-c", "credential.interactive=false", "credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A helper that exits early closes the pipe; the exit status decides.
        let _ = stdin.write_all(request.as_bytes());
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(parse_response(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_request() {
        assert_eq!(
            encode_request("s3", "bucket").unwrap(),
            "protocol=s3\nhost=bucket\n\n"
        );
        assert!(encode_request("s3", "a\nb").is_err());
        assert!(encode_request("", "h").is_err());
        assert!(encode_request("s3", "a\0b").is_err());
    }

    #[test]
    fn parses_response() {
        let c = parse_response("protocol=s3\nusername=AK\npassword=se=cret\n").unwrap();
        assert_eq!(
            (c.username.as_str(), c.password.as_str()),
            ("AK", "se=cret")
        );
    }

    #[test]
    fn rejects_bad_response() {
        assert!(parse_response("username=AK\n").is_none());
        assert!(parse_response("username=\npassword=x\n").is_none());
        assert!(parse_response("username=AK\npassword=\n").is_none());
        assert!(parse_response("username=A\0K\npassword=x\n").is_none());
        assert!(parse_response("username=AK\npassword=a\rb\n").is_none());
    }

    #[test]
    fn debug_redacts() {
        let c = parse_response("username=AK\npassword=hunter2\n").unwrap();
        let d = format!("{c:?}");
        assert!(!d.contains("AK") && !d.contains("hunter2"));
    }
}
