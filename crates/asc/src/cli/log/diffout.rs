//! The diff a commit shows under its message (`log -p`, `show`, `--stat`, `--name-status`…),
//! assembled from the engine's diff writers in git's order: raw and name listings, numstat, stat,
//! shortstat, summary, then the patch.

use asc_core::id::TreeId;
use asc_engine::diff::{
    self, DiffCtx, DiffOptions, NoAttributes, RenameOptions, write_numstat, write_patch,
    write_shortstat, write_stat,
};
use asc_engine::tree::Pathspec;

use super::opts::{DiffReq, Opts};
use super::revs::Selection;
use crate::cli::common::{CliError, Ctx, Result, WsCtx};

/// Config keys that tune the diff (`DiffOptions::apply_config`).
const KEYS: [&str; 11] = [
    "diff.algorithm",
    "diff.indentHeuristic",
    "diff.context",
    "diff.interHunkContext",
    "diff.noprefix",
    "diff.mnemonicPrefix",
    "diff.relative",
    "diff.renameLimit",
    "diff.renames",
    "core.bigFileThreshold",
    "core.abbrev",
];

/// How commits are diffed for one run.
pub struct DiffSetup {
    opts: DiffOptions,
    req: DiffReq,
    patch: bool,
    spec: Option<Pathspec>,
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

impl DiffSetup {
    /// `default_patch`: `show` prints the patch unless another format or `-s` is given.
    pub fn new(
        ctx: &Ctx,
        o: &Opts,
        sel: &Selection,
        default_patch: bool,
        abbrev: usize,
    ) -> Result<Self> {
        let mut opts = DiffOptions {
            renames: Some(RenameOptions::default()),
            abbrev,
            ..DiffOptions::default()
        };
        for key in KEYS {
            if let Some(v) = ctx.config.get(key) {
                opts.apply_config(key, &v).map_err(|e| {
                    fatal(format!(
                        "bad config variable '{}' value '{}'",
                        e.key, e.value
                    ))
                })?;
            }
        }
        opts.abbrev = abbrev;
        let req = o.diff.clone();
        if let Some(n) = req.unified {
            opts.context = n;
        }
        if let Some(a) = &req.algorithm {
            opts.algorithm = a.parse().map_err(|_| fatal("option diff-algorithm accepts \"myers\", \"minimal\", \"patience\" and \"histogram\""))?;
        }
        if req.no_renames {
            opts.renames = None;
        }
        opts.nul = o.nul;
        if let Some(args) = &req.stat_args {
            let mut it = args.split(',').map(|p| p.parse::<usize>().ok());
            opts.stat_width = it.next().flatten();
            opts.stat_name_width = it.next().flatten();
            opts.stat_count = it.next().flatten();
        }
        let patch = match req.patch {
            Some(p) => p,
            None => default_patch && !req.has_format(),
        };
        let spec = if sel.paths.is_empty() {
            None
        } else {
            Some(Pathspec::parse(sel.paths.iter().map(|p| p.as_bytes())).map_err(fatal)?)
        };
        Ok(Self {
            opts,
            req,
            patch,
            spec,
        })
    }

    /// Whether the run prints diffs at all.
    pub fn wants(&self) -> bool {
        self.patch || self.req.has_format()
    }

    /// Whether a diffstat and the patch are both printed (git then writes `---`).
    pub fn stat_and_patch(&self) -> bool {
        self.patch && self.req.stat
    }

    /// Whether the patch is among the formats.
    pub fn has_patch(&self) -> bool {
        self.patch
    }

    /// The diff from `old` to `new`; `None` when the trees do not differ in the pathspec.
    pub async fn render(&self, ws: &WsCtx, old: TreeId, new: TreeId) -> Result<Option<Vec<u8>>> {
        let changes = ws
            .engine
            .diff_trees(old, new, self.spec.as_ref(), &self.opts)
            .await
            .map_err(fatal)?;
        if changes.is_empty() {
            return Ok(None);
        }
        let ctx = DiffCtx {
            opts: &self.opts,
            content: &ws.engine,
            attrs: &NoAttributes,
        };
        let mut w: Vec<u8> = Vec::new();
        let mut separator = false;
        let r = &self.req;
        if r.raw {
            diff::format::write_raw(&mut w, &changes, &self.opts).map_err(fatal)?;
            separator = true;
        }
        if r.name_only {
            diff::format::write_name_only(&mut w, &changes, &self.opts).map_err(fatal)?;
            separator = true;
        }
        if r.name_status {
            diff::format::write_name_status(&mut w, &changes, &self.opts).map_err(fatal)?;
            separator = true;
        }
        if r.numstat {
            write_numstat(&mut w, &ctx, &changes).await.map_err(fatal)?;
            separator = true;
        }
        if r.stat {
            write_stat(&mut w, &ctx, &changes).await.map_err(fatal)?;
            separator = true;
        }
        if r.shortstat {
            write_shortstat(&mut w, &ctx, &changes)
                .await
                .map_err(fatal)?;
            separator = true;
        }
        if r.summary {
            diff::format::write_summary(&mut w, &changes, &self.opts).map_err(fatal)?;
            separator = true;
        }
        if self.patch {
            if separator {
                w.push(b'\n');
            }
            write_patch(&mut w, &ctx, &changes).await.map_err(fatal)?;
        }
        Ok(Some(w))
    }
}
