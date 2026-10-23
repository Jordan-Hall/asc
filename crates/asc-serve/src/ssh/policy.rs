//! What the SSH listener lets a client ask for (SPEC §25.11.4, §25.11.11 item 1).
//!
//! Exactly one thing succeeds: an `exec` request on a `session` channel whose command is
//! `git-upload-pack '<repo>'` or `git-receive-pack '<repo>'`. [`parse_command`] is the bounded,
//! hand-written parser for that command line; no shell reads it, and no part of it becomes an
//! argv element. [`Request`] names every other channel request and channel open the listener
//! refuses, for the audit record and the `asc_ssh_refused_total{request}` metric (a fixed set of
//! labels, so a client cannot grow the metric).

use asc_git::compat::GitProtocol;
use asc_rpc::auth::Access;
use asc_rpc::server::RepoName;

/// Longest accepted exec string, in bytes. A valid command is under 300.
pub const MAX_COMMAND_LINE: usize = 512;
/// Longest accepted quoted argument, quotes included (SPEC §25.11.4: 256 bytes).
pub const MAX_ARG: usize = 256;
/// Longest accepted owner or repository name segment (SPEC §25.8 item 1).
pub const MAX_SEGMENT: usize = 100;
/// The one environment variable a client may set.
pub const GIT_PROTOCOL_VAR: &str = "GIT_PROTOCOL";

/// The two commands the listener runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verb {
    /// `git-upload-pack`: clone and fetch; needs `read`.
    UploadPack,
    /// `git-receive-pack`: push; needs `write`.
    ReceivePack,
}

impl Verb {
    /// The command name as the client spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }

    /// What the key needs on the repository.
    pub fn access(self) -> Access {
        match self {
            Self::UploadPack => Access::Read,
            Self::ReceivePack => Access::Write,
        }
    }
}

/// A parsed, validated command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub verb: Verb,
    /// `owner/name` with no leading `/` and no `.git`: the repository's registry name.
    pub repo: String,
}

/// Why a command line was refused. The client sees one message for all of them
/// (`asc: command not allowed`); the variant is for tests and the fuzz target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// Over [`MAX_COMMAND_LINE`] or [`MAX_ARG`] bytes.
    TooLong,
    /// Not one of the two verbs, or no single space after it.
    Verb,
    /// The argument is not one single-quoted word of plain bytes.
    Quote,
    /// The quoted text is not `[/]owner/name[.git]` by the repository name rules.
    Path,
}

/// Parses an exec string. Total: any byte string gives `Ok` or one [`CommandError`].
///
/// The form is `<verb> '<arg>'`: a verb, one space, then one single-quoted word, which is what
/// git's `sq_quote` produces for a path with no `'` and no `!`. The word has no byte under 0x20,
/// no DEL, no non-ASCII byte, no backslash and no inner quote, so the `'\''` escape is refused
/// along with every shell metacharacter that the name rules would refuse anyway. After one
/// optional leading `/` it is `owner/name` or `owner/name.git`, each segment 1 to
/// [`MAX_SEGMENT`] bytes of `[A-Za-z0-9._-]`, not starting or ending with `.` (so no `.`/`..`
/// segment), and the pair must pass [`RepoName::parse`].
pub fn parse_command(line: &[u8]) -> Result<Command, CommandError> {
    if line.len() > MAX_COMMAND_LINE {
        return Err(CommandError::TooLong);
    }
    let (verb, rest) = if let Some(rest) = line.strip_prefix(b"git-upload-pack ") {
        (Verb::UploadPack, rest)
    } else if let Some(rest) = line.strip_prefix(b"git-receive-pack ") {
        (Verb::ReceivePack, rest)
    } else {
        return Err(CommandError::Verb);
    };
    if rest.len() > MAX_ARG {
        return Err(CommandError::TooLong);
    }
    let inner = match rest {
        [b'\'', inner @ .., b'\''] => inner,
        _ => return Err(CommandError::Quote),
    };
    if inner
        .iter()
        .any(|&b| !(0x20..0x7f).contains(&b) || b == b'\'' || b == b'\\')
    {
        return Err(CommandError::Quote);
    }
    // ASCII by the check above.
    let text = std::str::from_utf8(inner).map_err(|_| CommandError::Quote)?;
    let path = text.strip_prefix('/').unwrap_or(text);
    let (owner, name) = path.split_once('/').ok_or(CommandError::Path)?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    if !segment_ok(owner) || !segment_ok(name) {
        return Err(CommandError::Path);
    }
    let repo = format!("{owner}/{name}");
    RepoName::parse(&repo).map_err(|_| CommandError::Path)?;
    Ok(Command { verb, repo })
}

/// One owner or repository name (SPEC §25.8 item 1).
fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SEGMENT
        && !s.starts_with('.')
        && !s.ends_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The value of an accepted `env` request: only `GIT_PROTOCOL`, and only a value that matches
