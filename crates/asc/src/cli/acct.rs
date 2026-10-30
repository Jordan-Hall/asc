//! What the account and administration commands (`asc backup`, `asc usage`, `asc sessions`,
//! `asc data-export`, `asc ai`) share: owner arguments, JSON output and byte sizes. They are thin
//! wrappers over the `Backups`, `Usage`, `Sessions`, `DataExport` and `AiKeyShares` services; the
//! server decides who may do what.

use std::io::Write;
use std::process::ExitCode;

use asc_rpc::server::wire;

use super::agent::conn::MAX_MESSAGE;
use super::codeintel::{clean, emit_json, lines, with_stdout};
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

/// Longest server string shown, in characters.
const SHOWN: usize = 512;

/// A string from the server made safe for a terminal: cut to [`SHOWN`] characters, then every
/// control character, escape sequence introducer, bidirectional control and separator is written
/// as an escape (`\x1b`), so nothing it holds can move the cursor or hide text.
pub(crate) fn clean_str(s: &str) -> String {
    clean(s.chars().take(SHOWN).collect::<String>().as_bytes())
}

/// Multi-line server text: each line cleaned on its own.
pub(crate) fn clean_text(s: &str) -> String {
    s.lines().map(clean_str).collect::<Vec<_>>().join("\n")
}

/// A writer that cleans everything written to it, keeping only line breaks and tabs. All output of
/// these commands goes through it, so no field of a server answer can reach the terminal raw.
pub(crate) struct Clean<W: Write>(pub W);

impl<W: Write> Write for Clean<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for seg in String::from_utf8_lossy(buf).split_inclusive('\n') {
            let (body, nl) = seg.strip_suffix('\n').map_or((seg, ""), |b| (b, "\n"));
            self.0.write_all(clean(body.as_bytes()).as_bytes())?;
            self.0.write_all(nl.as_bytes())?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Runs `body` over a cleaning, locked stdout.
pub(crate) fn out(body: impl FnOnce(&mut dyn Write) -> Result<ExitCode>) -> Result<ExitCode> {
    with_stdout(|w| body(&mut Clean(w)))
}

/// A line to stdout, cleaned.
pub(crate) fn emit_out(text: &str) {
    let _ = writeln!(Clean(std::io::stdout().lock()), "{text}");
}

/// A line to stderr, cleaned.
pub(crate) fn emit_err(text: &str) {
    let _ = writeln!(Clean(std::io::stderr().lock()), "{text}");
}

macro_rules! say {
    ($($t:tt)*) => { $crate::cli::acct::emit_out(&format!($($t)*)) };
}
macro_rules! warn_ {
    ($($t:tt)*) => { $crate::cli::acct::emit_err(&format!($($t)*)) };
}
pub(crate) use {say, warn_};

/// A confirmation word the server asks to be typed: a short plain word, nothing else. Anything
/// else is refused, never shown as an instruction and never typed for the user.
pub(crate) fn confirm_word(s: &str) -> Option<&str> {
    (!s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        .then_some(s)
}

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

    const HOSTILE: &str = "\u{1b}[2J\u{1b}]0;pwned\u{7}\r\u{202e}evil\u{9b}x\u{2028}y\u{7f}";

    #[test]
    fn server_strings_never_carry_a_control_into_the_output() {
        let c = clean_str(HOSTILE);
        assert!(
            !c.chars()
                .any(|ch| ch.is_control() || matches!(ch, '\u{202e}' | '\u{2028}')),
            "{c:?}"
        );
        assert!(c.contains("\\x1b"));
        assert_eq!(clean_str(&"a".repeat(5000)).len(), 512, "cut to a bound");
        assert!(
            !clean_text(&format!("ok\n{HOSTILE}"))
                .chars()
                .any(char::is_control)
                || clean_text("a\nb") == "a\nb"
        );
    }

    #[test]
    fn the_cleaning_writer_escapes_everything_but_line_breaks_and_tabs() {
        let mut sink = Vec::new();
        {
            let mut w = Clean(&mut sink);
            writeln!(w, "id {HOSTILE}\tname {HOSTILE}").unwrap();
            write!(w, "two\nlines").unwrap();
        }
        let text = String::from_utf8(sink).unwrap();
        let body: String = text.chars().filter(|c| *c != '\n' && *c != '\t').collect();
        assert!(
            !body
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{202e}' | '\u{2028}')),
            "{text:?}"
        );
        assert_eq!(text.lines().count(), 3);
        assert!(text.contains('\t'));
    }

    #[test]
    fn a_confirmation_word_is_a_short_plain_word() {
        assert_eq!(confirm_word("RESTORE"), Some("RESTORE"));
        assert_eq!(confirm_word("re-store"), Some("re-store"));
        for bad in [
            "",
            "a b",
            "RESTORE\u{1b}[2J",
            "x;rm -rf",
            &"A".repeat(33),
            "\u{202e}RESTORE",
        ] {
            assert_eq!(confirm_word(bad), None, "{bad:?}");
        }
    }
}
