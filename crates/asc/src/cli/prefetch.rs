//! `asc prefetch <pathspec>… [--history <n>]` (SPEC §10.2 item 5a, §11.2): fills the machine
//! cache with what HEAD holds under the pathspecs, so later reads (a build, a language server, an
//! offline stretch) hit local disk. Files are read through the object stack, which verifies each
//! object against its key and caches what the remote tier supplies; nothing is written to the
//! work tree. With `--history <n>` the changesets and root trees of the `n` newest commits
//! reachable from HEAD are fetched too, so `log` and `diff` work offline over that window.
//!
//! Work is bounded: a fixed read fan-out, no file is held whole (a chunked file is read chunk by
//! chunk), and `--history` stops at [`MAX_HISTORY`].

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::codec::decode_envelope;
use asc_core::content::CHUNK_THRESHOLD;
use asc_core::id::{ChangesetId, FileId};
use asc_core::object::{ChunkList, Kind};
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use futures::StreamExt;

use super::common::{CliError, Ctx, Result};
use super::restore::Spec;
use crate::native::NativeSpec;

const USAGE: &str = "usage: asc prefetch <pathspec>... [--history <n>]";

/// Files read at once.
const READS: usize = 8;

/// Most commits `--history` fetches.
const MAX_HISTORY: u64 = 1_000_000;

/// Most failures listed before the rest are only counted.
const MAX_LISTED: usize = 20;

#[derive(clap::Parser, Debug)]
#[command(
    name = "prefetch",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "prefetch",
    flags: &["--history"],
    placeholders: &[],
}];

#[derive(Debug, PartialEq, Eq)]
struct Opts {
    paths: Vec<OsString>,
    history: u64,
}

fn parse(rest: &[OsString]) -> Result<Opts> {
    let mut o = Opts {
        paths: Vec::new(),
        history: 0,
    };
    let mut it = rest.iter();
    let mut literal = false;
    while let Some(a) = it.next() {
        let text = a.to_string_lossy();
        if literal || !text.starts_with('-') || text == "-" {
            o.paths.push(a.clone());
            continue;
        }
        let (name, inline) = match text.split_once('=') {
            Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
            None => (text.to_string(), None),
        };
        match name.as_str() {
            "--" => literal = true,
            "--history" => {
                let v = match inline {
                    Some(v) => v,
                    None => it
                        .next()
                        .map(|v| v.to_string_lossy().into_owned())
                        .ok_or_else(|| usage("--history needs a number"))?,
                };
                o.history = v
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n <= MAX_HISTORY)
                    .ok_or_else(|| {
                        usage(&format!("--history takes 0 to {MAX_HISTORY}, not '{v}'"))
                    })?;
            }
            other => return Err(usage(&format!("unknown option `{other}'"))),
        }
    }
    if o.paths.is_empty() && o.history == 0 {
        return Err(CliError::Usage(USAGE.to_owned()));
    }
    Ok(o)
}

fn usage(msg: &str) -> CliError {
    CliError::Usage(format!("error: {msg}\n{USAGE}"))
}

/// Reads every object of one file through the stack, never holding more than one chunk.
async fn warm_file(
    objects: &dyn Objects,
    file: FileId,
    size: u64,
) -> std::result::Result<u64, String> {
    let miss = |what: &str| format!("{what} of {file} is not available");
    if size <= CHUNK_THRESHOLD {
        let data = objects
            .get(&ObjKey::File(file))
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| miss("the content"))?;
        return Ok(data.len() as u64);
    }
    let list = objects
        .get(&ObjKey::ChunkList(file))
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| miss("the chunk list"))?;
    let list = decode_envelope::<ChunkList>(&list).map_err(|e| e.to_string())?;
    if list.size != size {
        return Err(format!("the chunk list of {file} disagrees with its size"));
    }
    let mut total = 0u64;
    for (id, len) in &list.chunks {
        let chunk = objects
            .get(&ObjKey::Chunk(*id))
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| miss("a chunk"))?;
        if chunk.len() != *len as usize {
            return Err(format!("a chunk of {file} has the wrong length"));
        }
        total += chunk.len() as u64;
    }
    Ok(total)
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let o = parse(&args.rest)?;
    let ws = ctx.ws()?;
    let head = ws.head_commit().await?;
    let mut failures: Vec<String> = Vec::new();
    let (mut files, mut bytes) = (0u64, 0u64);

    if !o.paths.is_empty()
        && let Some(head) = head
    {
        let spec = Spec::new(ctx, &o.paths)?;
        let root = ws.engine.get_changeset(head).await?.root;
        let objects: &dyn Objects = &*ws.objects;
        let mut work = Box::pin(
            ws.engine
                .walk(root, Some(&spec.spec))
                .filter_map(|r| async move {
                    match r {
                        Ok((path, e))
                            if matches!(e.kind, Kind::File | Kind::Exec | Kind::Symlink) =>
                        {
                            Some(Ok((path, e)))
                        }
                        Ok(_) => None,
                        Err(e) => Some(Err(e)),
                    }
                })
                .map(|r| async move {
                    let (path, e) = r?;
                    let read = warm_file(objects, FileId::from_bytes(e.id), e.size).await;
                    Ok::<_, asc_engine::tree::Error>((path, read))
                })
                .buffer_unordered(READS),
        );
        while let Some(next) = work.next().await {
            let (path, read) = next?;
            match read {
                Ok(n) => {
                    files += 1;
                    bytes = bytes.saturating_add(n);
                }
                Err(why) => failures.push(format!("{path}: {why}")),
            }
        }
    }

    let mut commits = 0u64;
    if o.history > 0
        && let Some(head) = head
    {
        let mut queue: VecDeque<ChangesetId> = VecDeque::from([head]);
        let mut seen: HashSet<ChangesetId> = HashSet::from([head]);
        while commits < o.history {
            let Some(cs) = queue.pop_front() else { break };
            match ws.engine.get_changeset(cs).await {
                Ok(c) => {
                    commits += 1;
                    if let Err(e) = ws.engine.get_tree(c.root).await {
                        failures.push(format!("tree of {cs}: {e}"));
                    }
                    for p in &c.parents {
                        if seen.insert(*p) {
                            queue.push_back(*p);
                        }
                    }
                }
                Err(e) => failures.push(format!("changeset {cs}: {e}")),
            }
        }
    }

    for f in failures.iter().take(MAX_LISTED) {
        eprintln!("error: asc: could not prefetch {f}");
    }
    if failures.len() > MAX_LISTED {
        eprintln!("error: asc: … and {} more", failures.len() - MAX_LISTED);
    }
    println!("prefetched {files} files ({bytes} bytes) and {commits} commits");
    Ok(if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn arguments() {
        assert!(parse(&os(&[])).is_err());
        let o = parse(&os(&["src", "--history", "5", "docs"])).unwrap();
        assert_eq!(o.paths, os(&["src", "docs"]));
        assert_eq!(o.history, 5);
        assert_eq!(parse(&os(&["--history=3"])).unwrap().history, 3);
        assert!(parse(&os(&["--history", "x", "a"])).is_err());
        assert!(parse(&os(&["--history", "9999999999", "a"])).is_err());
        assert!(parse(&os(&["--bogus", "a"])).is_err());
        assert_eq!(parse(&os(&["--", "-odd"])).unwrap().paths, os(&["-odd"]));
    }
}
