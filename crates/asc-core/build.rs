//! Embeds the registry packages of the workspace `Cargo.lock` for `asc_core::self_deps`.
//!
//! The script reads only the workspace lockfile (never the network, never another file) and
//! writes `$OUT_DIR/self_deps.txt`: the `[[package]]` stanzas whose `source` is a registry,
//! reduced to their `name`, `version` and `source` lines. A missing lockfile gives an empty
//! file, which the library reports as an empty package list.
//!
//! The manifest directory is read from the environment on every run (not `env!`) because several
//! worktrees may share one `CARGO_TARGET_DIR`; see `crates/asc-rpc/build.rs`.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?,
    );
    let lock = manifest_dir.join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let text = match std::fs::read_to_string(&lock) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", lock.display()).into()),
    };
    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    std::fs::write(out.join("self_deps.txt"), registry_stanzas(&text))?;
    Ok(())
}

/// Keeps `name`, `version` and `source` of each registry package, one stanza per package.
fn registry_stanzas(lock: &str) -> String {
    let mut out = String::new();
    let mut block: Vec<&str> = Vec::new();
    let flush = |block: &mut Vec<&str>, out: &mut String| {
        if block.iter().any(|l| l.starts_with("source = \"registry+")) {
            for l in block.iter() {
                out.push_str(l);
                out.push('\n');
            }
            out.push('\n');
        }
        block.clear();
    };
    for line in lock.lines() {
        if line.starts_with("[[") {
            flush(&mut block, &mut out);
        } else if line.starts_with("name = ")
            || line.starts_with("version = ")
            || line.starts_with("source = ")
        {
            block.push(line);
        }
    }
    flush(&mut block, &mut out);
    out
}
