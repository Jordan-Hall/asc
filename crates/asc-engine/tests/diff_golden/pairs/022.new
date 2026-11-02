//! Language of a path by extension or shebang (SPEC §23.2.1, `ci_doc_langs`).

/// Languages with a project grammar (SPEC §23.6). Other files have no language row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lang {
    Rust,
    TypeScript,
    Tsx,
    /// TSRX (<https://tsrx.dev/>): TypeScript with a template syntax, see [`crate::tsrx`].
    Tsrx,
    JavaScript,
    Python,
    CSharp,
    Go,
}

impl Lang {
    /// Stable name stored in `ci_doc_langs` and shown in filters.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::Tsrx => "tsrx",
            Lang::JavaScript => "javascript",
            Lang::Python => "python",
            Lang::CSharp => "csharp",
            Lang::Go => "go",
        }
    }

    /// Inverse of [`Lang::name`].
    pub fn from_name(s: &str) -> Option<Lang> {
        [
            Lang::Rust,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::Tsrx,
            Lang::JavaScript,
            Lang::Python,
            Lang::CSharp,
            Lang::Go,
        ]
        .into_iter()
        .find(|l| l.name() == s)
    }

    fn from_ext(ext: &str) -> Option<Lang> {
        Some(match ext {
            "rs" => Lang::Rust,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "tsrx" => Lang::Tsrx,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "py" | "pyi" | "pyw" => Lang::Python,
            "cs" => Lang::CSharp,
            "go" => Lang::Go,
            _ => return None,
        })
    }

    fn from_interpreter(name: &str) -> Option<Lang> {
        let n = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
        match n {
            "python" => Some(Lang::Python),
            "node" | "nodejs" => Some(Lang::JavaScript),
            "ts-node" | "tsx" | "deno" | "bun" => Some(Lang::TypeScript),
            _ => None,
        }
    }
}

/// Longest first line examined for a shebang.
const SHEBANG_MAX: usize = 256;

/// Language from the extension (case-insensitive) of the last path segment, else from a
/// shebang in `head` (the first bytes of the file; only the first 256 are read).
pub fn detect(path: &str, head: &[u8]) -> Option<Lang> {
    let base = path.rsplit('/').next().unwrap_or(path);
    if let Some((stem, ext)) = base.rsplit_once('.')
        && !stem.is_empty()
        && let Some(l) = Lang::from_ext(&ext.to_ascii_lowercase())
    {
        return Some(l);
    }
    shebang(head)
}

fn shebang(head: &[u8]) -> Option<Lang> {
    let head = &head[..head.len().min(SHEBANG_MAX)];
    let rest = head.strip_prefix(b"#!")?;
    let line = rest.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut words = line.split_ascii_whitespace();
    let mut prog = words.next()?.rsplit('/').next()?;
    if prog == "env" {
        // `env [-S] [-i] [VAR=x] prog`
        prog = words.find(|w| !w.starts_with('-') && !w.contains('='))?;
        prog = prog.rsplit('/').next()?;
    }
    Lang::from_interpreter(prog)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_and_shebang() {
        assert_eq!(detect("a/b.RS", b""), Some(Lang::Rust));
        assert_eq!(detect("x.tsx", b""), Some(Lang::Tsx));
        assert_eq!(
            detect("tool", b"#!/usr/bin/env -S python3 -u\n"),
            Some(Lang::Python)
        );
        assert_eq!(detect("tool", b"#!/usr/bin/node\n"), Some(Lang::JavaScript));
        assert_eq!(detect("tool", b"#!/bin/sh\n"), None);
        assert_eq!(detect(".rs", b""), None);
        assert_eq!(Lang::from_name("csharp"), Some(Lang::CSharp));
    }
}
