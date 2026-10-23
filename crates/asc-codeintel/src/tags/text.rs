//! Names, signatures and doc comments from a node. Everything is length-bounded.

use tree_sitter::Node;

use super::{MAX_DOC, MAX_NAME, MAX_SIG};
use crate::lang::Lang;

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut n = max;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
    s
}

/// A tag name: lossy UTF-8, trimmed, single line, empty when unusable.
pub fn name(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    if s.contains(['\n', '\r']) {
        return String::new();
    }
    truncate(s.to_string(), MAX_NAME)
}

fn collapse(bytes: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    truncate(s.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

fn slice(src: &[u8], start: usize, end: usize) -> &[u8] {
    let end = end.min(src.len());
    &src[start.min(end)..end]
}

/// The definition's text up to its body, whitespace collapsed.
pub fn sig(node: Node, src: &[u8]) -> Vec<u8> {
    let body = node.child_by_field_name("body").or_else(|| {
        node.child_by_field_name("value")
            .and_then(|v| v.child_by_field_name("body"))
    });
    let end = body
        .map_or(node.end_byte(), |b| b.start_byte())
        .max(node.start_byte());
    let end = end.min(node.start_byte() + 4 * MAX_SIG);
    collapse(slice(src, node.start_byte(), end), MAX_SIG).into_bytes()
}

fn strip_marker(line: &str) -> &str {
    let l = line.trim();
    for p in ["///", "//!", "//", "/**", "/*", "*/", "*", "#"] {
        if let Some(r) = l.strip_prefix(p) {
            return r.trim_end_matches("*/").trim();
        }
    }
    l.trim_end_matches("*/").trim()
}

fn python_docstring(node: Node, src: &[u8]) -> Option<String> {
    let first = node.child_by_field_name("body")?.named_child(0)?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let s = first.named_child(0)?;
    if s.kind() != "string" {
        return None;
    }
    let t = String::from_utf8_lossy(slice(src, s.start_byte(), s.end_byte()));
    let t = t.trim_matches(|c| c == '"' || c == '\'');
    Some(truncate(
        t.split_whitespace().collect::<Vec<_>>().join(" "),
        MAX_DOC,
    ))
}

/// Comments directly above the definition (attributes and decorators may sit between), or a
/// Python docstring.
pub fn doc(lang: Lang, node: Node, src: &[u8]) -> Vec<u8> {
    let mut anchor = node;
    while let Some(p) = anchor.parent()
        && matches!(
            p.kind(),
            "export_statement"
                | "decorated_definition"
                | "type_declaration"
                | "lexical_declaration"
                | "variable_declaration"
                | "const_declaration"
                | "var_declaration"
        )
    {
        anchor = p;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut comments = 0;
    let mut next_row = anchor.start_position().row;
    let mut cur = anchor.prev_sibling();
    let mut budget = MAX_DOC;
    while let Some(s) = cur {
        let k = s.kind();
        if k.contains("comment") {
            if s.end_position().row + 1 < next_row || comments >= 64 {
                break;
            }
            comments += 1;
            let text = String::from_utf8_lossy(slice(src, s.start_byte(), s.end_byte()));
            for line in text.lines().rev() {
                let l = strip_marker(line);
                if !l.is_empty() {
                    budget = budget.saturating_sub(l.len() + 1);
                    parts.push(l.to_string());
                }
            }
            if budget == 0 {
                break;
            }
            next_row = s.start_position().row;
        } else if matches!(k, "attribute_item" | "decorator" | "inner_attribute_item") {
            next_row = s.start_position().row;
        } else {
            break;
        }
        cur = s.prev_sibling();
    }
    if parts.is_empty() {
        if lang == Lang::Python && matches!(node.kind(), "function_definition" | "class_definition")
        {
            return python_docstring(node, src).unwrap_or_default().into_bytes();
        }
        return Vec::new();
    }
    parts.reverse();
    truncate(parts.join("\n"), MAX_DOC).into_bytes()
}
