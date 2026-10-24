//! Embeds the built web UI (`web/dist`, docs/WEB-UI.md section 8).
//!
//! Writes `$OUT_DIR/web_assets.rs`: a table of `(path, identity, gzip, brotli)` byte slices made
//! with `include_bytes!`. With no `web/dist/index.html` (a build without bun) the table is empty and
//! the crate still builds; `[web] enabled = true` then needs `[web] dir`. Override the directory
//! with `ASC_WEB_DIST`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = read.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn literal(p: &Path) -> String {
    format!("{:?}", p.to_string_lossy().replace('\\', "/"))
}

fn main() {
    println!("cargo:rerun-if-env-changed=ASC_WEB_DIST");
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let dist = env::var_os("ASC_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../web/dist"));
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut src = String::from("pub static EMBEDDED: &[Embedded] = &[\n");
    if dist.join("index.html").is_file() {
        let dist = dist.canonicalize().unwrap_or(dist);
        let mut files = Vec::new();
        walk(&dist, &mut files);
        for abs in &files {
            let rel = abs.strip_prefix(&dist).expect("under dist");
            let name = rel.to_string_lossy().replace('\\', "/");
            if name.ends_with(".br") || name.ends_with(".gz") || name == "manifest.json" {
                continue;
            }
            println!("cargo:rerun-if-changed={}", abs.display());
            let sibling = |ext: &str| {
                let mut s = abs.as_os_str().to_owned();
                s.push(ext);
                let p = PathBuf::from(s);
                p.is_file().then(|| {
                    println!("cargo:rerun-if-changed={}", p.display());
                    format!("Some(include_bytes!({}))", literal(&p))
                })
            };
            let gz = sibling(".gz").unwrap_or_else(|| "None".into());
            let br = sibling(".br").unwrap_or_else(|| "None".into());
            let _ = writeln!(
                src,
                "    Embedded {{ path: {name:?}, identity: include_bytes!({}), gzip: {gz}, brotli: {br} }},",
                literal(abs)
            );
        }
    }
    src.push_str("];\n");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("web_assets.rs");
    fs::write(out, src).expect("write web_assets.rs");
}
