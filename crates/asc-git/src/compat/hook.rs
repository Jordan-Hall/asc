//! The `pre-receive` hook protocol (SPEC §25.2 "Writes", §25.8 items 2 and 3; PLAN U82).
//!
//! `git receive-pack` waits for exactly one thing from asc: its asc-owned `pre-receive` hook. The
//! hook is `asc serve git-hook pre-receive`, a few lines of Rust in the `asc` binary. It reads git's
//! command lines from stdin, collects the few environment variables git sets for it, connects to
//! the server's local socket and presents the single-use request token it inherited, then prints
//! the server's refusal reason on stderr and exits 0 or 1.
//!
//! This module is the transport-free half, so that the same bytes are built and read by the
//! client (in `asc`) and the server (`asc-serve::git_push`) and so that every bound is named here:
//!
//! - [`HookInput::collect`]: stdin plus environment into a [`HookRequest`], strictly. Nothing the
//!   hook forwards is trusted by the server; the server re-validates all of it (`receive.rs`).
//! - [`HookRequest::encode`] / [`HookRequest::decode`] and [`HookReply`]: a length-prefixed binary
//!   frame, never longer than [`MAX_REQUEST_BYTES`] and [`MAX_REPLY_BYTES`].
//! - [`HookReply::stderr_text`]: the server's message made safe to print on a terminal.
//!
//! No credential is ever in the hook's argv or environment: the token is a random 256-bit value
//! that names one request on one server process, works once and dies with the request.

use std::ffi::OsString;
use std::io::{self, Read};

use asc_core::id::GitOid;

use super::server_git::{MAX_REF_NAME_BYTES, valid_ref_name};

/// The environment variable that carries the request token into the hook.
pub const ENV_TOKEN: &str = "ASC_GIT_HOOK_TOKEN";
/// The environment variable that names the server's local socket (a path on Unix, a pipe name on
/// Windows).
pub const ENV_ENDPOINT: &str = "ASC_GIT_HOOK_ENDPOINT";
/// The token is 256 bits as lower-case hex.
pub const TOKEN_HEX_LEN: usize = 64;
/// Ref commands in one push. The same cap as `UpdateRefs` (`MAX_REF_UPDATES`).
pub const MAX_COMMANDS: usize = 1024;
/// Push options in one push, and the longest one (as `UpdateRefs`).
pub const MAX_PUSH_OPTIONS: usize = 64;
pub const MAX_PUSH_OPTION_BYTES: usize = 1024;
/// Longest environment value forwarded (a quarantine path or an alternates list).
pub const MAX_ENV_VALUE_BYTES: usize = 8192;
/// Longest endpoint text read from the environment.
pub const MAX_ENDPOINT_BYTES: usize = 4096;
/// Environment variables forwarded: three object-directory variables, the option count and the
/// options themselves.
pub const MAX_ENV_VARS: usize = 3 + 1 + MAX_PUSH_OPTIONS;
/// One command line on git's stdin: two ids, two spaces, a ref name and the newline.
pub const MAX_COMMAND_LINE_BYTES: usize = 40 + 1 + 40 + 1 + MAX_REF_NAME_BYTES + 1;
/// All of git's stdin.
pub const MAX_STDIN_BYTES: usize = MAX_COMMANDS * MAX_COMMAND_LINE_BYTES;
/// A request frame: the token, every command and every variable, with their length fields.
pub const MAX_REQUEST_BYTES: usize = TOKEN_HEX_LEN
    + 8
    + MAX_COMMANDS * (40 + 40 + 2 + MAX_REF_NAME_BYTES)
    + MAX_ENV_VARS * (2 + 64 + 4 + MAX_ENV_VALUE_BYTES);
/// The message of a reply.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
/// A reply frame: the status byte and the message.
pub const MAX_REPLY_BYTES: usize = 1 + MAX_MESSAGE_BYTES;
/// The magic and version at the start of every frame.
pub const MAGIC: &[u8; 4] = b"ASC1";
/// Magic plus the 4-byte big-endian payload length.
pub const HEADER_LEN: usize = 8;