/// `^version=[0-2](:[a-z0-9=]+)*$` within 256 bytes (SPEC §25.8 item 2). `None` refuses the
/// request.
pub fn env_git_protocol(name: &str, value: &str) -> Option<GitProtocol> {
    if name == GIT_PROTOCOL_VAR {
        GitProtocol::parse(value)
    } else {
        None
    }
}

/// A channel request or channel open the listener refuses, by the name the audit record and the
/// metric use. The set is fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Request {
    Shell,
    Subsystem,
    PtyReq,
    X11Req,
    Env,
    /// An `exec` whose command line did not parse, or a second `exec` on a channel.
    Exec,
    DirectTcpip,
    DirectStreamlocal,
    ForwardedTcpip,
    X11Open,
    TcpipForward,
    CancelTcpipForward,
    StreamlocalForward,
    CancelStreamlocalForward,
    AuthAgentReq,
    /// A channel open past the per-connection cap.
    SessionOpen,
    /// A certificate offered as a login key (not supported in v1).
    Certificate,
}

impl Request {
    /// The request's name on the wire where it has one, else a fixed label.
    pub fn name(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Subsystem => "subsystem",
            Self::PtyReq => "pty-req",
            Self::X11Req => "x11-req",
            Self::Env => "env",
            Self::Exec => "exec",
            Self::DirectTcpip => "direct-tcpip",
            Self::DirectStreamlocal => "direct-streamlocal@openssh.com",
            Self::ForwardedTcpip => "forwarded-tcpip",
            Self::X11Open => "x11",
            Self::TcpipForward => "tcpip-forward",
            Self::CancelTcpipForward => "cancel-tcpip-forward",
            Self::StreamlocalForward => "streamlocal-forward@openssh.com",
            Self::CancelStreamlocalForward => "cancel-streamlocal-forward@openssh.com",
            Self::AuthAgentReq => "auth-agent-req@openssh.com",
            Self::SessionOpen => "session",
            Self::Certificate => "certificate",
        }
    }
}

/// The text written on stderr before exit status 1 for a command that is not allowed.
pub const NOT_ALLOWED: &str = "asc: command not allowed\n";
/// The text for a connection already running its share of commands.
pub const BUSY: &str = "asc: too many concurrent operations, retry later\n";
/// The text when a command ran past `limits.ssh.request_timeout`.
pub const TIMED_OUT: &str = "asc: the operation took too long and was stopped\n";

