//! `affected` (SPEC §12A.7, §12A.9): the projects a change touches, in the local engine.
//!
//! The helpers shared with `projects` and `graph` live here: revision resolution, the error
//! mapping (exit 1 for a bad `.asc/projects.toml` or an unmatched selector, 128 for a revision
//! that does not resolve) and the project shape (workspace membership, Contains edges).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::io::Write;
use std::process::{ExitCode, Stdio};

use asc_core::id::ChangesetId;
use asc_core::path::{Name, RepoPath};
use asc_engine::history::HistoryApi;
use asc_engine::projects::{AffectedQuery, AffectedResult, Error as ProjectsError};
use asc_graph::affected::{self as algebra, ChangeKind, ChangedPath, Query, Reason, Side};
use asc_graph::detect::{Claim, claim_of};
use asc_graph::model::{DepKind, Ecosystem, ManifestSet, Project, ProjectGraph, ProjectKey, ProjectKind};
use asc_graph::registry::DETECTORS;
use asc_graph::select::Selector;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::output::Output;
use super::pager::PagerWant;
use super::revparse::commit_of;
use crate::native::NativeSpec;

#[path = "../fmt/affected.rs"]
pub(super) mod fmt_affected;

use fmt_affected::{MatrixBy, Report, Row};

/// The most paths one `--select`, `--type` or `--owner` list may hold.
pub(super) const MAX_ARGS: usize = 4096;

/// The most bytes read from the work tree status.
const MAX_STATUS_BYTES: usize = 256 << 20;

/// A path next to a dirty manifest, standing in for it in the algebra (see [`uncommitted`]).
const PROBE: &[u8] = b"asc-uncommitted-input";

#[derive(clap::Parser, Debug)]
#[command(name = "affected", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(long)]
    pub base: Option<String>,
    #[arg(long)]
    pub head: Option<String>,
    #[arg(long = "no-merge-base")]
    pub no_merge_base: bool,
    #[arg(long)]
    pub uncommitted: bool,
    #[arg(long = "type", value_delimiter = ',')]
    pub types: Vec<String>,
    #[arg(long)]
    pub target: Option<String>,
    #[arg(long)]
    pub select: Vec<String>,
    #[arg(long)]
    pub format: Option<String>,
    #[arg(long = "matrix-by")]
    pub matrix_by: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub explain: bool,
    #[arg(long)]
    pub hashes: bool,
    #[arg(long)]
    pub repo: Option<String>,
    /// Anything else is a usage error.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "affected",
    flags: &[
        "--base", "--head", "--no-merge-base", "--uncommitted", "--type", "--target", "--select",
        "--format", "--matrix-by", "--json", "--explain", "--hashes", "--repo",
    ],
    placeholders: &[],
}];

// ------------------------------------------------------------------ shared helpers

pub(super) fn usage(text: impl std::fmt::Display) -> CliError {
    CliError::Usage(format!("error: {text}"))
}

/// Maps an engine failure: config and selector problems exit 1, the rest are fatal.
pub(super) fn projects_err(e: ProjectsError) -> CliError {
    match e {
        ProjectsError::Config { .. }
        | ProjectsError::Select(_)
        | ProjectsError::Pattern(_)
        | ProjectsError::Resolve(_)
        | ProjectsError::Affected(_) => CliError::Raw(format!("asc: {e}"), 1),
        other => CliError::Fatal(other.to_string()),
    }
}

pub(super) fn select_err(e: asc_graph::select::SelectError) -> CliError {
    CliError::Raw(format!("asc: {e}"), 1)
}

/// `word` as a changeset, or exit 128.
pub(super) async fn rev(ctx: &Ctx, word: &str) -> Result<ChangesetId> {
    commit_of(ctx, word)
        .await?
        .ok_or_else(|| CliError::Fatal(format!("bad revision '{word}'")))
}