/// Why a frame, git's stdin or the environment could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HookError {
    #[error("the request token is missing or malformed")]
    BadToken,
    #[error("the server endpoint is missing or malformed")]
    BadEndpoint,
    #[error("git's command list is malformed: {0}")]
    BadCommands(&'static str),
    #[error("the push has {0} ref updates, the limit is {MAX_COMMANDS}")]
    TooManyCommands(usize),
    #[error("a forwarded environment variable is malformed: {0}")]
    BadEnv(&'static str),
    #[error("a frame is malformed: {0}")]
    BadFrame(&'static str),
    #[error("a frame is longer than its limit of {0} bytes")]
    TooLong(usize),
}

// ------------------------------------------------------------------------------ the request

/// One ref command from git's `pre-receive` stdin; `None` is git's all-zero id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommand {
    pub old: Option<GitOid>,
    pub new: Option<GitOid>,
    pub name: String,
}

/// What the hook tells the server: the token and what git gave the hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRequest {
    /// 64 lower-case hex digits.
    pub token: String,
    /// The forwarded environment, in the order collected (see [`forwarded_env_name`]).
    pub env: Vec<(String, String)>,
    pub commands: Vec<HookCommand>,
}

/// Whether the hook forwards the variable `name`. The set is closed: the object directories git
/// uses for the quarantine, and the push options.
pub fn forwarded_env_name(name: &str) -> bool {
    matches!(
        name,
        "GIT_QUARANTINE_PATH"
            | "GIT_OBJECT_DIRECTORY"
            | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
            | "GIT_PUSH_OPTION_COUNT"
    ) || name
        .strip_prefix("GIT_PUSH_OPTION_")
        .is_some_and(|n| !n.is_empty() && n.len() <= 4 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether `token` has the shape of a request token.
pub fn valid_token(token: &str) -> bool {
    token.len() == TOKEN_HEX_LEN
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn parse_oid(hex: &str) -> Option<Option<GitOid>> {
    if hex.len() != 40 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let oid: GitOid = hex.parse().ok()?;
    Some((oid.as_bytes() != &[0u8; 20]).then_some(oid))
}

/// Parses git's `pre-receive` stdin: `<old> <new> <ref>\n` per command. Strict: lower-case hex
/// ids, one space between fields, a ref name git would accept, at most [`MAX_COMMANDS`] lines and
/// [`MAX_STDIN_BYTES`] bytes.
pub fn parse_commands<R: Read>(stdin: R) -> Result<Vec<HookCommand>, HookError> {
    let mut bytes = Vec::new();
    stdin
        .take(MAX_STDIN_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| HookError::BadCommands("stdin could not be read"))?;
    if bytes.len() > MAX_STDIN_BYTES {
        return Err(HookError::BadCommands("too long"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| HookError::BadCommands("not UTF-8"))?;
    let mut out = Vec::new();
    for line in text.split_inclusive('\n') {
        let line = line
            .strip_suffix('\n')
            .ok_or(HookError::BadCommands("a line has no newline"))?;
        if out.len() == MAX_COMMANDS {
            return Err(HookError::TooManyCommands(MAX_COMMANDS + 1));
        }
        let mut parts = line.splitn(3, ' ');
        let (Some(old), Some(new), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(HookError::BadCommands("a line does not have three fields"));
        };
        let old = parse_oid(old).ok_or(HookError::BadCommands("an old id is not a SHA-1"))?;
        let new = parse_oid(new).ok_or(HookError::BadCommands("a new id is not a SHA-1"))?;
        if !valid_ref_name(name) {
            return Err(HookError::BadCommands("a ref name is not acceptable"));
        }
        out.push(HookCommand {
            old,
            new,
            name: name.to_owned(),
        });
    }
    if out.is_empty() {
        return Err(HookError::BadCommands("no commands"));
    }
    Ok(out)
}

/// What the hook process has after reading its inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookInput {
    /// Where to connect.
    pub endpoint: String,
    pub request: HookRequest,
}

impl HookInput {
    /// Reads git's stdin and the process environment (`vars`) into a request. A variable that is
    /// forwarded but too long, not UTF-8 or over the count is an error: the hook fails closed.
    pub fn collect<R, I>(stdin: R, vars: I) -> Result<Self, HookError>
    where
        R: Read,
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        let mut token = None;
        let mut endpoint = None;
        let mut env: Vec<(String, String)> = Vec::new();
        for (name, value) in vars {
            let Some(name) = name.to_str() else { continue };
            let is_hook = name == ENV_TOKEN || name == ENV_ENDPOINT;
            if !is_hook && !forwarded_env_name(name) {
                continue;
            }
            let value = value
                .into_string()
                .map_err(|_| HookError::BadEnv("a value is not UTF-8"))?;
            if value.contains('\0') {
                return Err(HookError::BadEnv("a value has a NUL"));
            }
            match name {
                ENV_TOKEN => token = Some(value),
                ENV_ENDPOINT => endpoint = Some(value),
                _ => {
                    if value.len() > MAX_ENV_VALUE_BYTES {
                        return Err(HookError::BadEnv("a value is too long"));
                    }
                    if env.len() == MAX_ENV_VARS {
                        return Err(HookError::BadEnv("too many variables"));
                    }
                    env.push((name.to_owned(), value));
                }
            }
        }
        let token = token
            .filter(|t| valid_token(t))
            .ok_or(HookError::BadToken)?;
        let endpoint = endpoint
            .filter(|e| !e.is_empty() && e.len() <= MAX_ENDPOINT_BYTES)
            .ok_or(HookError::BadEndpoint)?;
        let commands = parse_commands(stdin)?;
        Ok(Self {
            endpoint,
            request: HookRequest {
                token,
                env,
                commands,
            },
        })
    }
}

fn put_u16(out: &mut Vec<u8>, n: usize) -> Result<(), HookError> {
    let n = u16::try_from(n).map_err(|_| HookError::BadFrame("a field is too long"))?;
    out.extend_from_slice(&n.to_be_bytes());
    Ok(())
}

fn put_u32(out: &mut Vec<u8>, n: usize) -> Result<(), HookError> {
    let n = u32::try_from(n).map_err(|_| HookError::BadFrame("a field is too long"))?;
    out.extend_from_slice(&n.to_be_bytes());
    Ok(())
}

/// A bounded cursor over a frame's payload.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], HookError> {
        if self.0.len() < n {
            return Err(HookError::BadFrame("it ends early"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u16(&mut self) -> Result<usize, HookError> {
        let b = self.take(2)?;
        Ok(usize::from(u16::from_be_bytes([b[0], b[1]])))
    }

    fn u32(&mut self) -> Result<usize, HookError> {
        let b = self.take(4)?;
        usize::try_from(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            .map_err(|_| HookError::BadFrame("a length does not fit"))
    }

    fn text(&mut self, n: usize) -> Result<String, HookError> {
        String::from_utf8(self.take(n)?.to_vec())
            .map_err(|_| HookError::BadFrame("a field is not UTF-8"))
    }
}

fn oid_text(oid: Option<GitOid>) -> String {
    oid.map_or_else(|| "0".repeat(40), |o| o.to_string())
}

impl HookRequest {
    /// The frame to send: header and payload.
    pub fn encode(&self) -> Result<Vec<u8>, HookError> {
        if !valid_token(&self.token) {
            return Err(HookError::BadToken);
        }
        if self.commands.len() > MAX_COMMANDS {
            return Err(HookError::TooManyCommands(self.commands.len()));
        }
        if self.env.len() > MAX_ENV_VARS {
            return Err(HookError::BadEnv("too many variables"));
        }
        let mut p = Vec::new();
        p.extend_from_slice(self.token.as_bytes());
        put_u16(&mut p, self.env.len())?;
        for (name, value) in &self.env {
            if !forwarded_env_name(name) || value.len() > MAX_ENV_VALUE_BYTES {
                return Err(HookError::BadEnv("a variable may not be forwarded"));
            }
            put_u16(&mut p, name.len())?;
            p.extend_from_slice(name.as_bytes());
            put_u32(&mut p, value.len())?;
            p.extend_from_slice(value.as_bytes());
        }
        put_u16(&mut p, self.commands.len())?;
        for c in &self.commands {
            if !valid_ref_name(&c.name) {
                return Err(HookError::BadCommands("a ref name is not acceptable"));
            }
            p.extend_from_slice(oid_text(c.old).as_bytes());
            p.extend_from_slice(oid_text(c.new).as_bytes());
            put_u16(&mut p, c.name.len())?;
            p.extend_from_slice(c.name.as_bytes());
        }
        frame(&p, MAX_REQUEST_BYTES)
    }

    /// Reads a payload (without the header) read under [`frame_len`]`(…, MAX_REQUEST_BYTES)`.
    /// Everything is checked again here: the server does not trust the hook process.
    pub fn decode(payload: &[u8]) -> Result<Self, HookError> {
        if payload.len() > MAX_REQUEST_BYTES {
            return Err(HookError::TooLong(MAX_REQUEST_BYTES));
        }
        let mut c = Cursor(payload);
        let token = c.text(TOKEN_HEX_LEN)?;
        if !valid_token(&token) {
            return Err(HookError::BadToken);
        }
        let n = c.u16()?;
        if n > MAX_ENV_VARS {
            return Err(HookError::BadEnv("too many variables"));
        }
        let mut env = Vec::with_capacity(n);
        for _ in 0..n {
            let name_len = c.u16()?;
            if name_len > 64 {
                return Err(HookError::BadEnv("a name is too long"));
            }
            let name = c.text(name_len)?;
            let value_len = c.u32()?;
            if value_len > MAX_ENV_VALUE_BYTES {
                return Err(HookError::BadEnv("a value is too long"));
            }
            let value = c.text(value_len)?;
            if !forwarded_env_name(&name) || value.contains('\0') {
                return Err(HookError::BadEnv("a variable may not be forwarded"));
            }
            env.push((name, value));
        }
        let n = c.u16()?;
        if n == 0 {
            return Err(HookError::BadCommands("no commands"));
        }
        if n > MAX_COMMANDS {
            return Err(HookError::TooManyCommands(n));
        }
        let mut commands = Vec::with_capacity(n);
        for _ in 0..n {
            let old = c.text(40)?;
            let new = c.text(40)?;
            let name_len = c.u16()?;
            if name_len > MAX_REF_NAME_BYTES {
                return Err(HookError::BadCommands("a ref name is too long"));
            }
            let name = c.text(name_len)?;
            if !valid_ref_name(&name) {
                return Err(HookError::BadCommands("a ref name is not acceptable"));
            }
            commands.push(HookCommand {
                old: parse_oid(&old).ok_or(HookError::BadCommands("an old id is not a SHA-1"))?,
                new: parse_oid(&new).ok_or(HookError::BadCommands("a new id is not a SHA-1"))?,
                name,
            });
        }
        if !c.0.is_empty() {
            return Err(HookError::BadFrame("it has trailing bytes"));
        }
        Ok(Self {
            token,
            env,
            commands,
        })
    }

    /// The value of the forwarded variable `name`.
    pub fn env_value(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The push options (`GIT_PUSH_OPTION_COUNT` and `GIT_PUSH_OPTION_<n>`), in order. The count
    /// must agree with the variables present and stay within [`MAX_PUSH_OPTIONS`].
    pub fn push_options(&self) -> Result<Vec<String>, HookError> {
        let Some(count) = self.env_value("GIT_PUSH_OPTION_COUNT") else {
            return Ok(Vec::new());
        };
        let count: usize = count
            .parse()
            .map_err(|_| HookError::BadEnv("the push option count is not a number"))?;
        if count > MAX_PUSH_OPTIONS {
            return Err(HookError::BadEnv("too many push options"));
        }
        (0..count)
            .map(|i| {
                let v = self
                    .env_value(&format!("GIT_PUSH_OPTION_{i}"))
                    .ok_or(HookError::BadEnv("a push option is missing"))?;
                if v.len() > MAX_PUSH_OPTION_BYTES {
                    return Err(HookError::BadEnv("a push option is too long"));
                }
                Ok(v.to_owned())
            })
            .collect()
    }
}

// ------------------------------------------------------------------------------ frames

/// `payload` behind the magic and its length; refuses a payload over `max`.
pub fn frame(payload: &[u8], max: usize) -> Result<Vec<u8>, HookError> {
    if payload.len() > max {
        return Err(HookError::TooLong(max));
    }
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(MAGIC);
    put_u32(&mut out, payload.len())?;
    out.extend_from_slice(payload);
    Ok(out)
}

/// The payload length a frame header announces, if it is ours and within `max`.
pub fn frame_len(header: &[u8; HEADER_LEN], max: usize) -> Result<usize, HookError> {
    if &header[..4] != MAGIC {
        return Err(HookError::BadFrame("it does not start with the magic"));
    }
    let n = usize::try_from(u32::from_be_bytes([
        header[4], header[5], header[6], header[7],
    ]))
    .map_err(|_| HookError::BadFrame("the length does not fit"))?;
    if n > max {
        return Err(HookError::TooLong(max));
    }
    Ok(n)
}

// ------------------------------------------------------------------------------ the reply

/// The server's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReply {
    pub accepted: bool,
    /// Shown on stderr (git relays it as `remote:` lines). Empty is fine on success.
    pub message: String,
}

impl HookReply {
    pub fn accept(message: impl Into<String>) -> Self {
        Self {
            accepted: true,
            message: message.into(),
        }
    }

    pub fn reject(message: impl Into<String>) -> Self {
        Self {
            accepted: false,
            message: message.into(),
        }
    }

    /// The frame to send. The message is made printable and cut to [`MAX_MESSAGE_BYTES`].
    pub fn encode(&self) -> Vec<u8> {
        let mut p = vec![u8::from(!self.accepted)];
        p.extend_from_slice(printable(&self.message, MAX_MESSAGE_BYTES).as_bytes());
        // The payload is within the limit by construction.
        frame(&p, MAX_REPLY_BYTES).unwrap_or_default()
    }

    /// Reads a reply payload. Anything unclear is a refusal: the hook fails closed.
    pub fn decode(payload: &[u8]) -> Self {
        let Some((&status, rest)) = payload.split_first() else {
            return Self::reject("asc: the server sent an empty answer");
        };
        let message = printable(&String::from_utf8_lossy(rest), MAX_MESSAGE_BYTES);
        match status {
            0 => Self::accept(message),
            1 => Self::reject(message),
            _ => Self::reject("asc: the server sent an answer this hook does not understand"),
        }
    }

    /// The text for stderr, one `\n` after every line, or nothing when the message is empty.
    pub fn stderr_text(&self) -> String {
        let text = printable(&self.message, MAX_MESSAGE_BYTES);
        if text.is_empty() {
            String::new()
        } else {
            format!("{}\n", text.trim_end_matches('\n'))
        }
    }

    /// The hook's exit status: 0 accepts the push, 1 declines it.
    pub fn exit_code(&self) -> i32 {
        i32::from(!self.accepted)
    }
}

/// `text` with control characters removed (a newline and a tab stay), cut to `max` bytes on a
/// character boundary. The message is shown on a user's terminal, and a ref name or path in it
/// came from the client.
pub fn printable(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max));
    for ch in text.chars() {
        let keep = ch == '\n' || ch == '\t' || !ch.is_control();
        if !keep {
            continue;
        }
        if out.len() + ch.len_utf8() > max {
            break;
        }
        out.push(ch);
    }
    out
}

/// The refusal the hook prints when it cannot reach the server or read the reply. It names no
/// path and no error text.
pub const UNREACHABLE_MESSAGE: &str = "asc: the push could not be checked by the server; try again";

/// Reads a whole reply frame from a blocking stream (a convenience for tests and tools; the
/// server socket itself is read asynchronously by the caller with [`frame_len`]).
pub fn read_reply<R: Read>(r: &mut R) -> io::Result<HookReply> {
    let mut header = [0u8; HEADER_LEN];
    r.read_exact(&mut header)?;
    let n = frame_len(&header, MAX_REPLY_BYTES)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut payload = vec![0u8; n];
    r.read_exact(&mut payload)?;
    Ok(HookReply::decode(&payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";
    const ZERO: &str = "0000000000000000000000000000000000000000";

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn base_vars() -> Vec<(OsString, OsString)> {
        vars(&[(ENV_TOKEN, TOKEN), (ENV_ENDPOINT, "asc-git-hook-x")])
    }

    #[test]
    fn commands_parse_strictly() {
        let ok = format!(
            "{ZERO} {A} refs/heads/new\n{A} {B} refs/heads/trunk\n{B} {ZERO} refs/tags/x\n"
        );
        let cmds = parse_commands(ok.as_bytes()).unwrap();
        assert_eq!(cmds.len(), 3);
        assert_eq!((cmds[0].old, cmds[0].new.is_some()), (None, true));
        assert_eq!((cmds[2].old.is_some(), cmds[2].new), (true, None));
        for bad in [
            String::new(),
            format!("{A} {B} refs/heads/x"),
            format!("{A}  {B} refs/heads/x\n"),
            format!("{A} {B}\n"),
            format!("{} {B} refs/heads/x\n", "A".repeat(40)),
            format!("{A} {B} refs/heads/a..b\n"),
            format!("{A} {B} refs/heads/a b\n"),
            format!("{A} {B} refs/heads/a\0b\n"),
            format!("{A} {}z refs/heads/x\n", &B[..39]),
            format!("{A} {B} \n"),
        ] {
            assert!(parse_commands(bad.as_bytes()).is_err(), "{bad:?}");
        }
        let many = format!("{A} {B} refs/heads/x\n").repeat(MAX_COMMANDS + 1);
        assert!(matches!(
            parse_commands(many.as_bytes()),
            Err(HookError::TooManyCommands(_))
        ));
        let long = format!("{A} {B} refs/heads/{}\n", "a".repeat(MAX_REF_NAME_BYTES));
        assert!(parse_commands(long.as_bytes()).is_err());
        let huge = vec![b'x'; MAX_STDIN_BYTES + 1];
        assert!(parse_commands(huge.as_slice()).is_err());
    }

    #[test]
    fn collect_forwards_only_the_closed_set() {
        let mut v = base_vars();
        v.extend(vars(&[
            ("GIT_QUARANTINE_PATH", "/v/objects/incoming-x"),
            ("GIT_OBJECT_DIRECTORY", "/v/objects/incoming-x"),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/v/objects"),
            ("GIT_PUSH_OPTION_COUNT", "1"),
            ("GIT_PUSH_OPTION_0", "asc.ttl=1d"),
            ("GIT_DIR", "."),
            ("GIT_CONFIG_COUNT", "1"),
            ("PATH", "/bin"),
            ("GIT_PUSH_OPTION_", "x"),
            ("GIT_PUSH_OPTION_1x", "x"),
        ]));
        let input = HookInput::collect(format!("{A} {B} refs/heads/m\n").as_bytes(), v).unwrap();
        let names: Vec<&str> = input.request.env.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "GIT_QUARANTINE_PATH",
                "GIT_OBJECT_DIRECTORY",
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "GIT_PUSH_OPTION_COUNT",
                "GIT_PUSH_OPTION_0"
            ]
        );
        assert_eq!(input.endpoint, "asc-git-hook-x");
        assert_eq!(input.request.push_options().unwrap(), ["asc.ttl=1d"]);
    }

    #[test]
    fn collect_fails_closed() {
        let stdin = format!("{A} {B} refs/heads/m\n");
        let go = |v: Vec<(OsString, OsString)>| HookInput::collect(stdin.as_bytes(), v);
        assert_eq!(go(vec![]).unwrap_err(), HookError::BadToken);
        assert_eq!(
            go(vars(&[(ENV_TOKEN, "short"), (ENV_ENDPOINT, "e")])).unwrap_err(),
            HookError::BadToken
        );
        assert_eq!(
            go(vars(&[
                (ENV_TOKEN, &TOKEN.to_uppercase()),
                (ENV_ENDPOINT, "e")
            ]))
            .unwrap_err(),
            HookError::BadToken
        );
        assert_eq!(
            go(vars(&[(ENV_TOKEN, TOKEN)])).unwrap_err(),
            HookError::BadEndpoint
        );
        let mut v = base_vars();
        v.push((
            "GIT_QUARANTINE_PATH".into(),
            "x".repeat(MAX_ENV_VALUE_BYTES + 1).into(),
        ));
        assert!(go(v).is_err());
        let mut v = base_vars();
        v.push(("GIT_QUARANTINE_PATH".into(), "a\0b".into()));
        assert!(go(v).is_err());
        let mut v = base_vars();
        for i in 0..=MAX_ENV_VARS {
            v.push((format!("GIT_PUSH_OPTION_{i}").into(), "x".into()));
        }
        assert!(go(v).is_err());
    }

    #[test]
    fn a_request_round_trips_and_is_rechecked_on_decode() {
        let mut v = base_vars();
        v.extend(vars(&[
            ("GIT_QUARANTINE_PATH", "/v/objects/incoming-x"),
            ("GIT_PUSH_OPTION_COUNT", "2"),
            ("GIT_PUSH_OPTION_0", "a=b"),
            ("GIT_PUSH_OPTION_1", "topic=t"),
        ]));
        let stdin = format!("{ZERO} {A} refs/heads/new\n{A} {ZERO} refs/heads/old\n");
        let req = HookInput::collect(stdin.as_bytes(), v).unwrap().request;
        let bytes = req.encode().unwrap();
        let n = frame_len(bytes[..HEADER_LEN].try_into().unwrap(), MAX_REQUEST_BYTES).unwrap();
        assert_eq!(n, bytes.len() - HEADER_LEN);
        let back = HookRequest::decode(&bytes[HEADER_LEN..]).unwrap();
        assert_eq!(back, req);
        assert_eq!(back.push_options().unwrap(), ["a=b", "topic=t"]);

        // Every truncation fails; trailing bytes fail; a smuggled variable fails.
        for cut in 0..bytes.len() - HEADER_LEN {
            assert!(
                HookRequest::decode(&bytes[HEADER_LEN..HEADER_LEN + cut]).is_err(),
                "{cut}"
            );
        }
        let mut extra = bytes[HEADER_LEN..].to_vec();
        extra.push(0);
        assert!(HookRequest::decode(&extra).is_err());
        let smuggled = HookRequest {
            env: vec![("GIT_CONFIG_COUNT".into(), "1".into())],
            ..req.clone()
        };
        assert!(smuggled.encode().is_err());
    }

    #[test]
    fn push_option_counts_must_agree() {
        let mk = |env: &[(&str, &str)]| HookRequest {
            token: TOKEN.into(),
            env: env
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            commands: vec![],
        };
        assert!(
            mk(&[("GIT_PUSH_OPTION_COUNT", "x")])
                .push_options()
                .is_err()
        );
        assert!(
            mk(&[("GIT_PUSH_OPTION_COUNT", "2"), ("GIT_PUSH_OPTION_0", "a")])
                .push_options()
                .is_err()
        );
        assert!(
            mk(&[("GIT_PUSH_OPTION_COUNT", "65")])
                .push_options()
                .is_err()
        );
        assert_eq!(mk(&[]).push_options().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn frames_are_bounded() {
        let ok = frame(b"abc", 8).unwrap();
        assert_eq!(
            frame_len(ok[..HEADER_LEN].try_into().unwrap(), 8).unwrap(),
            3
        );
        assert!(frame(b"abcdefghi", 8).is_err());
        let mut lie = ok[..HEADER_LEN].to_vec();
        lie[4..].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            frame_len(lie.as_slice().try_into().unwrap(), MAX_REQUEST_BYTES),
            Err(HookError::TooLong(_))
        ));
        let mut wrong = ok[..HEADER_LEN].to_vec();
        wrong[0] = b'X';
        assert!(frame_len(wrong.as_slice().try_into().unwrap(), 8).is_err());
    }

    #[test]
    fn replies_are_printable_bounded_and_fail_closed() {
        let r = HookReply::reject("asc: bad \u{1b}[31mpath\u{7}\nsecond line");
        let bytes = r.encode();
        let n = frame_len(bytes[..HEADER_LEN].try_into().unwrap(), MAX_REPLY_BYTES).unwrap();
        let back = HookReply::decode(&bytes[HEADER_LEN..HEADER_LEN + n]);
        assert!(!back.accepted);
        assert_eq!(back.message, "asc: bad [31mpath\nsecond line");
        assert_eq!(back.stderr_text(), "asc: bad [31mpath\nsecond line\n");
        assert_eq!(back.exit_code(), 1);
        assert_eq!(HookReply::accept("").stderr_text(), "");
        assert_eq!(HookReply::accept("").exit_code(), 0);
        // Unknown status and an empty payload decline.
        assert!(!HookReply::decode(&[7, b'x']).accepted);
        assert!(!HookReply::decode(&[]).accepted);
        // Long messages are cut on a character boundary.
        let long = HookReply::reject("é".repeat(MAX_MESSAGE_BYTES));
        let bytes = long.encode();
        assert!(bytes.len() <= HEADER_LEN + MAX_REPLY_BYTES);
        let back = HookReply::decode(&bytes[HEADER_LEN..]);
        assert!(back.message.len() <= MAX_MESSAGE_BYTES);
    }
}
