//! The asset table: every file of the built web UI with its precompressed variants, a content type
//! and a strong ETag. Built once at startup from the embedded build (`build.rs`) or from a
//! directory (`[web] dir`).

use std::collections::HashMap;
use std::path::{Component, Path};

use bytes::Bytes;
use sha2::{Digest, Sha256};

/// One file embedded by `build.rs`.
pub struct Embedded {
    pub path: &'static str,
    pub identity: &'static [u8],
    pub gzip: Option<&'static [u8]>,
    pub brotli: Option<&'static [u8]>,
}

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

/// The UI is mounted here; `web/src/base.ts` and `web/rsbuild.config.ts` carry the same value.
pub const BASE: &str = "/ui";

/// A file ready to serve.
#[derive(Clone, Debug)]
pub struct Asset {
    pub content_type: &'static str,
    /// Strong validator of the identity body, without quotes.
    pub etag: String,
    pub identity: Bytes,
    pub gzip: Option<Bytes>,
    pub brotli: Option<Bytes>,
    /// Content-hashed (`static/...`): cacheable for a year.
    pub immutable: bool,
}

pub type Assets = HashMap<String, Asset>;

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

pub fn content_type(path: &str) -> &'static str {
    match path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain; charset=utf-8",
        Some("webmanifest") => "application/manifest+json",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn etag(body: &[u8]) -> String {
    let d = Sha256::digest(body);
    d[..12].iter().fold(String::with_capacity(24), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    })
}

fn make(path: &str, identity: Bytes, gzip: Option<Bytes>, brotli: Option<Bytes>) -> Asset {
    Asset {
        content_type: content_type(path),
        etag: etag(&identity),
        identity,
        gzip,
        brotli,
        immutable: path.starts_with("static/"),
    }
}

/// The embedded build. Empty when `web/dist` did not exist at compile time.
pub fn embedded() -> Assets {
    EMBEDDED
        .iter()
        .map(|e| {
            let asset = make(
                e.path,
                Bytes::from_static(e.identity),
                e.gzip.map(Bytes::from_static),
                e.brotli.map(Bytes::from_static),
            );
            (e.path.to_owned(), asset)
        })
        .collect()
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue; // never follow a link out of the directory
        }
        if kind.is_dir() {
            walk(root, &path, out)?;
        } else if kind.is_file() {
            let rel = path.strip_prefix(root).map_err(std::io::Error::other)?;
            if rel.components().all(|c| matches!(c, Component::Normal(_))) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

/// Loads a build directory (the layout of `web/dist`). Fails when `index.html` is missing.
pub fn from_dir(dir: &Path) -> std::io::Result<Assets> {
    let mut names = Vec::new();
    walk(dir, dir, &mut names)?;
    let read = |name: &str| -> std::io::Result<Option<Bytes>> {
        let p = dir.join(name);
        match std::fs::metadata(&p) {
            Ok(m) if m.is_file() && m.len() <= MAX_FILE_BYTES => {
                Ok(Some(Bytes::from(std::fs::read(p)?)))
            }
            Ok(_) => Err(std::io::Error::other(format!(
                "{name} is not a file or is too large"
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    };
    let mut out = Assets::new();
    for name in &names {
        if name.ends_with(".gz") || name.ends_with(".br") || name == "manifest.json" {
            continue;
        }
        let Some(identity) = read(name)? else {
            continue;
        };
        let gzip = read(&format!("{name}.gz"))?;
        let brotli = read(&format!("{name}.br"))?;
        out.insert(name.clone(), make(name, identity, gzip, brotli));
    }
    if !out.contains_key("index.html") {
        return Err(std::io::Error::other("no index.html in the web directory"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types() {
        assert_eq!(
            content_type("static/js/a.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("favicon.SVG"), "image/svg+xml");
        assert_eq!(content_type("noext"), "application/octet-stream");
    }

    #[test]
    fn etag_is_stable_and_content_derived() {
        assert_eq!(etag(b"a"), etag(b"a"));
        assert_ne!(etag(b"a"), etag(b"b"));
        assert_eq!(etag(b"a").len(), 24);
    }
}
