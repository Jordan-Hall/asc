//! `notes show`, `notes list` and `notes get-ref` (SPEC §11.2): reads of a notes ref. A notes
//! ref names a commit whose tree holds one blob per annotated object, named by the object's
//! hex id and possibly fanned out into two-digit directories (`ab/cdef…`). Every writing
//! subcommand is refused (SPEC §11.2: `notes` writes stay unsupported); the ref and tree can
//! still be made with `hash-object`, `mktree`, `commit-tree` and `update-ref`.

use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TreeId};
use asc_core::object::Kind;
use asc_core::path::RefName;
use asc_store::refs::Refs as _;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::plumbing::objects::{Object, Resolver, stream_blob};
use super::refs_plumbing::args::{Cursor, Word};
use crate::msg::Msg;
use crate::native::NativeSpec;

/// Deepest fan-out followed (a 40-digit id has at most 19 two-digit levels).
const MAX_FANOUT: usize = 19;
/// Most notes `list` reads.
const MAX_NOTES: usize = 10_000_000;

const USAGE: &str = "usage: git notes [--ref <notes-ref>] [list [<object>]]
   or: git notes [--ref <notes-ref>] add [-f] [--allow-empty] [--[no-]separator|--separator=<paragraph-break>] [--[no-]stripspace] [-m <msg> | -F <file> | (-c | -C) <object>] [<object>] [-e]
   or: git notes [--ref <notes-ref>] copy [-f] <from-object> <to-object>
   or: git notes [--ref <notes-ref>] append [--allow-empty] [--[no-]separator|--separator=<paragraph-break>] [--[no-]stripspace] [-m <msg> | -F <file> | (-c | -C) <object>] [<object>] [-e]
   or: git notes [--ref <notes-ref>] edit [--allow-empty] [<object>]
   or: git notes [--ref <notes-ref>] show [<object>]
   or: git notes [--ref <notes-ref>] merge [-v | -q] [-s <strategy>] <notes-ref>
   or: git notes merge --commit [-v | -q]
   or: git notes merge --abort [-v | -q]
   or: git notes [--ref <notes-ref>] remove [<object>...]
   or: git notes [--ref <notes-ref>] prune [-n] [-v]
   or: git notes [--ref <notes-ref>] get-ref

    --[no-]ref <notes-ref>
                          use notes from <notes-ref>
";

const SHOW_USAGE: &str = "usage: git notes show [<object>]\n";
const LIST_USAGE: &str = "usage: git notes list [<object>]\n";
const GET_REF_USAGE: &str = "usage: git notes get-ref\n";

#[derive(clap::Parser, Debug)]
#[command(name = "notes", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "notes",
    flags: &["--ref"],
    placeholders: &[],
}];

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

/// The full name of the notes ref `--ref` (or the default) spells.
fn full_ref(ctx: &Ctx, given: Option<String>) -> String {
    let name = given
        .or_else(|| {
            std::env::var("GIT_NOTES_REF")
                .ok()
                .filter(|v| !v.is_empty())
        })
        .or_else(|| ctx.config.get("core.notesRef"))
        .unwrap_or_else(|| "refs/notes/commits".to_owned());
    if name.starts_with("refs/notes/") {
        name
    } else if name.starts_with("notes/") {
        format!("refs/{name}")
    } else {
        format!("refs/notes/{name}")
    }
}

/// The root tree of the notes ref, `None` when the ref does not exist.
async fn notes_root(ws: &WsCtx, name: &str) -> Result<Option<TreeId>> {
    let Ok(rn) = RefName::new(name.as_bytes().to_vec()) else {
        return Ok(None);
    };
    let Some(entry) = ws.db.refs().get(ws.repo.key, &rn).await? else {
        return Ok(None);
    };
    let commit: ChangesetId = match entry.target {
        RefTarget::Changeset(c) => c,
        RefTarget::Tag(t) => super::log::refs::tag_record(ws, t).await?.target,
    };
    Ok(Some(ws.engine.get_changeset(commit).await?.root))
}

