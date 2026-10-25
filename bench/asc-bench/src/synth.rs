//! `bench/gen`: deterministic synthetic repository generator.
//!
//! The same `Config` always writes the same bytes: file paths, sizes and
//! contents come from a seeded xorshift generator, never from the clock or the
//! environment. `churn` rewrites a fixed fraction of the files for one commit.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Hard caps so a typo cannot fill a disk.
pub const MAX_FILES: u64 = 2_000_000;
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub files: u64,
    /// Directory nesting depth (>= 1).
    pub depth: u32,
    /// Fan-out per directory level (>= 1).
    pub fanout: u32,
    /// Median file size in bytes; sizes spread between 1/4 and 4x of it.
    pub median_bytes: u64,
    /// Files rewritten by each `churn` call.
    pub churn_files: u64,
    pub seed: u64,
}

impl Config {
    pub fn small() -> Self {
        Config {
            files: 500,
            depth: 3,
            fanout: 6,
            median_bytes: 2048,
            churn_files: 8,
            seed: 1,
        }
    }
    /// 10x scaled variant of `small` (SPEC 16.4 "10x synthetic repo").
    pub fn ten_x() -> Self {
        Config {
            files: 5000,
            depth: 4,
            fanout: 6,
            median_bytes: 2048,
            churn_files: 8,
            seed: 2,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.files == 0 || self.files > MAX_FILES {
            return Err(format!("files must be 1..={MAX_FILES}"));
        }
        if self.depth == 0 || self.depth > 16 || self.fanout == 0 || self.fanout > 1024 {
            return Err("depth must be 1..=16 and fanout 1..=1024".into());
        }
        if self.median_bytes == 0 || self.median_bytes.saturating_mul(4) > MAX_FILE_BYTES {
            return Err("median_bytes out of range".into());
        }
        Ok(())
    }
}

/// xorshift64*: small, fast and identical on every platform.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

/// Relative path of file `i` (always under `p<a>/p<b>/.../f<i>.txt`, never absolute or with `..`).
pub fn path_of(cfg: &Config, i: u64) -> PathBuf {
    let mut p = PathBuf::new();
    let mut h = i;
    for level in 0..cfg.depth {
        let slot = h % u64::from(cfg.fanout);
        h /= u64::from(cfg.fanout);
        p.push(format!("d{level}_{slot}"));
    }
    p.push(format!("f{i}.txt"));
    p
}

fn content(cfg: &Config, i: u64, version: u64) -> Vec<u8> {
    let mut rng =
        Rng::new(cfg.seed ^ i.wrapping_mul(0x1000_0000_01B3) ^ version.wrapping_mul(0xC2B2_AE3D));
    let q = cfg.median_bytes / 4;
    let size = (q + rng.below(cfg.median_bytes * 4 - q)) as usize;
    let mut out = Vec::with_capacity(size + 80);
    while out.len() < size {
        let line = format!(
            "{:016x} v{} file{} {:08x}\n",
            rng.next_u64(),
            version,
            i,
            rng.next_u64() as u32
        );
        out.extend_from_slice(line.as_bytes());
    }
    out.truncate(size.max(1));
    out
}

fn write_file(root: &Path, rel: &Path, bytes: &[u8]) -> io::Result<()> {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(full, bytes)
}

/// Write the initial tree (version 0) of `cfg` under `root`.
pub fn generate(cfg: &Config, root: &Path) -> io::Result<()> {
    cfg.validate()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    for i in 0..cfg.files {
        write_file(root, &path_of(cfg, i), &content(cfg, i, 0))?;
    }
    Ok(())
}

/// Rewrite the files that change in commit number `commit` (1-based); returns their paths.
pub fn churn(cfg: &Config, root: &Path, commit: u64) -> io::Result<Vec<PathBuf>> {
    let mut rng = Rng::new(cfg.seed ^ commit.wrapping_mul(0xD6E8_FEB8_6659_FD93));
    let mut touched = Vec::new();
    for _ in 0..cfg.churn_files.min(cfg.files) {
        let i = rng.below(cfg.files);
        let rel = path_of(cfg, i);
        write_file(root, &rel, &content(cfg, i, commit))?;
        touched.push(rel);
    }
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut v = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    v.push((
                        p.strip_prefix(root).unwrap().to_path_buf(),
                        fs::read(&p).unwrap(),
                    ));
                }
            }
        }
        v.sort();
        v
    }

    fn tiny() -> Config {
        Config {
            files: 40,
            depth: 2,
            fanout: 3,
            median_bytes: 256,
            churn_files: 3,
            seed: 9,
        }
    }

    #[test]
    fn same_config_same_bytes() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        generate(&tiny(), a.path()).unwrap();
        generate(&tiny(), b.path()).unwrap();
        let s = snapshot(a.path());
        assert_eq!(s.len(), 40);
        assert_eq!(s, snapshot(b.path()));
    }

    #[test]
    fn different_seed_differs() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        generate(&tiny(), a.path()).unwrap();
        generate(&Config { seed: 10, ..tiny() }, b.path()).unwrap();
        assert_ne!(snapshot(a.path()), snapshot(b.path()));
    }

    #[test]
    fn churn_is_deterministic_and_changes_content() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        generate(&tiny(), a.path()).unwrap();
        generate(&tiny(), b.path()).unwrap();
        let before = snapshot(a.path());
        let t1 = churn(&tiny(), a.path(), 1).unwrap();
        let t2 = churn(&tiny(), b.path(), 1).unwrap();
        assert_eq!(t1, t2);
        assert_eq!(snapshot(a.path()), snapshot(b.path()));
        assert_ne!(before, snapshot(a.path()));
    }

    #[test]
    fn paths_stay_relative_and_unique() {
        let cfg = tiny();
        let mut seen = std::collections::HashSet::new();
        for i in 0..cfg.files {
            let p = path_of(&cfg, i);
            assert!(p.is_relative());
            assert!(
                p.components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
            );
            assert!(seen.insert(p));
        }
    }

    #[test]
    fn validate_rejects_absurd_configs() {
        assert!(Config { files: 0, ..tiny() }.validate().is_err());
        assert!(
            Config {
                files: MAX_FILES + 1,
                ..tiny()
            }
            .validate()
            .is_err()
        );
        assert!(Config { depth: 0, ..tiny() }.validate().is_err());
        assert!(
            Config {
                median_bytes: u64::MAX,
                ..tiny()
            }
            .validate()
            .is_err()
        );
        assert!(tiny().validate().is_ok());
    }
}
