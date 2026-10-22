//! Compiles every `proto/asc/v1/*.proto` into tonic/prost code.
//!
//! `protox` parses the files in-process, so no `protoc` binary is needed on any host.

use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../proto");
    let dir = root.join("asc/v1");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "proto") {
            println!("cargo:rerun-if-changed={}", path.display());
            files.push(path);
        }
    }
    files.sort();
    let fds = protox::compile(&files, [&root])?;
    tonic_prost_build::configure().bytes(".").compile_fds(fds)?;
    Ok(())
}
