//! The tags only the TSRX layer sees (see [`crate::tsrx`]): components, control-flow scopes, the
//! names a template defines and the names it uses.

use super::{Kind, MAX_NAME, MAX_TAGS, NO_CONTAINER, Range, Role, Tag, assign_containers, sort_source_order};
use crate::tsrx::{Analysis, ItemKind, Span};

/// Row of every byte offset: `rows[i]` = zero-based line of byte `i` (`len + 1` entries).
fn rows_of(src: &[u8]) -> Vec<u32> {
    let mut rows = Vec::with_capacity(src.len() + 1);
    let mut r = 0u32;
    for &b in src {
        rows.push(r);
        if b == b'\n' {
            r += 1;
        }
    }
    rows.push(r);
    rows
}

fn range(rows: &[u32], s: Span) -> Range {
    let last = rows.len() - 1;
    let (a, b) = (s.start.min(last), s.end.min(last));
    Range {
        start_byte: a as u32,
        end_byte: b as u32,
        start_row: rows[a],
        end_row: rows[b],
    }
}

fn cut(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > MAX_NAME {
            break;
        }
        out.push(c);
    }
    out
}

/// Adds the TSRX tags of `a` (analysis of `src`) to `tags` and restores source order and containers.
pub(super) fn add(tags: &mut Vec<Tag>, a: &Analysis, src: &[u8]) {
    let rows = rows_of(src);
    // A function whose body is a `@{` container is a component.
    for c in &a.containers {
        let best = tags
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.role == Role::Def
                    && t.kind == Kind::Function
                    && (t.range.start_byte as usize) <= c.start
                    && c.end <= (t.range.end_byte as usize)
            })
            .min_by_key(|(_, t)| t.range.end_byte - t.range.start_byte)
            .map(|(i, _)| i);
        if let Some(i) = best {
            tags[i].kind = Kind::Component;
        }
    }
    let room = MAX_TAGS.saturating_sub(tags.len());
    let mut extra: Vec<Tag> = Vec::new();
    for s in &a.scopes {
        let kw = Span {
            start: s.span.start,
            end: (s.span.start + 1 + s.kind.keyword().len()).min(s.span.end),
        };
        extra.push(Tag {
            name: cut(&s.label),
            kind: Kind::Scope,
            role: Role::Def,
            range: range(&rows, s.span),
            name_range: range(&rows, kw),
            container: NO_CONTAINER,
            sig: s.label.clone().into_bytes(),
            doc: Vec::new(),
        });
    }
    for d in &a.defs {
        let (kind, sig) = match d.kind {
            ItemKind::StyleClass => (Kind::Property, format!(".{}", d.name).into_bytes()),
            _ => (Kind::Variable, d.name.clone().into_bytes()),
        };
        extra.push(Tag {
            name: cut(&d.name),
            kind,
            role: Role::Def,
            range: range(&rows, d.span),
            name_range: range(&rows, d.span),
            container: NO_CONTAINER,
            sig,
            doc: Vec::new(),
        });
    }
    for u in &a.uses {
        let kind = match u.kind {
            ItemKind::ComponentUse => Kind::Call,
            _ => Kind::Variable,
        };
        extra.push(Tag {
            name: cut(&u.name),
            kind,
            role: Role::Ref,
            range: range(&rows, u.span),
            name_range: range(&rows, u.span),
            container: NO_CONTAINER,
            sig: Vec::new(),
            doc: Vec::new(),
        });
    }
    extra.truncate(room);
    tags.extend(extra);
    // One tag per (name range, role); a reference at a definition's name is that definition.
    tags.sort_by_key(|t| (t.name_range.start_byte, t.name_range.end_byte, t.role));
    tags.dedup_by(|b, a| a.name_range == b.name_range && a.role == b.role);
    let defs: std::collections::HashSet<(u32, u32)> = tags
        .iter()
        .filter(|t| t.role == Role::Def)
        .map(|t| (t.name_range.start_byte, t.name_range.end_byte))
        .collect();
    tags.retain(|t| t.role == Role::Def || !defs.contains(&(t.name_range.start_byte, t.name_range.end_byte)));
    sort_source_order(tags);
    assign_containers(tags);
}
