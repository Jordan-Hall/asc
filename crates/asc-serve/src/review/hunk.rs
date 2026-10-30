//! Syntax-aware context for the header of a review hunk (`Review.HunkContext`).
//!
//! A diff hunk header names the function a hunk sits in. Git's rule (a line that starts with a
//! letter) is right for C and wrong for almost everything else, and `asc diff` keeps it so its output
//! stays byte-identical to git's. The review page is free of that: this call gives, for each line a
//! hunk starts at, the chain of definitions around it, from the tags of the file (functions,
//! classes, methods, modules; for TSRX also the component and the `@for`/`@if`/`@switch`/`@try`
//! scopes around the line, see `asc_codeintel::tsrx`).

use asc_rpc::server::{CallCtx, wire};
use tonic::Status;

use super::{Reviews, change_of, sql, store};
use crate::agents::repo::RepoView;
use crate::changes::versions;

/// Lines one call answers, and the largest file it reads.
pub const MAX_LINES: usize = 200;
const MAX_FILE: u64 = 2 * 1024 * 1024;
const MAX_CHAIN: usize = 8;

impl Reviews {
    /// `Review.HunkContext`.
    pub async fn hunk_context(
        &self,
        ctx: &CallCtx,
        req: wire::HunkContextRequest,
    ) -> Result<wire::HunkContextResponse, Status> {
        use asc_codeintel::tags::{self, Role};
        let change = change_of(&req.change)?;
        if req.lines.len() > MAX_LINES
            || req.path.is_empty()
            || req.path.len() > super::MAX_PATH
            || req.path.contains(&0)
        {
            return Err(Status::invalid_argument("a path and at most 200 lines"));
        }
        let path = std::str::from_utf8(&req.path)
            .map_err(|_| Status::invalid_argument("a path is UTF-8"))?;
        let entry = self.open(ctx).await?;
        let cs = {
            let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
            if !store::change_known(&mut conn, entry.ns, change).await? {
                return Err(Status::not_found("unknown change"));
            }
            let v = if req.version == 0 {
                versions::latest(&mut conn, entry.ns, change)
                    .await
                    .map_err(|e| super::internal("versions", &e))?
            } else {
                versions::list(&mut conn, entry.ns, change, Some(req.version - 1), 1)
                    .await
                    .map_err(|e| super::internal("versions", &e))?
                    .into_iter()
                    .find(|v| v.v == req.version)
            };
            v.ok_or_else(|| Status::invalid_argument("unknown version"))?
                .cs
        };
        let view = RepoView::new(self.registry.clone());
        let root = view.root_of(&entry, cs).await?;
        let Some(bytes) = view.read(&entry, root, path, MAX_FILE).await? else {
            return Err(Status::not_found("the file is not in this version"));
        };
        let head = &bytes[..bytes.len().min(256)];
        let lang = asc_codeintel::lang::detect(path, head);
        let defs: Vec<tags::Tag> = match lang {
            Some(l) => tags::extract(l, &bytes)
                .unwrap_or_default()
                .into_iter()
                .filter(|t| t.role == Role::Def)
                .collect(),
            None => Vec::new(),
        };
        let lines = req
            .lines
            .iter()
            .map(|&line| {
                let row = line.saturating_sub(1);
                // Definitions whose rows hold the line, outermost first (the tags are in source order).
                let mut chain: Vec<&tags::Tag> = defs
                    .iter()
                    .filter(|t| t.range.start_row <= row && row <= t.range.end_row)
                    .filter(|t| {
                        !matches!(
                            t.kind,
                            tags::Kind::Variable
                                | tags::Kind::Property
                                | tags::Kind::Field
                                | tags::Kind::Constant
                        )
                    })
                    .collect();
                chain.sort_by_key(|t| (t.range.start_byte, std::cmp::Reverse(t.range.end_byte)));
                if chain.len() > MAX_CHAIN {
                    chain.drain(..chain.len() - MAX_CHAIN);
                }
                wire::HunkContextLine {
                    line,
                    chain: chain
                        .iter()
                        .map(|t| wire::HunkContextEntry {
                            name: t.name.clone(),
                            kind: t.kind.name().to_owned(),
                        })
                        .collect(),
                }
            })
            .collect();
        Ok(wire::HunkContextResponse {
            language: lang.map(|l| l.name().to_owned()).unwrap_or_default(),
            lines,
        })
    }
}
