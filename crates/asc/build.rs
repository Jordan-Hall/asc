//! Build identity for `asc --version`: the commit and the target triple.
//!
//! `ASC_BUILD_COMMIT` (set by the release workflow to the tagged commit) wins. Without it the
//! commit of the checkout is read with `git rev-parse`, and a build outside a checkout, or
//! without git, says `unknown`. The value is checked to be hex before it is embedded, so the
//! environment cannot put anything else into the version line.

use std::path::{Path, PathBuf};
use std::process::Command;

fn is_commit(text: &str) -> bool {
    (7..=64).contains(&text.len()) && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git") // lint:allow-raw-git: build identity only, no repository state is read or written
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ASC_BUILD_COMMIT");

    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let from_env = std::env::var("ASC_BUILD_COMMIT")
        .ok()
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| is_commit(c));
    let commit = from_env.or_else(|| {
        // HEAD and its reflog change with every commit and checkout, in a worktree too.
        if let Some(git_dir) = git(&dir, &["rev-parse", "--absolute-git-dir"]) {
            for f in ["HEAD", "logs/HEAD"] {
                let p = Path::new(&git_dir).join(f);
                if p.is_file() {
                    println!("cargo:rerun-if-changed={}", p.display());
                }
            }
        }
        git(&dir, &["rev-parse", "HEAD"]).filter(|c| is_commit(c))
    });
    let commit = commit.as_deref().map_or("unknown", short);
    println!("cargo:rustc-env=ASC_BUILD_COMMIT={commit}");

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=ASC_BUILD_TARGET={target}");
}
