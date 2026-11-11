//! `asc cache restore` and `asc cache save` (docs/designs/build-cache.md §4.2): a build as one
//! cached action for a tool that runs the build itself, such as the `@asc/build-cache` plugins of
//! Rsbuild, Rslib and Vite. `restore` keys the build by its inputs and restores the output
//! directories on a hit; on a miss the tool builds and `save` stores the outputs under the key
//! `restore` printed.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use asc_buildcache::{Action, ActionKey};
use serde_json::json;

use super::remote::{self, Mode};
use super::run::{origin, spawn_push};
use super::{io_err, max_size, open_store};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::inputs;

#[derive(clap::Args, Debug)]
pub struct RestoreArgs {
    /// The tool's name: the entries' tool in `asc cache ls` and `status`.
    #[arg(long)]
    tool: String,
    /// The project's name, part of the key and shown in `asc cache ls`.
    #[arg(long)]
    name: String,
    /// Paths whose content keys the build (default: the current directory).
    #[arg(long = "inputs", num_args = 1..)]
    inputs: Vec<OsString>,
    /// The directories or files the build writes, restored on a hit.
    #[arg(long = "outputs", num_args = 1.., required = true)]
    outputs: Vec<OsString>,
    /// More of the key as `NAME=VALUE`: the tool's version, the mode, a digest of options.
    #[arg(long = "part")]
    part: Vec<String>,
    /// Environment variables whose values key the build.
    #[arg(long = "env")]
    env: Vec<String>,
    /// Neither fetch from nor queue for the server.
    #[arg(long = "no-remote")]
    no_remote: bool,
    /// `{"hit": bool, "key": "<hex>"}` on stdout instead of the key alone.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct SaveArgs {
    /// The key `asc cache restore` printed.
    #[arg(long)]
    key: String,
    #[arg(long)]
    tool: String,
    #[arg(long)]
    name: String,
    /// The directories or files the build wrote.
    #[arg(long = "outputs", num_args = 1.., required = true)]
    outputs: Vec<OsString>,
    #[arg(long = "no-remote")]
    no_remote: bool,
}

fn check_tool(tool: &str) -> Result<()> {
    let ok = !tool.is_empty()
        && tool.len() <= 32
        && tool
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(CliError::Usage(format!(
            "error: bad --tool '{tool}' (lower-case letters, digits and '-', at most 32)"
        )))
    }
}

/// The key: the tool, the project, the directory it builds in, the platform, the parts, the
/// named variables and the inputs digest.
fn key(a: &RestoreArgs, prefix: &str, digest: &[u8; 32]) -> Result<ActionKey> {
    let mut b = ActionKey::builder(&a.tool)
        .part("name", a.name.as_bytes())
        .part("cwd", prefix.as_bytes())
        .part(
            "platform",
            format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH).as_bytes(),
        );
    let mut parts: Vec<(&str, &str)> = Vec::new();
    for p in &a.part {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| CliError::Usage(format!("error: --part '{p}' is not NAME=VALUE")))?;
        parts.push((k, v));
    }
    parts.sort();
    for (k, v) in parts {
        b = b.part("part", k.as_bytes()).part("value", v.as_bytes());
    }
    let mut names: Vec<&String> = a.env.iter().collect();
    names.sort();
    names.dedup();
    for name in names {
        b = match std::env::var_os(name) {
            Some(v) => b
                .part("env", name.as_bytes())
                .part("value", v.as_encoded_bytes()),
            None => b.part("unset", name.as_bytes()),
        };
    }
    Ok(b.inputs(digest).finish())
}

/// The outputs as paths from the work tree's root.
fn outputs(ctx: &Ctx, paths: &[OsString]) -> Result<Vec<String>> {
    let ws = ctx.ws()?;
    let root = ws.projection.toplevel();
    let prefix = crate::cli::add::cwd_prefix(ctx, ws);
    paths
        .iter()
        .map(|p| {
            let rel = inputs::normalize(root, &prefix, p)?;
            if rel.is_empty() {
                return Err(CliError::Usage(
                    "error: an output cannot be the repository root".into(),
                ));
            }
            String::from_utf8(rel)
                .map_err(|_| CliError::Usage("error: an output path is not UTF-8".into()))
        })
        .collect()
}

fn mode(ctx: &Ctx, no_remote: bool) -> Result<Mode> {
    if no_remote {
        Ok(Mode::Off)
    } else {
        remote::mode(ctx)
    }
}