/// `--type` values, with the §12A.1 aliases.
pub(super) fn ecosystems(types: &[String]) -> Result<Vec<Ecosystem>> {
    if types.len() > MAX_ARGS {
        return Err(usage("too many --type values"));
    }
    types
        .iter()
        .map(|t| Ecosystem::parse(t).ok_or_else(|| usage(format!("unknown ecosystem '{t}' for --type"))))
        .collect()
}

pub(super) fn selectors(texts: &[String]) -> Result<Vec<Selector>> {
    if texts.len() > MAX_ARGS {
        return Err(usage("too many selectors"));
    }
    texts.iter().map(|t| Selector::parse(t).map_err(select_err)).collect()
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn kind_name(k: ProjectKind) -> &'static str {
    match k {
        ProjectKind::Package => "package",
        ProjectKind::Workspace => "workspace",
        ProjectKind::Solution => "solution",
    }
}

/// A changeset id as a git SHA-1 (SPEC §11.1).
pub(super) async fn git_id(ws: &WsCtx, cs: ChangesetId) -> Result<String> {
    Ok(ws.engine.git_commit_of(cs).await.map_err(|e| CliError::Fatal(e.to_string()))?.to_string())
}

pub(super) fn write_json(out: &mut Output, v: &serde_json::Value) -> Result<()> {
    serde_json::to_writer(&mut *out, v).map_err(std::io::Error::from)?;
    out.write_all(b"\n")?;
    Ok(())
}

pub(super) fn project_at<'a>(graph: &'a ProjectGraph, key: &ProjectKey) -> Option<(usize, &'a Project)> {
    graph.projects.binary_search_by(|p| p.key.cmp(key)).ok().map(|i| (i, &graph.projects[i]))
}

/// Workspace membership and Contains reachability of one graph.
pub(super) struct Shape {
    /// Project index to the index of its Workspace or Solution.
    member_of: HashMap<usize, usize>,
    /// Projects reached from a Node project through Contains edges.
    contained: BTreeSet<usize>,
}

impl Shape {
    pub(super) fn new(graph: &ProjectGraph) -> Self {
        let index = |k: &ProjectKey| graph.projects.binary_search_by(|p| p.key.cmp(k)).ok();
        let mut member_of = HashMap::new();
        for (i, p) in graph.projects.iter().enumerate() {
            if p.kind == ProjectKind::Package {
                continue;
            }
            for (k, kind) in &p.deps {
                if let (DepKind::Member, Some(j)) = (kind, index(k)) {
                    member_of.entry(j).or_insert(i);
                }
            }
        }
        let mut contained = BTreeSet::new();
        let mut queue: Vec<usize> = graph
            .projects
            .iter()
            .enumerate()
            .filter(|(_, p)| p.key.eco == Ecosystem::Node)
            .map(|(i, _)| i)
            .collect();
        while let Some(i) = queue.pop() {
            for (k, kind) in &graph.projects[i].deps {
                if let (DepKind::Contains, Some(j)) = (kind, index(k))
                    && contained.insert(j)
                {
                    queue.push(j);
                }
            }
        }
        Self { member_of, contained }
    }
}

fn row_of(
    graph: &ProjectGraph,
    ids: &[String],
    shape: &Shape,
    p: &algebra::AffectedProject,
    reasons: Vec<String>,
) -> Row {
    let hash = p.input_hash.map(|h| hex(h.as_bytes()));
    match project_at(graph, &p.project.key) {
        Some((i, proj)) => {
            let (workspace, workspace_id, head) = match shape.member_of.get(&i) {
                Some(&w) => (graph.projects[w].root_lossy(), ids[w].clone(), false),
                None => (proj.root_lossy(), ids[i].clone(), true),
            };
            Row {
                id: ids[i].clone(),
                ecosystem: proj.key.eco.as_str(),
                name: proj.name_lossy(),
                root: proj.root_lossy(),
                kind: kind_name(proj.kind),
                direct: p.direct,
                reasons,
                hash,
                turbo_visible: proj.key.eco == Ecosystem::Node
                    && proj.kind == ProjectKind::Package
                    && proj.root.is_some(),
                contained_by_node: shape.contained.contains(&i),
                workspace,
                workspace_id,
                workspace_head: head || proj.kind != ProjectKind::Package,
                targets: asc_graph::targets::commands_for(graph, proj),
            }
        }
        // Not in the head graph (it cannot happen for an affected project): a bare row.
        None => Row {
            id: p.project.id.clone(),
            ecosystem: p.project.key.eco.as_str(),
            name: p.project.id.clone(),
            root: ".".into(),
            kind: "package",
            direct: p.direct,
            reasons,
            hash,
            turbo_visible: false,
            contained_by_node: false,
            workspace: ".".into(),
            workspace_id: p.project.id.clone(),
            workspace_head: true,
            targets: BTreeMap::new(),
        },
    }
}

