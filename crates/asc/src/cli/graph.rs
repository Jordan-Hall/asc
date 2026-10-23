//! `graph` (SPEC §12A.9): the project graph of a revision as JSON or Graphviz.

use std::io::Write;
use std::process::ExitCode;

use asc_graph::model::DepKind;
use serde_json::json;

use super::affected::{ecosystems, git_id, hex, kind_name, projects_err, rev, usage, write_json};
use super::common::{CliError, Ctx, Result};
use super::output::Output;
use super::pager::PagerWant;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "graph", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(long)]
    pub rev: Option<String>,
    #[arg(long = "type", value_delimiter = ',')]
    pub types: Vec<String>,
    #[arg(long)]
    pub format: Option<String>,
    #[arg(long)]
    pub hashes: bool,
    #[arg(long)]
    pub target: Option<String>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "graph",
    flags: &["--rev", "--type", "--format", "--hashes", "--target"],
    placeholders: &[],
}];

fn dep_name(k: DepKind) -> &'static str {
    match k {
        DepKind::Normal => "normal",
        DepKind::Dev => "dev",
        DepKind::Build => "build",
        DepKind::Peer => "peer",
        DepKind::Optional => "optional",
        DepKind::Implicit => "implicit",
        DepKind::Contains => "contains",
        DepKind::Member => "member",
    }
}

/// A Graphviz quoted string: backslash, quote and control characters escaped.
fn dot_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => out.push('?'),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let dot = match a.format.as_deref() {
        None | Some("json") => false,
        Some("dot") => true,
        Some(other) => return Err(usage(format!("invalid value '{other}' for --format"))),
    };
    if a.target.is_some() && !a.hashes {
        return Err(usage("--target needs --hashes"));
    }
    let eco = ecosystems(&a.types)?;
    let ws = ctx.ws()?;
    let cs = rev(ctx, a.rev.as_deref().unwrap_or("HEAD")).await?;
    let view = ws.engine.project_graph(cs).await.map_err(projects_err)?;
    let graph = &*view.graph;
    let ids = graph.display_ids();
    let hashes = if a.hashes {
        Some(
            ws.engine
                .input_hashes(cs, a.target.as_deref().unwrap_or(""))
                .await
                .map_err(projects_err)?,
        )
    } else {
        None
    };
    let keep = |i: usize| eco.is_empty() || eco.contains(&graph.projects[i].key.eco);
    let index =
        |k: &asc_graph::model::ProjectKey| graph.projects.binary_search_by(|p| p.key.cmp(k)).ok();
    let hash_of = |i: usize| {
        hashes.as_ref().and_then(|h| {
            h.0.binary_search_by(|(k, _)| k.cmp(&graph.projects[i].key))
                .ok()
                .map(|j| hex(&h.0[j].1))
        })
    };
    let mut out = Output::open(ctx, "graph", PagerWant::Never)?;
    let written = if dot {
        let mut text = String::from("digraph projects {\n");
        for i in (0..graph.projects.len()).filter(|&i| keep(i)) {
            text.push_str(&format!("  {};\n", dot_quote(&ids[i])));
        }
        for i in (0..graph.projects.len()).filter(|&i| keep(i)) {
            for (k, kind) in &graph.projects[i].deps {
                if let Some(j) = index(k).filter(|&j| keep(j)) {
                    let label = if *kind == DepKind::Normal {
                        String::new()
                    } else {
                        format!(" [label={}]", dot_quote(dep_name(*kind)))
                    };
                    text.push_str(&format!(
                        "  {} -> {}{label};\n",
                        dot_quote(&ids[i]),
                        dot_quote(&ids[j])
                    ));
                }
            }
        }
        text.push_str("}\n");
        out.write_all(text.as_bytes()).map_err(CliError::from)
    } else {
        let projects: Vec<_> = (0..graph.projects.len())
            .filter(|&i| keep(i))
            .map(|i| {
                let p = &graph.projects[i];
                let deps: Vec<_> = p
                    .deps
                    .iter()
                    .filter_map(|(k, kind)| index(k).filter(|&j| keep(j)).map(|j| json!({"id": ids[j], "kind": dep_name(*kind)})))
                    .collect();
                json!({"id": ids[i], "ecosystem": p.key.eco.as_str(), "name": p.name_lossy(),
                       "root": p.root_lossy(), "kind": kind_name(p.kind), "deps": deps, "hash": hash_of(i)})
            })
            .collect();
        let issues: Vec<_> = graph
            .issues
            .iter()
            .map(|i| json!({"path": i.path.to_string(), "message": String::from_utf8_lossy(&i.message)}))
            .collect();
        let doc = json!({"version": 1, "rev": git_id(ws, cs).await?, "projects": projects, "issues": issues});
        write_json(&mut out, &doc)
    };
    let flushed = out.finish();
    written?;
    flushed?;
    Ok(ExitCode::SUCCESS)
}
