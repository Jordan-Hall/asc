//! `projects` (SPEC §12A.9): the project list of a revision, or the owners of paths.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use asc_core::path::RepoPath;
use asc_graph::detect::claim_of;
use asc_graph::own::Ownership;
use asc_graph::registry::DETECTORS;
use asc_graph::select::select;
use serde_json::json;

use super::affected::fmt_affected::clean;
use super::affected::{
    MAX_ARGS, ecosystems, git_id, kind_name, projects_err, rev, select_err, selectors, usage,
    write_json,
};
use super::common::{CliError, Ctx, Result};
use super::output::Output;
use super::pager::PagerWant;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "projects",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    /// Selectors (SPEC §12A.1); none selects every project.
    pub selectors: Vec<String>,
    #[arg(long)]
    pub rev: Option<String>,
    #[arg(long = "type", value_delimiter = ',')]
    pub types: Vec<String>,
    #[arg(long)]
    pub format: Option<String>,
    #[arg(long)]
    pub owner: Vec<String>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "projects",
    flags: &["--rev", "--type", "--format", "--owner"],
    placeholders: &[],
}];

/// The directory of `cwd` below `root` as a `/`-terminated byte string.
fn cwd_prefix(root: &Path, cwd: &Path) -> String {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_owned()
    };
    let (r, c) = (norm(root), norm(cwd));
    let (r_cmp, c_cmp) = if cfg!(windows) {
        (r.to_ascii_lowercase(), c.to_ascii_lowercase())
    } else {
        (r.clone(), c.clone())
    };
    match c_cmp.strip_prefix(&r_cmp) {
        Some(rest) if rest.starts_with('/') => format!("{}/", &c[r.len() + 1..]),
        _ => String::new(),
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let json_out = match a.format.as_deref() {
        None | Some("list") => false,
        Some("json") => true,
        Some(other) => return Err(usage(format!("invalid value '{other}' for --format"))),
    };
    if a.owner.len() > MAX_ARGS {
        return Err(usage("too many --owner paths"));
    }
    let eco = ecosystems(&a.types)?;
    let wanted = selectors(&a.selectors)?;
    let ws = ctx.ws()?;
    let cs = rev(ctx, a.rev.as_deref().unwrap_or("HEAD")).await?;
    let view = ws.engine.project_graph(cs).await.map_err(projects_err)?;
    let graph = &*view.graph;
    let ids = graph.display_ids();
    let mut out = Output::open(ctx, "projects", PagerWant::Never)?;
    let written = if a.owner.is_empty() {
        let mut chosen: Vec<usize> = if wanted.is_empty() {
            (0..graph.projects.len()).collect()
        } else {
            select(&graph.projects, &wanted).map_err(select_err)?
        };
        chosen.retain(|&i| eco.is_empty() || eco.contains(&graph.projects[i].key.eco));
        if !wanted.is_empty() && chosen.is_empty() {
            return Err(CliError::Raw(
                "asc: the selectors match no project".into(),
                1,
            ));
        }
        if json_out {
            let projects: Vec<_> = chosen
                .iter()
                .map(|&i| {
                    let p = &graph.projects[i];
                    json!({"id": ids[i], "ecosystem": p.key.eco.as_str(), "name": p.name_lossy(),
                           "root": p.root_lossy(), "kind": kind_name(p.kind)})
                })
                .collect();
            let issues: Vec<_> = graph
                .issues
                .iter()
                .map(|i| json!({"path": i.path.to_string(), "message": String::from_utf8_lossy(&i.message)}))
                .collect();
            let doc = json!({"version": 1, "rev": git_id(ws, cs).await?, "projects": projects, "issues": issues});
            write_json(&mut out, &doc)
        } else {
            let mut text = String::new();
            for &i in &chosen {
                text.push_str(&clean(&ids[i]));
                text.push('\n');
            }
            out.write_all(text.as_bytes()).map_err(CliError::from)
        }
    } else {
        let own =
            Ownership::new(graph, DETECTORS).map_err(|e| CliError::Raw(format!("asc: {e}"), 1))?;
        let prefix = cwd_prefix(ws.projection.toplevel(), &ctx.cwd);
        let mut lines = Vec::new();
        for raw in &a.owner {
            let text = raw.replace('\\', "/");
            let text = format!("{prefix}{}", text.trim_end_matches('/'));
            let path = RepoPath::new(text.into_bytes())
                .map_err(|e| usage(format!("bad --owner path '{raw}': {e}")))?;
            let owners = own.owners(&path);
            let answer = if !owners.is_empty() {
                owners
                    .iter()
                    .map(|&i| clean(&ids[i]))
                    .collect::<Vec<_>>()
                    .join(",")
            } else if let Some((eco, _)) = claim_of(DETECTORS, &graph.config, &path) {
                format!("claimed by {eco}")
            } else {
                "unowned".to_owned()
            };
            lines.push((path.to_string(), answer));
        }
        if json_out {
            let rows: Vec<_> = lines
                .iter()
                .map(|(p, o)| json!({"path": p, "owner": o}))
                .collect();
            write_json(&mut out, &json!({"version": 1, "owners": rows}))
        } else {
            let text: String = lines
                .iter()
                .map(|(p, o)| format!("{}\t{o}\n", clean(p)))
                .collect();
            out.write_all(text.as_bytes()).map_err(CliError::from)
        }
    };
    let flushed = out.finish();
    written?;
    flushed?;
    Ok(ExitCode::SUCCESS)
}
