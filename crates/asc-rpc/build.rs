//! Compiles every `proto/asc/v1/*.proto` into tonic/prost code.
//!
//! `protox` parses the files in-process, so no `protoc` binary is needed on any host. The
//! `FileDescriptorSet` (with imports and comments) is written to `asc_descriptor.bin` for
//! `asc_rpc::DESCRIPTOR_SET`.
//!
//! Several git worktrees may share one `CARGO_TARGET_DIR`, so the compiled build script itself
//! can be reused from another worktree. The manifest directory is therefore read from the
//! environment on every run (`env!` would bake in the path of the worktree that built the
//! script), and `rerun-if-env-changed` makes cargo re-run the script when that path changes.

use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?,
    );
    let root = manifest_dir.join("../../proto");
    let dir = root.join("asc/v1");
    require_dir(&dir)?;
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| format!("cannot list proto directory {}: {e}", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "proto") {
            println!("cargo:rerun-if-changed={}", path.display());
            files.push(path);
        }
    }
    if files.is_empty() {
        return Err(format!("no .proto files found in {}", dir.display()).into());
    }
    files.sort();
    let mut compiler = protox::Compiler::new([&root])?;
    compiler
        .include_source_info(true)
        .include_imports(true)
        .open_files(&files)?;
    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    // Encode from the compiler, not from `prost_types`, which drops the `(asc.v1.json)` options.
    std::fs::write(
        out.join("asc_descriptor.bin"),
        compiler.encode_file_descriptor_set(),
    )?;
    let fds = compiler.file_descriptor_set();
    tonic_prost_build::configure().bytes(".").compile_fds(fds)?;
    Ok(())
}

/// Fails with a readable message when the proto directory is missing or not a directory.
fn require_dir(dir: &Path) -> Result<(), String> {
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(format!("proto path {} is not a directory", dir.display())),
        Err(e) => Err(format!(
            "proto directory {} is not readable ({e}); it is resolved from CARGO_MANIFEST_DIR, \
             so the checkout must contain proto/asc/v1",
            dir.display()
        )),
    }
}
