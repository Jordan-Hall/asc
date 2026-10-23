//! The commit message: cleanup modes, the editor template and the editor.

use std::path::Path;

use crate::cli::common::{CliError, Ctx, Result};

/// The scissors line git writes below the editable part.
const SCISSORS: &str = "------------------------ >8 ------------------------";

/// `--cleanup=<mode>` / `commit.cleanup`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cleanup {
    Strip,
    Whitespace,
    Verbatim,
    Scissors,
    /// `strip` when an editor runs, else `whitespace`.
    Default,
}

impl Cleanup {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "strip" => Self::Strip,
            "whitespace" => Self::Whitespace,
            "verbatim" => Self::Verbatim,
            "scissors" => Self::Scissors,
            "default" => Self::Default,
            _ => return None,
        })
    }

    fn resolve(self, edited: bool) -> Self {
        match self {
            Self::Default if edited => Self::Strip,
            Self::Default => Self::Whitespace,
            other => other,
        }
    }
}

/// The comment character: `core.commentChar` when it is one ASCII character, else `#`.
pub fn comment_char(ctx: &Ctx) -> u8 {
    match ctx.config.get("core.commentChar").as_deref() {
        Some(c) if c.len() == 1 && c.is_ascii() && c != "auto" => c.as_bytes()[0],
        _ => b'#',
    }
}

/// git's `stripspace`: trailing blanks off every line, runs of empty lines collapsed to one,
/// leading and trailing empty lines dropped, one final newline.
fn whitespace(lines: impl Iterator<Item = Vec<u8>>) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut pending_blank = false;
    for mut line in lines {
        while line.last().is_some_and(u8::is_ascii_whitespace) {
            line.pop();
        }
        if line.is_empty() {
            pending_blank = !out.is_empty();
            continue;
        }
        if pending_blank {
            out.push(b'\n');
            pending_blank = false;
        }
        out.extend_from_slice(&line);
        out.push(b'\n');
    }
    out
}

/// Cleans `text` as git does for `mode`; `edited` picks what `Default` means.
pub fn cleanup(text: &[u8], mode: Cleanup, edited: bool, comment: u8) -> Vec<u8> {
    let mode = mode.resolve(edited);
    if mode == Cleanup::Verbatim {
        return text.to_vec();
    }
    let scissors = format!("{} {SCISSORS}", comment as char).into_bytes();
    let mut lines: Vec<Vec<u8>> = text.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
    if text.ends_with(b"\n") {
        lines.pop();
    }
    if matches!(mode, Cleanup::Scissors | Cleanup::Strip)
        && let Some(at) = lines.iter().position(|l| *l == scissors)
    {
        lines.truncate(at);
    }
    if mode == Cleanup::Strip {
        lines.retain(|l| l.first() != Some(&comment));
    }
    whitespace(lines.into_iter())
}

/// The subject: the first paragraph of a cleaned message on one line.
pub fn subject(message: &[u8]) -> String {
    let text = String::from_utf8_lossy(message);
    let mut parts = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if parts.is_empty() {
                continue;
            }
            break;
        }
        // git's `format_subject` keeps leading blanks and drops trailing ones.
        parts.push(line.trim_end());
    }
    parts.join(" ")
}

/// What the editor shows below the message.
pub fn template(comment: u8, idents: &[u8], status: &[u8], verbose_diff: Option<&[u8]>) -> Vec<u8> {
    let c = comment as char;
    let mut bytes = format!(
        "{c} Please enter the commit message for your changes. Lines starting\n\
         {c} with '{c}' will be ignored, and an empty message aborts the commit.\n{c}\n"
    )
    .into_bytes();
    bytes.extend_from_slice(idents);
    bytes.extend_from_slice(status);
    if let Some(diff) = verbose_diff {
        bytes.extend_from_slice(
            format!(
                "{c} {SCISSORS}\n{c} Do not modify or remove the line above.\n\
                 {c} Everything below it will be ignored.\n"
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(diff);
    }
    bytes
}

/// The editor command: `GIT_EDITOR`, `core.editor`, `VISUAL`, `EDITOR`, then `vi`.
pub fn editor(ctx: &Ctx) -> String {
    let from_env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    ctx.env
        .editor
        .as_ref()
        .map(|e| e.to_string_lossy().into_owned())
        .filter(|e| !e.is_empty())
        .or_else(|| ctx.config.get("core.editor"))
        .or_else(|| from_env("VISUAL"))
        .or_else(|| from_env("EDITOR"))
        .unwrap_or_else(|| "vi".to_owned())
}

/// Runs the editor on `file` as git does (`sh -c '<editor> "$@"' <editor> <file>`). `Ok(false)`
/// means the editor failed.
pub async fn edit(ctx: &Ctx, file: &Path) -> Result<bool> {
    let editor = editor(ctx);
    if editor == ":" {
        return Ok(true);
    }
    let mut cmd = ctx.child(ctx.shell())?;
    cmd.arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(&editor)
        .arg(file);
    let status = tokio::task::spawn_blocking(move || cmd.status())
        .await
        .map_err(|e| CliError::Fatal(e.to_string()))??;
    if !status.success() {
        eprintln!("error: There was a problem with the editor '{editor}'.");
    }
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_cleanup_collapses_and_trims() {
        let got = cleanup(
            b"\n\nsubject  \n\n\n\nbody\t\n\n",
            Cleanup::Whitespace,
            false,
            b'#',
        );
        assert_eq!(got, b"subject\n\nbody\n");
    }

    #[test]
    fn strip_drops_comments_and_everything_below_scissors() {
        let text =
            b"msg\n# comment\n\n# ------------------------ >8 ------------------------\ndiff\n";
        assert_eq!(cleanup(text, Cleanup::Default, true, b'#'), b"msg\n");
        // Without an editor the default keeps comment lines.
        assert_eq!(
            cleanup(b"msg\n# kept\n", Cleanup::Default, false, b'#'),
            b"msg\n# kept\n"
        );
    }

    #[test]
    fn verbatim_is_untouched_and_empty_stays_empty() {
        assert_eq!(
            cleanup(b"a  \n\n\n", Cleanup::Verbatim, false, b'#'),
            b"a  \n\n\n"
        );
        assert!(cleanup(b"\n  \n# only\n", Cleanup::Strip, true, b'#').is_empty());
    }

    #[test]
    fn subject_is_the_first_paragraph_on_one_line() {
        assert_eq!(subject(b"one\ntwo\n\nbody\n"), "one two");
        assert_eq!(subject(b"\n\nonly\n"), "only");
        assert_eq!(subject(b"  spaced   message  \n"), "  spaced   message");
    }
}