// ------------------------------------------------------------------ the default base

/// `--base`'s default (SPEC §12A.9): `refs/remotes/origin/HEAD`, else
/// `refs/remotes/origin/<default branch>`, else the local default branch.
async fn default_base(ctx: &Ctx, ws: &WsCtx) -> Result<ChangesetId> {
    let default = match ws.repo.repo.default_branch.as_str() {
        "" => "master",
        b => b,
    };
    let candidates = [
        "refs/remotes/origin/HEAD".to_owned(),
        format!("refs/remotes/origin/{default}"),
        format!("refs/heads/{default}"),
    ];
    for name in &candidates {
        if let Some(cs) = commit_of(ctx, name).await? {
            return Ok(cs);
        }
    }
    Err(CliError::Fatal(format!(
        "cannot find a base revision ({}); pass --base",
        candidates.join(", ")
    )))
}

// ------------------------------------------------------------------ uncommitted paths

/// The staged, unstaged and untracked-but-not-ignored paths of the work tree, from this
/// binary's own `status --porcelain -z`.
fn dirty_paths(ctx: &Ctx, ws: &WsCtx) -> Result<Vec<ChangedPath>> {
    let exe = std::env::current_exe().map_err(|e| CliError::Fatal(format!("cannot find asc: {e}")))?;
    let out = ctx
        .child(exe)?
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames"])
        .current_dir(ws.projection.toplevel())
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()?;
    if !out.status.success() {
        return Err(CliError::Fatal("cannot read the work tree status".into()));
    }
    if out.stdout.len() > MAX_STATUS_BYTES {
        return Err(CliError::Fatal("the work tree status is too large".into()));
    }
    let mut paths = Vec::new();
    for entry in out.stdout.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        if entry.len() < 4 || entry[2] != b' ' {
            return Err(CliError::Fatal("unexpected status output".into()));
        }
        let (x, y) = (entry[0], entry[1]);
        let path = RepoPath::new(entry[3..].to_vec())
            .map_err(|e| CliError::Fatal(format!("unusable path in the work tree: {e}")))?;
        let kind = if x == b'D' || y == b'D' { ChangeKind::Delete } else { ChangeKind::Write };
        paths.push(ChangedPath { path, kind });
    }
    Ok(paths)
}

