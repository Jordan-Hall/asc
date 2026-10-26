//! Output of a diff in git's order: raw or name lines, then the stat forms, then the summary,
//! then the patch, each form separated by a blank line as `diff_flush` does. Patches are
//! written one change at a time so memory stays bounded by the largest file.

use std::io::Write;

use asc_core::id::GitOid;
use asc_core::path::RepoPath;
use asc_engine::diff::format::{write_name_only, write_name_status, write_raw, write_summary};
use asc_engine::diff::{
    self, Attributes, ContentSource, DiffAttr, DiffCtx, DiffOptions, Status, TreeChange,
    write_numstat, write_patch, write_shortstat, write_stat, write_word_diff,
};
use async_trait::async_trait;
use bytes::Bytes;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, Value};

use super::binary;
use super::opts::Formats;
use super::work::is_local;
use crate::cli::common::{CliError, Result};
use crate::cli::json::JsonLines;

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

/// `-a` / `--text`: every file is text.
struct AllText;

#[async_trait]
impl Attributes for AllText {
    fn diff_attr(&self, _: &RepoPath) -> DiffAttr {
        DiffAttr::Text
    }

    async fn textconv(&self, _: &str, _: &RepoPath, _: &Bytes) -> diff::Result<Option<Bytes>> {
        Ok(None)
    }
}

/// What one rendering needs besides the changes.
pub struct Target<'a> {
    pub fmt: Formats,
    pub opts: &'a DiffOptions,
    pub src: &'a dyn ContentSource,
    pub attrs: &'a dyn Attributes,
    pub text: bool,
    pub binary: bool,
    /// Hex digits of ids in raw lines: git's plumbing prints them in full unless `--abbrev`.
    pub raw_abbrev: usize,
    /// zlib level of binary literals.
    pub zlib_level: i32,
}

/// The null id git prints for a work tree side that was not hashed in a raw line.
fn blank_work_ids(changes: &[TreeChange]) -> Vec<TreeChange> {
    changes
        .iter()
        .cloned()
        .map(|mut c| {
            if let Some(n) = c.new.as_mut().filter(|n| is_local(n)) {
                n.git = GitOid::from_bytes([0; 20]);
            }
            c
        })
        .collect()
}

/// A type change prints as a deletion and a creation; `--binary` needs them apart.
fn split_type_changes(changes: &[TreeChange]) -> Vec<TreeChange> {
    let mut out = Vec::with_capacity(changes.len());
    for c in changes {
        if c.status == Status::TypeChanged {
            out.push(TreeChange {
                new: None,
                status: Status::Deleted,
                ..c.clone()
            });
            out.push(TreeChange {
                old: None,
                status: Status::Added,
                ..c.clone()
            });
        } else {
            out.push(c.clone());
        }
    }
    out
}

fn unmerged_before<'p>(
    pending: &mut std::slice::Iter<'p, RepoPath>,
    limit: Option<&RepoPath>,
) -> Vec<&'p RepoPath> {
    let mut out = Vec::new();
    while let Some(p) = pending.as_slice().first() {
        if limit.is_some_and(|l| p >= l) {
            break;
        }
        out.push(p);
        pending.next();
    }
    out
}

/// Writes every selected form of `changes` to `w`.
pub async fn render<W: Write>(
    w: &mut W,
    t: &Target<'_>,
    changes: &[TreeChange],
    unmerged: &[RepoPath],
) -> Result<()> {
    let attrs: &dyn Attributes = if t.text { &AllText } else { t.attrs };
    let ctx = DiffCtx {
        opts: t.opts,
        content: t.src,
        attrs,
    };
    let fmt = t.fmt;
    let mut separate = false;
    if fmt.raw || fmt.name_only || fmt.name_status {
        if fmt.raw {
            write_raw(
                w,
                &blank_work_ids(changes),
                &DiffOptions {
                    abbrev: t.raw_abbrev,
                    ..t.opts.clone()
                },
            )
            .map_err(fatal)?;
            for p in unmerged {
                w.write_all(b":000000 000000 0000000 0000000 U\t")?;
                w.write_all(p.as_bytes())?;
                w.write_all(if t.opts.nul { b"\0" } else { b"\n" })?;
            }
        }
        if fmt.name_only {
            write_name_only(w, changes, t.opts).map_err(fatal)?;
        }
        if fmt.name_status {
            write_name_status(w, changes, t.opts).map_err(fatal)?;
        }
        separate = true;
    }
    if fmt.numstat {
        write_numstat(w, &ctx, changes).await.map_err(fatal)?;
        separate = true;
    }
    if fmt.stat {
        write_stat(w, &ctx, changes).await.map_err(fatal)?;
        separate = true;
    }
    if fmt.shortstat {
        write_shortstat(w, &ctx, changes).await.map_err(fatal)?;
        separate = true;
    }
    if fmt.summary {
        let mut text = Vec::new();
        write_summary(&mut text, changes, t.opts).map_err(fatal)?;
        if !text.is_empty() {
            w.write_all(&text)?;
            separate = true;
        }
    }
    if fmt.patch || fmt.word_diff {
        if separate && (!changes.is_empty() || !unmerged.is_empty()) {
            w.write_all(b"\n")?;
        }
        patch(w, &ctx, t, changes, unmerged).await?;
    }
    Ok(())
}

