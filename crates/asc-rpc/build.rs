//! Compiles every `proto/asc/v1/*.proto` into tonic/prost code.
//!
//! `protox` parses the files in-process, so no `protoc` binary is needed on any host. The
//! `FileDescriptorSet` (with imports and comments) is written to `asc_descriptor.bin` for
//! `asc_rpc::DESCRIPTOR_SET`.

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
    let mut compiler = protox::Compiler::new([&root])?;
    compiler.include_source_info(true).include_imports(true).open_files(&files)?;
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    // Encode from the compiler, not from `prost_types`, which drops the `(asc.v1.json)` options.
    std::fs::write(out.join("asc_descriptor.bin"), compiler.encode_file_descriptor_set())?;
    let fds = compiler.file_descriptor_set();
    tonic_prost_build::configure().bytes(".").compile_fds(fds)?;
    Ok(())
}