/// `--uncommitted` always runs locally (SPEC §12A.9). The committed paths and the dirty paths
/// go through the algebra together.
///
/// Both sides use the head graph's manifests as they are committed, with no manifest sets: a
/// project is "manifest-changed" by the path alone. So that a changed manifest or lockfile
/// still affects its project, a probe path (a plain file next to it) is added to the changed
/// paths, and the `file:` reason of the probe is reported as the real path.
async fn uncommitted(
    ctx: &Ctx,
    ws: &WsCtx,
    q: &AffectedQuery,
    selectors: &[Selector],
) -> Result<AffectedResult> {
    let merge_base = if q.no_merge_base {
        q.base
    } else {
        HistoryApi::merge_base(&ws.engine, q.head, &[q.base], false)
            .await
            .map_err(|e| CliError::Fatal(e.to_string()))?
            .first()
            .copied()
            .ok_or_else(|| CliError::Fatal(format!("{} and {} have no common ancestor", q.head, q.base)))?
    };
    let (m, h) = (
        ws.engine.get_changeset(merge_base).await?,
        ws.engine.get_changeset(q.head).await?,
    );
    let mut changed: Vec<ChangedPath> = ws
        .engine
        .leafdiff(m.root, h.root, None)
        .await?
        .into_iter()
        .map(|(path, c)| ChangedPath {
            path,
            kind: match c {
                asc_core::object::Change::Write { .. } => ChangeKind::Write,
                asc_core::object::Change::Delete => ChangeKind::Delete,
                asc_core::object::Change::Gitlink { .. } => ChangeKind::Gitlink,
            },
        })
        .collect();
    let committed = changed.len();
    changed.extend(dirty_paths(ctx, ws)?);
    let (gb, gh) = (
        ws.engine.project_graph(merge_base).await.map_err(projects_err)?,
        ws.engine.project_graph(q.head).await.map_err(projects_err)?,
    );
    let probe = Name::new(PROBE.to_vec()).map_err(|e| CliError::Fatal(e.to_string()))?;
    let mut originals: BTreeMap<RepoPath, RepoPath> = BTreeMap::new();
    for c in changed.clone() {
        let claimed = claim_of(DETECTORS, &gh.graph.config, &c.path)
            .or_else(|| claim_of(DETECTORS, &gb.graph.config, &c.path));
        if matches!(claimed, Some((_, Claim::Manifest | Claim::Lockfile))) {
            let stand_in = match c.path.parent() {
                Some(dir) => dir.join(&probe),
                None => RepoPath::new(PROBE.to_vec()).map_err(|e| CliError::Fatal(e.to_string()))?,
            };
            if !originals.contains_key(&stand_in) {
                originals.insert(stand_in.clone(), c.path.clone());
                changed.push(ChangedPath { path: stand_in, kind: c.kind });
            }
        }
    }
    let empty = ManifestSet::default();
    let query = Query { target: q.target.as_deref(), ecosystems: &q.ecosystems, selectors };
    let mut a = algebra::affected(
        DETECTORS,
        Side { graph: &gb.graph, set: &empty },
        Side { graph: &gh.graph, set: &empty },
        &changed,
        &query,
    )
    .map_err(|e| projects_err(e.into()))?;
    for p in &mut a.projects {
        for r in &mut p.reasons {
            let real = match r {
                Reason::File(path) => originals.get(path),
                _ => None,
            };
            if let Some(real) = real {
                *r = Reason::File(real.clone());
            }
        }
        p.reasons.sort();
        p.reasons.dedup();
    }
    a.unowned.retain(|p| !originals.contains_key(p));
    if q.with_hashes {
        let hashes = ws.engine.input_hashes(q.head, q.target.as_deref().unwrap_or("")).await.map_err(projects_err)?;
        for p in &mut a.projects {
            p.input_hash = hashes
                .0
                .binary_search_by(|(k, _)| k.cmp(&p.project.key))
                .ok()
                .map(|i| hashes.0[i].1.into());
        }
    }
    Ok(AffectedResult {
        base: q.base,
        head: q.head,
        merge_base,
        all: a.all,
        projects: a.projects,
        removed: a.removed,
        unowned: a.unowned,
        issues: a.issues,
        changed_paths: committed + changed.len(),
    })
}

// ------------------------------------------------------------------ the command

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    List,
    Json,
    Turbo,
    Nx,
    Matrix,
    Gha,
}

fn format_of(a: &Args) -> Result<Format> {
    let named = match a.format.as_deref() {
        None => None,
        Some("list") => Some(Format::List),
        Some("json") => Some(Format::Json),
        Some("turbo") => Some(Format::Turbo),
        Some("nx") => Some(Format::Nx),
        Some("matrix") => Some(Format::Matrix),
        Some("gha") => Some(Format::Gha),
        Some(other) => return Err(usage(format!("invalid value '{other}' for --format"))),
    };
    match (named, a.json) {
        (Some(f), true) if f != Format::Json => Err(usage("--json conflicts with --format")),
        (_, true) => Ok(Format::Json),
        (Some(f), false) => Ok(f),
        (None, false) => Ok(Format::List),
    }
}

