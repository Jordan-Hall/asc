//! The npm packages the source files of a revision import.
//!
//! The manifests say what a project depends on; the sources say what it uses. This module reads the
//! module specifiers of JavaScript and TypeScript sources, TSX and TSRX included (`.tsrx` goes through
//! the TSRX layer of `asc-codeintel`, which blanks the template regions first, so an `import` inside a
//! template's text is not an import), and reduces them to package names: `react`, `@scope/pkg`. Relative
//! paths, `node:` built-ins, URLs and `#` subpath imports name no package. The scan is a bounded
//! heuristic over text, like the other facts of this module, and never runs code.

use std::collections::BTreeSet;

/// Extensions of the sources that are scanned.
pub const SOURCE_EXTS: [&str; 9] = ["ts", "tsx", "tsrx", "js", "jsx", "mjs", "cjs", "mts", "cts"];
/// Most source files one revision scan reads, and the largest one.
pub const MAX_SOURCES: usize = 5_000;
pub const MAX_SOURCE_BYTES: u64 = 256 * 1024;

/// Whether a file called `name` is a source this module scans.
pub fn is_source(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && SOURCE_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// The package a specifier names, if it names one.
pub fn package_of(spec: &str) -> Option<String> {
    if spec.is_empty() || spec.starts_with(['.', '/', '#']) || spec.contains(':') || spec.contains('\\') {
        return None;
    }
    let mut parts = spec.split('/');
    let first = parts.next()?;
    let name = if let Some(scope) = first.strip_prefix('@') {
        if scope.is_empty() {
            return None;
        }
        format!("{first}/{}", parts.next().filter(|s| !s.is_empty())?)
    } else {
        first.to_owned()
    };
    let ok = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@' | b'/' | b'~'));
    (ok && name.len() <= 214).then_some(name)
}

/// The distinct packages `bytes` (the source at `path`) imports, sorted.
pub fn package_imports(path: &str, bytes: &[u8]) -> Vec<String> {
    if !is_source(path) {
        return Vec::new();
    }
    let a = asc_codeintel::tsrx::analyze(bytes);
    let set: BTreeSet<String> = asc_codeintel::tsrx::import_specifiers(&a)
        .iter()
        .filter_map(|s| package_of(s))
        .collect();
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specifiers_reduce_to_package_names() {
        for (s, want) in [
            ("react", Some("react")),
            ("react/jsx-runtime", Some("react")),
            ("@tanstack/query-core/build", Some("@tanstack/query-core")),
            ("./local", None),
            ("../up", None),
            ("/abs", None),
            ("node:fs", None),
            ("https://cdn.example/x.js", None),
            ("#internal/x", None),
            ("@scope", None),
            ("", None),
        ] {
            assert_eq!(package_of(s).as_deref(), want, "{s}");
        }
    }

    #[test]
    fn tsrx_imports_count_like_typescript_and_template_text_does_not() {
        let src = b"import { signal } from '@tsrx/signals';\nimport Card from './Card.tsrx';\nimport 'normalize.css';\nexport function A() @{\n  <p>import x from 'not-a-package'</p>\n}\n";
        assert_eq!(package_imports("src/A.tsrx", src), ["@tsrx/signals", "normalize.css"]);
        let ts = b"import a from 'lodash';\nconst b = require('chalk');\nexport * from 'zod';\nconst c = import('left-pad');\n";
        assert_eq!(package_imports("x.ts", ts), ["chalk", "left-pad", "lodash", "zod"]);
        assert!(package_imports("README.md", ts).is_empty());
    }

    #[test]
    fn only_sources_are_scanned() {
        assert!(is_source("a.tsrx") && is_source("A.TSX") && is_source("x/y.mjs"));
        assert!(!is_source("package.json") && !is_source(".ts") && !is_source("a.rs"));
    }
}