/// What `shell` answers (so `ssh -T git@host` is a login test).
pub fn shell_greeting(who: &str) -> String {
    let who: String = who.chars().filter(|c| !c.is_control()).take(100).collect();
    format!("Hi {who}! You've successfully authenticated, but asc does not provide shell access.\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(line: &str) -> Command {
        parse_command(line.as_bytes()).unwrap_or_else(|e| panic!("{line:?}: {e:?}"))
    }

    #[test]
    fn the_two_verbs_parse_with_and_without_the_slash_and_suffix() {
        for (line, verb, repo) in [
            ("git-upload-pack 'o/r.git'", Verb::UploadPack, "o/r"),
            ("git-upload-pack '/o/r'", Verb::UploadPack, "o/r"),
            (
                "git-receive-pack 'owner/my-repo_1.x.git'",
                Verb::ReceivePack,
                "owner/my-repo_1.x",
            ),
            ("git-receive-pack '/o/r.git'", Verb::ReceivePack, "o/r"),
        ] {
            let c = ok(line);
            assert_eq!((c.verb, c.repo.as_str()), (verb, repo), "{line}");
        }
        assert_eq!(Verb::UploadPack.access(), Access::Read);
        assert_eq!(Verb::ReceivePack.access(), Access::Write);
    }

    #[test]
    fn everything_else_is_refused() {
        let long_arg = format!("git-upload-pack '{}'", "a/".repeat(129));
        let seg101 = format!("git-upload-pack 'o/{}'", "a".repeat(101));
        let cases: Vec<(&[u8], CommandError)> = vec![
            (b"git-upload-pack '../x'", CommandError::Path),
            (b"git-upload-pack 'o/../x'", CommandError::Path),
            (b"git-upload-pack 'o/.'", CommandError::Path),
            (b"git-upload-pack './r'", CommandError::Path),
            (b"git-upload-pack 'o/.hidden'", CommandError::Path),
            (b"git-upload-pack 'o/r.'", CommandError::Path),
            (b"git-upload-pack 'o/.git'", CommandError::Path),
            (b"git-upload-pack 'o/r;id'", CommandError::Path),
            (b"git-upload-pack '$(id)'", CommandError::Path),
            (b"git-upload-pack 'o/$(id)'", CommandError::Path),
            (b"git-upload-pack 'o/`id`'", CommandError::Path),
            (b"git-upload-pack 'o/r s'", CommandError::Path),
            (b"git-upload-pack 'o'", CommandError::Path),
            (b"git-upload-pack 'o/r/s'", CommandError::Path),
            (b"git-upload-pack '//o/r'", CommandError::Path),
            (b"git-upload-pack ''", CommandError::Path),
            (b"git-upload-pack '", CommandError::Quote),
            (b"git-upload-pack 'a'\\''b'", CommandError::Quote),
            (b"git-upload-pack 'o/r\\x'", CommandError::Quote),
            (b"git-upload-pack o/r", CommandError::Quote),
            (b"git-upload-pack \"o/r\"", CommandError::Quote),
            (b"git-upload-pack 'o/r", CommandError::Quote),
            (b"git-upload-pack  'o/r'", CommandError::Quote),
            (b"git-upload-pack 'o/r' ", CommandError::Quote),
            (b"git-upload-pack 'o/r' 'p/q'", CommandError::Quote),
            (b"git-upload-pack 'o/r'; id", CommandError::Quote),
            (b"git-upload-pack 'o/r\0'", CommandError::Quote),
            (b"git-upload-pack 'o/r\n'", CommandError::Quote),
            (b"git-upload-pack 'o/\x01r'", CommandError::Quote),
            (b"git-upload-pack 'o/r\x7f'", CommandError::Quote),
            (b"git-upload-pack 'o/\xc3\xa9'", CommandError::Quote),
            (b"git-upload-archive 'o/r'", CommandError::Verb),
            (b"git-lfs-authenticate 'o/r' download", CommandError::Verb),
            (b"git upload-pack 'o/r'", CommandError::Verb),
            (b"git-upload-pack", CommandError::Verb),
            (b"git-upload-pack\t'o/r'", CommandError::Verb),
            (b" git-upload-pack 'o/r'", CommandError::Verb),
            (b"GIT-UPLOAD-PACK 'o/r'", CommandError::Verb),
            (b"scp -t /tmp", CommandError::Verb),
            (b"sftp", CommandError::Verb),
            (b"asc-authenticate", CommandError::Verb),
            (b"sh -c id", CommandError::Verb),
            (b"", CommandError::Verb),
        ];
        for (line, why) in cases {
            assert_eq!(
                parse_command(line),
                Err(why),
                "{:?}",
                String::from_utf8_lossy(line)
            );
        }
        assert_eq!(
            parse_command(long_arg.as_bytes()),
            Err(CommandError::TooLong)
        );
        assert_eq!(parse_command(seg101.as_bytes()), Err(CommandError::Path));
        assert_eq!(
            parse_command(&[b'x'; MAX_COMMAND_LINE + 1]),
            Err(CommandError::TooLong)
        );
    }

    #[test]
    fn the_argument_bound_is_256_bytes_with_the_quotes() {
        // 'a/' + padding: owner 100 + '/' + name 100 + leading '/' + quotes stays under 256.
        let longest = format!("git-upload-pack '/{}/{}'", "a".repeat(100), "b".repeat(100));
        assert!(parse_command(longest.as_bytes()).is_ok());
        let over = format!("git-upload-pack '{}'", "a".repeat(255));
        assert_eq!(parse_command(over.as_bytes()), Err(CommandError::TooLong));
    }

    #[test]
    fn env_takes_only_a_valid_git_protocol() {
        assert!(env_git_protocol("GIT_PROTOCOL", "version=2").is_some());
        assert!(env_git_protocol("GIT_PROTOCOL", "version=1:a1=b2:c3").is_some());
        for bad in [
            "version=3",
            "version=2;x",
            "version=2:",
            "",
            "x",
            "version=2 ",
        ] {
            assert!(env_git_protocol("GIT_PROTOCOL", bad).is_none(), "{bad:?}");
        }
        assert!(
            env_git_protocol("GIT_PROTOCOL", &format!("version=2:{}", "a".repeat(300))).is_none()
        );
        for name in [
            "LD_PRELOAD",
            "PATH",
            "GIT_SSH_COMMAND",
            "git_protocol",
            "GIT_PROTOCOLX",
            "LANG",
        ] {
            assert!(env_git_protocol(name, "version=2").is_none(), "{name}");
        }
    }

    #[test]
    fn refused_request_names_are_fixed_and_distinct() {
        let all = [
            Request::Shell,
            Request::Subsystem,
            Request::PtyReq,
            Request::X11Req,
            Request::Env,
            Request::Exec,
            Request::DirectTcpip,
            Request::DirectStreamlocal,
            Request::ForwardedTcpip,
            Request::X11Open,
            Request::TcpipForward,
            Request::CancelTcpipForward,
            Request::StreamlocalForward,
            Request::CancelStreamlocalForward,
            Request::AuthAgentReq,
            Request::SessionOpen,
            Request::Certificate,
        ];
        let mut names: Vec<_> = all.iter().map(|r| r.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len());
    }

    #[test]
    fn the_greeting_has_no_control_bytes() {
        let g = shell_greeting("al\u{1b}[31mice\n");
        assert_eq!(g.matches('\n').count(), 1);
        assert!(!g.contains('\u{1b}'));
        assert!(g.starts_with("Hi al[31mice! You've successfully authenticated"));
    }
}