fn is_hex_pair(name: &[u8]) -> bool {
    name.len() == 2 && name.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// One note: the annotated object's hex id and the note blob.
struct Note {
    object: String,
    blob: GitOid,
    file: FileId,
}

/// The note of `hex`, following each fan-out its tree uses.
async fn find_note(ws: &WsCtx, root: TreeId, hex: &str) -> Result<Option<Note>> {
    let mut tree = ws.engine.get_tree(root).await?;
    let mut rest = hex;
    for _ in 0..=MAX_FANOUT {
        // The whole remaining id as a file name?
        if let Ok(name) = asc_core::path::Name::new(rest.as_bytes().to_vec())
            && let Some(e) = tree.get(&name)
            && matches!(e.kind, Kind::File | Kind::Exec | Kind::Symlink)
        {
            return Ok(Some(Note {
                object: hex.to_owned(),
                blob: e.git,
                file: FileId::from_bytes(e.id),
            }));
        }
        if rest.len() <= 2 {
            return Ok(None);
        }
        let Ok(dir) = asc_core::path::Name::new(rest.as_bytes()[..2].to_vec()) else {
            return Ok(None);
        };
        let next = match tree.get(&dir) {
            Some(e) if e.kind == Kind::Dir => TreeId::from_bytes(e.id),
            _ => return Ok(None),
        };
        tree = ws.engine.get_tree(next).await?;
        rest = &rest[2..];
    }
    Ok(None)
}

/// Every note below `root`, by annotated object id.
async fn all_notes(ws: &WsCtx, root: TreeId) -> Result<Vec<Note>> {
    let mut out: Vec<Note> = Vec::new();
    // (tree, hex prefix so far, depth)
    let mut stack: Vec<(TreeId, String, usize)> = vec![(root, String::new(), 0)];
    while let Some((id, prefix, depth)) = stack.pop() {
        let tree = ws.engine.get_tree(id).await?;
        for e in tree.entries() {
            let name = e.name.as_bytes();
            match e.kind {
                Kind::Dir if is_hex_pair(name) && depth < MAX_FANOUT => {
                    let p = format!("{prefix}{}", String::from_utf8_lossy(name));
                    stack.push((TreeId::from_bytes(e.id), p, depth + 1));
                }
                Kind::File | Kind::Exec | Kind::Symlink => {
                    let full = format!("{prefix}{}", String::from_utf8_lossy(name));
                    if full.len() == 40
                        && full.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                    {
                        if out.len() >= MAX_NOTES {
                            return Err(fatal("asc: notes: too many notes"));
                        }
                        out.push(Note {
                            object: full,
                            blob: e.git,
                            file: FileId::from_bytes(e.id),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    out.sort_by(|a, b| a.object.cmp(&b.object));
    Ok(out)
}

/// The object `word` names, or git's "failed to resolve" error.
async fn resolve_object(res: &Resolver<'_>, word: &str) -> Result<Object> {
    match res.resolve(word).await {
        Ok(Some(obj)) => Ok(obj),
        _ => Err(fatal(format!("failed to resolve '{word}' as a valid ref."))),
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let mut cur = Cursor::new(&a.rest, USAGE);
    let mut given: Option<String> = None;
    let mut sub: Option<String> = None;
    let mut operands: Vec<String> = Vec::new();
    while let Some(word) = cur.next() {
        match word {
            Word::End => operands.extend(cur.rest()),
            Word::Operand(p) if sub.is_none() => sub = Some(p),
            Word::Operand(p) => operands.push(p),
            Word::Long(name, inline) if name == "ref" && sub.is_none() => {
                given = Some(cur.value("ref", inline)?);
            }
            Word::Long(name, _) if name == "no-ref" && sub.is_none() => given = None,
            Word::Long(name, _) => {
                return Err(cur.usage_error(&format!("unknown option `{name}'")));
            }
            Word::Short(f) => {
                return Err(cur.usage_error(&format!("unknown switch `{}'", &f[..1])));
            }
        }
    }
    let sub = sub.unwrap_or_else(|| "list".to_owned());
    let name = full_ref(ctx, given);
    match sub.as_str() {
        "get-ref" => {
            if !operands.is_empty() {
                return Err(CliError::Usage(format!(
                    "error: too many arguments\n{GET_REF_USAGE}"
                )));
            }
            println!("{name}");
            Ok(ExitCode::SUCCESS)
        }
        "show" | "list" => {
            let usage = if sub == "show" {
                SHOW_USAGE
            } else {
                LIST_USAGE
            };
            if operands.len() > 1 {
                return Err(CliError::Usage(format!(
                    "error: too many arguments\n{usage}"
                )));
            }
            let ws = ctx.ws()?;
            let res = Resolver::new(ctx, ws);
            let root = notes_root(ws, &name).await?;
            if sub == "list" && operands.is_empty() {
                let Some(root) = root else {
                    return Ok(ExitCode::SUCCESS);
                };
                let mut out = String::new();
                for n in all_notes(ws, root).await? {
                    out.push_str(&format!("{} {}\n", n.blob, n.object));
                }
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(out.as_bytes())?;
                stdout.flush()?;
                return Ok(ExitCode::SUCCESS);
            }
            let word = operands.first().map_or("HEAD", String::as_str);
            let obj = resolve_object(&res, word).await?;
            let hex = obj.oid().to_string();
            let found = match root {
                Some(r) => find_note(ws, r, &hex).await?,
                None => None,
            };
            let Some(note) = found else {
                return Err(CliError::Raw(
                    format!("error: no note found for object {hex}."),
                    1,
                ));
            };
            if sub == "list" {
                println!("{}", note.blob);
                return Ok(ExitCode::SUCCESS);
            }
            let mut stdout = std::io::stdout().lock();
            stream_blob(ws, note.file, &mut stdout).await?;
            stdout.flush()?;
            Ok(ExitCode::SUCCESS)
        }
        "add" | "copy" | "append" | "edit" | "merge" | "remove" | "prune" => {
            Err(CliError::Msg(Msg::AscNotInWorkspace {
                cmd: "notes".to_owned(),
            }))
        }
        other => Err(CliError::Usage(format!(
            "error: unknown subcommand: `{other}'\n{USAGE}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_pairs_are_fan_out_directories() {
        assert!(is_hex_pair(b"ab"));
        assert!(!is_hex_pair(b"AB"));
        assert!(!is_hex_pair(b"abc"));
    }
}
