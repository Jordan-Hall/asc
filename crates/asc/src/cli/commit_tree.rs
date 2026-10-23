//! `commit-tree` (SPEC §11.2): a commit with the given tree and parents, printed by id. Nothing
//! else changes: no ref moves, no hook runs. The message is the `-m` / `-F` pieces, or standard
//! input when neither is given, stored byte for byte.

use std::ffi::OsString;
use std::io::Read;
use std::process::ExitCode;

use asc_core::git::ObjectKind;
use asc_core::id::ChangesetId;

use super::common::{CliError, Ctx, Result};
use super::conflict::{self, NewCommit};
use super::plumbing::objects::{Object, Resolver};
use super::refs_plumbing::args::{Cursor, Word};
use crate::native::NativeSpec;

/// Largest message read from a file or standard input.
const MAX_MESSAGE: u64 = 64 << 20;

const USAGE: &str = "usage: git commit-tree <tree> [(-p <parent>)...]
   or: git commit-tree [(-p <parent>)...] [-S[<keyid>]] [(-m <message>)...]
                       [(-F <file>)...] <tree>

    -p <parent>           id of a parent commit object
    -m <message>          commit message
    -F <file>             read commit log message from file
    -S, --[no-]gpg-sign[=<key-id>]
                          GPG sign commit
";

#[derive(clap::Parser, Debug)]
#[command(
    name = "commit-tree",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "commit-tree",
    flags: &["-p", "-m", "-F"],
    placeholders: &[],
}];

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

/// One piece of the message, in command-line order.
enum Piece {
    Text(String),
    File(String),
}

#[derive(Default)]
struct Opts {
    parents: Vec<String>,
    pieces: Vec<Piece>,
    operands: Vec<String>,
}

/// The value of a short switch: attached (`-mtext`) or the next word.
fn switch_value(cur: &mut Cursor, name: &str, inline: Option<String>) -> Result<String> {
    cur.value(name, inline)
        .map_err(|_| CliError::Raw(format!("error: switch `{name}' requires a value"), 129))
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, USAGE);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.operands.extend(cur.rest()),
            Word::Operand(p) => o.operands.push(p),
            Word::Short(flags) => {
                let (c, attached) = flags.split_at(1);
                let inline = (!attached.is_empty()).then(|| attached.to_owned());
                match c {
                    "p" => o.parents.push(switch_value(&mut cur, "p", inline)?),
                    "m" => o
                        .pieces
                        .push(Piece::Text(switch_value(&mut cur, "m", inline)?)),
                    "F" => o
                        .pieces
                        .push(Piece::File(switch_value(&mut cur, "F", inline)?)),
                    other => {
                        return Err(cur.usage_error(&format!("unknown switch `{other}'")));
                    }
                }
            }
            Word::Long(name, _) => {
                return Err(cur.usage_error(&format!("unknown option `{name}'")));
            }
        }
    }
    Ok(o)
}

/// Reads up to [`MAX_MESSAGE`] bytes of `reader`.
fn read_bounded(reader: impl Read, what: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader.take(MAX_MESSAGE + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_MESSAGE {
        return Err(fatal(format!("{what} is too large")));
    }
    Ok(buf)
}

/// The message git builds from the pieces: each piece after the first is separated from the
/// text before it by a blank line; `-m` text is completed with a newline, a file is taken as is.
fn message(pieces: &[Piece]) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    for piece in pieces {
        if !buf.is_empty() {
            buf.push(b'\n');
        }
        match piece {
            Piece::Text(t) => {
                buf.extend_from_slice(t.as_bytes());
                if !buf.ends_with(b"\n") {
                    buf.push(b'\n');
                }
            }
            Piece::File(path) => {
                let bytes = if path == "-" {
                    read_bounded(std::io::stdin().lock(), "standard input")?
                } else {
                    let file = std::fs::File::open(path).map_err(|e| {
                        fatal(format!("git commit-tree: failed to read '{path}': {e}"))
                    })?;
                    read_bounded(file, path)?
                };
                buf.extend_from_slice(&bytes);
            }
        }
    }
    Ok(buf)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = parse(&a.rest)?;
    if o.operands.len() != 1 {
        return Err(fatal("must give exactly one tree"));
    }
    let ws = ctx.ws()?;
    let res = Resolver::new(ctx, ws);
    let word = &o.operands[0];
    let tree = match res.resolve(word).await? {
        Some(Object::Tree { id, .. }) => id,
        Some(other) => {
            return Err(fatal(format!(
                "{} is not a valid 'tree' object",
                other.oid()
            )));
        }
        None => return Err(fatal(format!("not a valid object name {word}"))),
    };
    let mut parents: Vec<ChangesetId> = Vec::new();
    for p in &o.parents {
        let Some(obj) = res.resolve(p).await? else {
            return Err(fatal(format!("not a valid object name {p}")));
        };
        let Some(Object::Commit { id, oid }) = res.peel(obj, ObjectKind::Commit).await? else {
            return Err(fatal(format!(
                "{} is not a valid 'commit' object",
                obj.oid()
            )));
        };
        if parents.contains(&id) {
            eprintln!("error: duplicate parent {oid} ignored");
            continue;
        }
        parents.push(id);
    }
    let text = if o.pieces.is_empty() {
        read_bounded(std::io::stdin().lock(), "standard input")?
    } else {
        message(&o.pieces)?
    };
    let idents = conflict::identities(ctx)?;
    let (_, oid) = conflict::write_commit(
        ws,
        NewCommit {
            parents,
            author: idents.author,
            committer: idents.committer,
            message: text,
            root: tree,
        },
    )
    .await?;
    res.flush().await?;
    println!("{oid}");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_are_separated_by_blank_lines() {
        let got = message(&[Piece::Text("x".into()), Piece::Text("y".into())]).unwrap();
        assert_eq!(got, b"x\n\ny\n");
    }

    #[test]
    fn parse_collects_parents_and_messages() {
        let args: Vec<OsString> = ["-p", "a", "-mhello", "tree", "-p", "b"]
            .iter()
            .map(OsString::from)
            .collect();
        let o = parse(&args).unwrap();
        assert_eq!(o.parents, ["a", "b"]);
        assert_eq!(o.operands, ["tree"]);
        assert_eq!(o.pieces.len(), 1);
    }
}