fn matrix_by(a: &Args) -> Result<MatrixBy> {
    match a.matrix_by.as_deref() {
        None | Some("project") => Ok(MatrixBy::Project),
        Some("workspace") => Ok(MatrixBy::Workspace),
        Some("ecosystem") => Ok(MatrixBy::Ecosystem),
        Some(other) => Err(usage(format!("invalid value '{other}' for --matrix-by"))),
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    if let Some(extra) = a.rest.first() {
        return Err(usage(format!("unexpected argument '{}'", extra.to_string_lossy())));
    }
    let format = format_of(&a)?;
    let by = matrix_by(&a)?;
    if a.repo.is_some() {
        return Err(CliError::Fatal(
            "asc: --repo needs the server path, which this build does not have".into(),
        ));
    }
    let eco = ecosystems(&a.types)?;
    let selectors = selectors(&a.select)?;
    let ws = ctx.ws()?;
    let head = rev(ctx, a.head.as_deref().unwrap_or("HEAD")).await?;
    let base = match a.base.as_deref() {
        Some(word) => rev(ctx, word).await?,
        None => default_base(ctx, ws).await?,
    };
    let view = ws.engine.project_graph(head).await.map_err(projects_err)?;
    if !selectors.is_empty() {
        // A selector that matches nothing is an error (exit 1), not an empty result.
        let chosen = asc_graph::select::select(&view.graph.projects, &selectors).map_err(select_err)?;
        if chosen.is_empty() {
            return Err(CliError::Raw("asc: the selectors match no project".into(), 1));
        }
    }
    let q = AffectedQuery {
        base,
        head,
        no_merge_base: a.no_merge_base,
        target: a.target.clone(),
        ecosystems: eco,
        select: a.select.clone(),
        with_hashes: a.hashes,
    };
    let result = if a.uncommitted {
        uncommitted(ctx, ws, &q, &selectors).await?
    } else {
        ws.engine.affected(&q).await.map_err(projects_err)?
    };
    let graph = &*view.graph;
    let (ids, shape) = (graph.display_ids(), Shape::new(graph));
    let rows = result
        .projects
        .iter()
        .map(|p| {
            let reasons = p.reasons.iter().map(ToString::to_string).collect();
            row_of(graph, &ids, &shape, p, reasons)
        })
        .collect();
    let report = Report {
        base: git_id(ws, result.base).await?,
        head: git_id(ws, result.head).await?,
        merge_base: git_id(ws, result.merge_base).await?,
        all: result.all,
        rows,
        removed: result.removed.iter().map(|r| r.id.clone()).collect(),
        unowned: result.unowned.iter().map(ToString::to_string).collect(),
        issues: result
            .issues
            .iter()
            .map(|i| (i.path.to_string(), String::from_utf8_lossy(&i.message).into_owned()))
            .collect(),
    };
    let mut out = Output::open(ctx, "affected", PagerWant::Never)?;
    let written = emit(&mut out, &report, format, by, a.explain);
    let flushed = out.finish();
    written?;
    flushed?;
    if matches!(format, Format::Turbo | Format::Nx | Format::Gha | Format::Matrix) {
        eprint!("{}", fmt_affected::invisible_note(&report));
    }
    Ok(ExitCode::SUCCESS)
}

fn emit(out: &mut Output, r: &Report, format: Format, by: MatrixBy, explain: bool) -> Result<()> {
    match format {
        Format::List => out.write_all(fmt_affected::list(r, explain).as_bytes())?,
        Format::Json => write_json(out, &fmt_affected::json(r))?,
        Format::Turbo => out.write_all(fmt_affected::turbo(r).as_bytes())?,
        Format::Nx => out.write_all(fmt_affected::nx(r).as_bytes())?,
        Format::Matrix => write_json(out, &fmt_affected::matrix(r, by))?,
        Format::Gha => out.write_all(fmt_affected::gha(r, by).as_bytes())?,
    }
    Ok(())
}
