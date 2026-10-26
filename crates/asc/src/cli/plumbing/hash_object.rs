//! `hash-object` (SPEC §11.2): the git id of file or standard-input content after the line-ending
//! normalisation the path's attributes ask for (SPEC §2.4.1), written to `store/` with `-w`.

use std::ffi::OsString;
use std::io::{Cursor, Read};
use std::path::Path;
use std::process::ExitCode;

use asc_core::content::{Body, Ingested, ingest};
use asc_core::git::ObjectKind;
use asc_core::id::GitOid;
use asc_core::path::RepoPath;
use asc_engine::derive::GitMapRow;
use asc_state::state::git_map;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects as _;
use asc_wc::status::Eol;
use bytes::Bytes;

use super::objects::{hash, kind_named};
use super::usage;
use crate::cli::add::{convert, workspace_attrs};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::refs_plumbing::args::{Cursor as Args, Word};

/// Bytes checked for NUL by git's binary heuristic.
const BINARY_PROBE: usize = 8000;

#[derive(Default)]
struct Opts {
    kind: Option<String>,
    write: bool,
    stdin: bool,
    path: Option<String>,
    no_filters: bool,
    files: Vec<String>,
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Args::new(args, usage::HASH_OBJECT);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.files.extend(cur.rest()),
            Word::Operand(p) => o.files.push(p),
            Word::Short(flags) => {
                let mut chars = flags.chars();
                while let Some(c) = chars.next() {
                    match c {
                        'w' => o.write = true,
                        't' => {
                            let rest: String = chars.by_ref().collect();
                            o.kind = Some(if rest.is_empty() {
                                cur.value("t", None)?
                            } else {
                                rest
                            });
                        }
                        other => return Err(cur.usage_error(&format!("unknown switch `{other}'"))),
                    }
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "stdin" => o.stdin = true,
                "no-filters" => o.no_filters = true,
                "literally" => {}
                "path" => o.path = Some(cur.value("path", inline)?),
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    if o.no_filters && o.path.is_some() {
        return Err(CliError::Usage(format!(
            "error: Can't use --path with --no-filters
{}",
            usage::HASH_OBJECT
        )));
    }
    Ok(o)
}

/// `bytes` through the conversion `eol` asks for.
fn normalize(bytes: Vec<u8>, eol: Eol) -> Vec<u8> {
    if eol == Eol::Keep {
        return bytes;
    }
    let nul = bytes.iter().take(BINARY_PROBE).any(|&b| b == 0);
    let lone_cr = bytes
        .iter()
        .enumerate()
        .any(|(i, &b)| b == b'\r' && bytes.get(i + 1) != Some(&b'\n'));
    if eol == Eol::Auto && (nul || lone_cr) {
        return bytes;
    }
    let mut out = Vec::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            continue;
        }
        out.push(b);
    }
    out
}

/// Writes the file's objects to the object stack and records its git id.
pub(super) async fn store(ws: &WsCtx, got: &Ingested) -> Result<()> {
    match &got.body {
        Body::Inline(bytes) => {
            ws.objects
                .put(&ObjKey::File(got.file), encode_stored(bytes))
                .await?;
        }
        Body::Chunked { list, chunks } => {
            for (id, data) in chunks {
                ws.objects
                    .put(&ObjKey::Chunk(*id), encode_stored(data))
                    .await?;
            }
            let envelope = Bytes::from(asc_core::codec::Enveloped::to_envelope(list));
            ws.objects
                .put(&ObjKey::ChunkList(got.file), encode_stored(&envelope))
                .await?;
        }
    }
    git_map::insert_many(
        &ws.db,
        ws.repo.key,
        &[GitMapRow {
            git: got.git,
            kind: ObjectKind::Blob,
            asc: *got.file.as_bytes(),
        }],
    )
    .await?;
    Ok(())
}

/// The operating system's reason an open failed, as git words it.
fn reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "Permission denied".to_owned(),
        _ => e.to_string(),
    }
}

