//! What the ref plumbing commands share: git's option cursor (`--name=value`, `--name value`,
//! options with an optional value that default to `HEAD`) and the resolution of an object name to
//! something a ref can hold.

use std::ffi::OsString;

use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, GitOid, RefTarget, TagId};
use asc_core::path::RefName;
use asc_engine::revspec::dwim_candidates;
use asc_state::state::git_map;
use asc_store::refs::Refs as _;

use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::revparse::commit_of;

/// The arguments of one command, consumed left to right.
pub struct Cursor {
    items: Vec<String>,
    at: usize,
    usage: &'static str,
}

/// One word of the command line.
pub enum Word {
    /// `--` was seen: everything after it is an operand.
    End,
    /// `--name` or `--name=value`.
    Long(String, Option<String>),
    /// `-x` (or a cluster; commands that take clusters split it themselves).
    Short(String),
    /// An operand.
    Operand(String),
}

impl Cursor {
    pub fn new(args: &[OsString], usage: &'static str) -> Self {
        Self { items: args.iter().map(|a| a.to_string_lossy().into_owned()).collect(), at: 0, usage }
    }

    /// The usage error git prints for a bad option.
    pub fn usage_error(&self, message: &str) -> CliError {
        CliError::Usage(format!("error: {message}\n{}", self.usage))
    }

    /// The bare usage text (exit 129).
    pub fn usage_only(&self) -> CliError {
        CliError::Usage(self.usage.to_owned())
    }

    /// The next word, classified; `None` at the end.
    pub fn next(&mut self) -> Option<Word> {
        let item = self.items.get(self.at)?.clone();
        self.at += 1;
        Some(if item == "--" {
            Word::End
        } else if let Some(long) = item.strip_prefix("--") {
            match long.split_once('=') {
                Some((name, value)) => Word::Long(name.to_owned(), Some(value.to_owned())),
                None => Word::Long(long.to_owned(), None),
            }
        } else if item.len() > 1 && item.starts_with('-') {
            Word::Short(item[1..].to_owned())
        } else {
            Word::Operand(item)
        })
    }

    /// Everything not yet read (after `--`).
    pub fn rest(&mut self) -> Vec<String> {
        let rest = self.items[self.at..].to_vec();
        self.at = self.items.len();
        rest
    }

    /// How many words are left.
    pub fn remaining(&self) -> usize {
        self.items.len() - self.at
    }

    /// The value of an option that needs one: the inline `=value` or the next word.
    pub fn value(&mut self, name: &str, inline: Option<String>) -> Result<String> {
        if let Some(v) = inline {
            return Ok(v);
        }
        match self.items.get(self.at) {
            Some(v) => {
                self.at += 1;
                Ok(v.clone())
            }
            None => Err(CliError::Raw(format!("error: option `{name}' requires a value"), 129)),
        }
    }

    /// The value of an option that may be last (`--merged`): the inline value, else the next word
    /// unless it is the last word or `--`, else `default`.
    pub fn value_or(&mut self, inline: Option<String>, default: &str) -> String {
        if let Some(v) = inline {
            return v;
        }
        match self.items.get(self.at) {
            Some(v) if v != "--" => {
                self.at += 1;
                v.clone()
            }
            _ => default.to_owned(),
        }
    }
}

/// `word` as a full git object id.
pub fn parse_oid(word: &str) -> Option<GitOid> {
    (word.len() == 40).then(|| word.parse().ok()).flatten()
}

/// What `word` names as a ref target: an annotated tag (by name or id) or a commit.
pub async fn resolve_target(ctx: &Ctx, ws: &WsCtx, word: &str) -> Result<Option<RefTarget>> {
    if let Some(oid) = parse_oid(word) {
        if let Some(asc) = git_map::git_to_asc(&ws.db, ws.repo.key, oid, ObjectKind::Tag).await? {
            return Ok(Some(RefTarget::Tag(TagId::from_bytes(asc))));
        }
        if let Some(asc) = git_map::git_to_asc(&ws.db, ws.repo.key, oid, ObjectKind::Commit).await? {
            return Ok(Some(RefTarget::Changeset(ChangesetId::from_bytes(asc))));
        }
        return Ok(None);
    }
    if !word.contains(['~', '^', '@', ':']) {
        for cand in dwim_candidates(word) {
            let Ok(name) = RefName::new(cand.into_bytes()) else { continue };
            if let Some(entry) = ws.db.refs().get(ws.repo.key, &name).await? {
                return Ok(Some(entry.target));
            }
        }
    }
    Ok(commit_of(ctx, word).await?.map(RefTarget::Changeset))
}

/// The git id of what `word` names (a commit or an annotated tag), for `--points-at`.
pub async fn resolve_oid(ctx: &Ctx, ws: &WsCtx, word: &str) -> Result<Option<GitOid>> {
    let Some(target) = resolve_target(ctx, ws, word).await? else {
        return Ok(None);
    };
    object_oid_of(ws, target).await.map(Some)
}

/// The git id of a ref target's object.
pub async fn object_oid_of(ws: &WsCtx, target: RefTarget) -> Result<GitOid> {
    match target {
        RefTarget::Changeset(c) => ws.engine.git_commit_of(c).await.map_err(|e| CliError::Fatal(e.to_string())),
        RefTarget::Tag(t) => {
            let rec = crate::cli::log::refs::tag_record(ws, t).await?;
            let target_git =
                ws.engine.git_commit_of(rec.target).await.map_err(|e| CliError::Fatal(e.to_string()))?;
            let raw = asc_core::git::export_tag(&rec, target_git).map_err(|e| CliError::Fatal(e.to_string()))?;
            Ok(asc_core::git::object_oid(ObjectKind::Tag, &raw))
        }
    }
}