async fn patch<W: Write>(
    w: &mut W,
    ctx: &DiffCtx<'_>,
    t: &Target<'_>,
    changes: &[TreeChange],
    unmerged: &[RepoPath],
) -> Result<()> {
    let owned;
    let list: &[TreeChange] = if t.binary {
        owned = split_type_changes(changes);
        &owned
    } else {
        changes
    };
    let mut pending = unmerged.iter();
    let mut buf = Vec::new();
    for c in list {
        for p in unmerged_before(&mut pending, Some(&c.path)) {
            w.write_all(b"* Unmerged path ")?;
            w.write_all(p.as_bytes())?;
            w.write_all(b"\n")?;
        }
        buf.clear();
        let one = std::slice::from_ref(c);
        if t.fmt.word_diff {
            write_word_diff(&mut buf, ctx, one).await.map_err(fatal)?;
        } else {
            write_patch(&mut buf, ctx, one).await.map_err(fatal)?;
        }
        if t.binary {
            binary::upgrade(&mut buf, t.src, c, t.zlib_level)
                .await
                .map_err(fatal)?;
        }
        w.write_all(&buf)?;
    }
    for p in unmerged_before(&mut pending, None) {
        w.write_all(b"* Unmerged path ")?;
        w.write_all(p.as_bytes())?;
        w.write_all(b"\n")?;
    }
    Ok(())
}

// ------------------------------------------------------------------------ json

/// The `DiffResponse` and `PathChange` descriptors.
pub struct JsonShape {
    response: MessageDescriptor,
    change: MessageDescriptor,
}

impl JsonShape {
    pub fn load() -> Result<Self> {
        let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).map_err(fatal)?;
        let get = |name: &str| {
            pool.get_message_by_name(name)
                .ok_or_else(|| fatal(format!("asc: missing message {name}")))
        };
        Ok(Self {
            response: get("asc.v1.DiffResponse")?,
            change: get("asc.v1.PathChange")?,
        })
    }

    fn path_change(&self, c: &TreeChange) -> DynamicMessage {
        let bytes = |v: &[u8]| Value::Bytes(v.to_vec().into());
        let mut m = DynamicMessage::new(self.change.clone());
        m.set_field_by_name("path", bytes(c.path.as_bytes()));
        m.set_field_by_name("status", Value::String(c.status.letter().to_string()));
        if c.status.score().is_some() {
            m.set_field_by_name("old_path", bytes(c.old_path.as_bytes()));
        }
        m.set_field_by_name("old_mode", Value::U32(c.old_mode()));
        m.set_field_by_name("new_mode", Value::U32(c.new_mode()));
        for (side, entry, id, size) in [
            ("old", &c.old, "old_id", "old_size"),
            ("new", &c.new, "new_id", "new_size"),
        ] {
            let Some(e) = entry else { continue };
            m.set_field_by_name(&format!("{side}_git"), bytes(e.git.as_bytes()));
            if !is_local(e) {
                m.set_field_by_name(id, bytes(&e.id));
            }
            m.set_field_by_name(size, Value::U64(e.size));
        }
        if let Some(score) = c.status.score() {
            m.set_field_by_name("similarity", Value::U32(u32::from(score)));
        }
        m
    }

    /// One `DiffResponse` line per changed path: its `PathChange` and the selected text form.
    pub async fn emit<W: Write>(
        &self,
        out: &mut W,
        t: &Target<'_>,
        changes: &[TreeChange],
        unmerged: &[RepoPath],
    ) -> Result<()> {
        let mut lines = JsonLines::new(out);
        for c in changes {
            let mut text = Vec::new();
            render(&mut text, t, std::slice::from_ref(c), &[]).await?;
            let mut m = DynamicMessage::new(self.response.clone());
            m.set_field_by_name(
                "changes",
                Value::List(vec![Value::Message(self.path_change(c))]),
            );
            m.set_field_by_name("text", Value::Bytes(text.into()));
            lines.emit(&m).map_err(fatal)?;
        }
        for p in unmerged {
            let mut change = DynamicMessage::new(self.change.clone());
            change.set_field_by_name("path", Value::Bytes(p.as_bytes().to_vec().into()));
            change.set_field_by_name("status", Value::String("U".into()));
            let mut m = DynamicMessage::new(self.response.clone());
            m.set_field_by_name("changes", Value::List(vec![Value::Message(change)]));
            lines.emit(&m).map_err(fatal)?;
        }
        lines.finish()?;
        Ok(())
    }
}