/// The repository path of `path` when it lies in the work tree.
fn repo_path_of(ws: &WsCtx, path: &Path) -> Option<RepoPath> {
    let root = ws.projection.toplevel();
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
    // The file itself may not exist (`--path`): resolve its directory.
    let resolved = match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => canon(dir).join(name),
        _ => canon(path),
    };
    let rel = resolved.strip_prefix(canon(root)).ok()?.to_owned();
    RepoPath::new(rel.to_string_lossy().replace('\\', "/").into_bytes()).ok()
}

/// What to hash and how to normalise it.
struct Source {
    eol: Eol,
}

fn eol_for(ctx: &Ctx, o: &Opts, repo_path: Option<RepoPath>) -> Result<Source> {
    let (Some(ws), Some(path), false) = (ctx.ws_opt(), repo_path, o.no_filters) else {
        return Ok(Source { eol: Eol::Keep });
    };
    let mut attrs = workspace_attrs(ctx, ws)?;
    let found = attrs
        .lookup(&path)
        .map_err(|e| CliError::Fatal(e.to_string()))?;
    Ok(Source { eol: found.eol })
}

async fn hash_stdin(ctx: &Ctx, o: &Opts, kind: ObjectKind) -> Result<GitOid> {
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;
    let repo_path = match (&o.path, ctx.ws_opt()) {
        (Some(p), Some(ws)) => repo_path_of(ws, &ctx.cwd.join(p)),
        _ => None,
    };
    let source = if kind == ObjectKind::Blob && o.path.is_some() {
        eol_for(ctx, o, repo_path)?
    } else {
        Source { eol: Eol::Keep }
    };
    let bytes = normalize(bytes, source.eol);
    finish(ctx, o, kind, bytes).await
}

async fn finish(ctx: &Ctx, o: &Opts, kind: ObjectKind, bytes: Vec<u8>) -> Result<GitOid> {
    if !o.write {
        return Ok(hash(kind, &bytes));
    }
    if kind != ObjectKind::Blob {
        return Err(CliError::Fatal("asc: only blobs can be written".into()));
    }
    let ws = ctx.ws()?;
    let got = ingest(Cursor::new(bytes)).map_err(|e| CliError::Fatal(e.to_string()))?;
    store(ws, &got).await?;
    Ok(got.git)
}

async fn hash_file(ctx: &Ctx, o: &Opts, kind: ObjectKind, arg: &str) -> Result<GitOid> {
    let path = ctx.cwd.join(arg);
    let open_error = |e: std::io::Error| {
        CliError::Fatal(format!(
            "could not open '{arg}' for reading: {}",
            reason(&e)
        ))
    };
    let meta = std::fs::symlink_metadata(&path).map_err(open_error)?;
    if kind != ObjectKind::Blob {
        let bytes = std::fs::read(&path).map_err(open_error)?;
        return finish(ctx, o, kind, bytes).await;
    }
    if meta.file_type().is_symlink() {
        let bytes = convert::link_target(&path).map_err(open_error)?;
        return finish(ctx, o, kind, bytes).await;
    }
    // `--path` names the location whose attributes apply, whatever the file is called.
    let located = o
        .path
        .as_ref()
        .map_or_else(|| path.clone(), |p| ctx.cwd.join(p));
    let source = eol_for(
        ctx,
        o,
        ctx.ws_opt().and_then(|ws| repo_path_of(ws, &located)),
    )?;
    if !o.write {
        return convert::blob_oid(&path, source.eol).map_err(open_error);
    }
    let ws = ctx.ws()?;
    let got = convert::ingest_file(&path, source.eol, &ws.home.tmp())
        .map_err(|e| CliError::Fatal(e.to_string()))?;
    store(ws, &got).await?;
    Ok(got.git)
}

pub async fn run(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let o = parse(&args)?;
    let kind = match o.kind.as_deref() {
        None => ObjectKind::Blob,
        Some(name) => kind_named(name)
            .ok_or_else(|| CliError::Fatal(format!("invalid object type \"{name}\"")))?,
    };
    if o.write && ctx.ws_opt().is_none() {
        ctx.ws()?;
    }
    let mut out = Vec::new();
    if o.stdin {
        out.push(hash_stdin(ctx, &o, kind).await?);
    }
    for file in &o.files {
        out.push(hash_file(ctx, &o, kind, file).await?);
    }
    for oid in out {
        println!("{oid}");
    }
    Ok(ExitCode::SUCCESS)
}