pub async fn restore(ctx: &Ctx, a: RestoreArgs, out: &mut impl Write) -> Result<ExitCode> {
    check_tool(&a.tool)?;
    let ws = ctx.ws()?;
    let root = ws.projection.toplevel().to_owned();
    let prefix = crate::cli::add::cwd_prefix(ctx, ws);
    let outs = outputs(ctx, &a.outputs)?;
    let digest = inputs::compute(ctx, ws, &a.inputs, false).await?.digest;
    let key = key(&a, &prefix, &digest)?;
    let store = open_store(ctx)?;
    let origin = origin(&root);
    let mode = mode(ctx, a.no_remote)?;

    let mut found = store.lookup(&a.tool, &key, &origin).map_err(io_err)?;
    if found.is_none() && mode != Mode::Off {
        found = remote::fetch_entry(ctx, &store, &key).await;
    }
    // Only an entry whose blobs are all here touches the outputs. Files already holding the
    // entry's bytes stay as they are (same modification time); the others are written, and what
    // the last build left below the outputs that the entry lacks is removed.
    let mut synced = None;
    if let Some(action) = found {
        synced = store.sync(&action, &root, &outs).map_err(io_err)?;
        if synced.is_some() && mode == Mode::ReadWrite {
            store.queue_push(&key).map_err(io_err)?;
            spawn_push(&store, &ctx.cwd);
        }
    }
    let hit = synced.is_some();
    if a.json {
        let s = synced.unwrap_or_default();
        writeln!(
            out,
            "{}",
            json!({
                "hit": hit,
                "key": key.to_string(),
                "written": s.written,
                "kept": s.kept,
                "removed": s.removed,
            })
        )?;
    } else {
        writeln!(out, "{key}")?;
    }
    Ok(if hit {
        ExitCode::SUCCESS
    } else {
        // A miss is not a failure; `--json` callers read `hit`, others the exit code.
        if a.json {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        }
    })
}

pub fn save(ctx: &Ctx, a: SaveArgs) -> Result<ExitCode> {
    check_tool(&a.tool)?;
    let key = ActionKey::parse(&a.key)
        .ok_or_else(|| CliError::Usage(format!("error: bad --key '{}'", a.key)))?;
    let ws = ctx.ws()?;
    let root = ws.projection.toplevel().to_owned();
    let outs = outputs(ctx, &a.outputs)?;
    let present: Vec<String> = outs
        .into_iter()
        .filter(|o| std::fs::symlink_metadata(root.join(o)).is_ok())
        .collect();
    if present.is_empty() {
        return Err(CliError::Fatal(
            "asc cache save: none of the outputs exists".into(),
        ));
    }
    let store = open_store(ctx)?;
    let mut action = Action::new(&a.tool, &a.name, origin(&root));
    action.outputs = store.put_outputs(&root, &present).map_err(io_err)?;
    store.put_action(&key, &action).map_err(io_err)?;
    if mode(ctx, a.no_remote)? == Mode::ReadWrite {
        store.queue_push(&key).map_err(io_err)?;
        spawn_push(&store, &ctx.cwd);
    }
    if let Ok(max) = max_size(ctx) {
        let _ = store.maybe_gc(max);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(clap::Parser)]
    struct T {
        #[command(flatten)]
        a: RestoreArgs,
    }

    fn args(v: &[&str]) -> RestoreArgs {
        T::try_parse_from(
            ["t", "--tool", "vite", "--name", "app", "--outputs", "dist"]
                .into_iter()
                .chain(v.iter().copied()),
        )
        .unwrap()
        .a
    }

    #[test]
    fn the_key_covers_the_parts_in_any_order() {
        let d = [1u8; 32];
        let k = |v: &[&str]| key(&args(v), "", &d).unwrap();
        let base = k(&["--part", "mode=production", "--part", "version=6"]);
        assert_eq!(
            base,
            k(&["--part", "version=6", "--part", "mode=production"])
        );
        assert_ne!(
            base,
            k(&["--part", "mode=development", "--part", "version=6"])
        );
        assert_ne!(base, k(&["--part", "mode=production"]));
        assert_ne!(
            base,
            key(
                &args(&["--part", "mode=production", "--part", "version=6"]),
                "web",
                &d
            )
            .unwrap()
        );
        assert!(key(&args(&["--part", "novalue"]), "", &d).is_err());
    }

    #[test]
    fn tool_names_are_plain() {
        assert!(check_tool("rsbuild").is_ok());
        assert!(check_tool("vite-2").is_ok());
        for bad in ["", "Vite", "a/b", "../x", &"x".repeat(33)] {
            assert!(check_tool(bad).is_err(), "{bad}");
        }
    }
}
