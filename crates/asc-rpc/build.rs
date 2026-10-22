//! Compiles every `proto/asc/v1/*.proto` into tonic/prost code.
//!
//! `protox` parses the files in-process, so no `protoc` binary is needed on any host. The
//! `FileDescriptorSet` (with imports and comments) is written to `asc_descriptor.bin` for
//! `asc_rpc::DESCRIPTOR_SET`.

use std::path::{Path, PathBuf};

use prost::Message;

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
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("asc_descriptor.bin"), fds.encode_to_vec())?;
    tonic_prost_build::configure().bytes(".").compile_fds(fds)?;
    Ok(())
}
