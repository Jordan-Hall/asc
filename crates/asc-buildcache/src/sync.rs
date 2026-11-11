//! Incremental restore (docs/designs/build-cache.md §2): an entry's outputs are brought into a
//! tree that already holds an earlier build, so only the files whose bytes differ are written and
//! only the files the entry no longer has are removed. Unchanged files keep their modification
//! time, so a watcher, a dev server or the next incremental step sees just the changed file.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

use asc_core::id::FileHasher;

use crate::store::{Action, Output, OutputKind, Store, executable};
use crate::unhex;

/// What [`Store::sync`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Synced {
    /// Outputs written because they were absent or differed.
    pub written: usize,
    /// Outputs already in place with the same bytes, left untouched.
    pub kept: usize,
    /// Files and links below the output roots the entry does not have, removed.
    pub removed: usize,
}

impl Store {
    /// Brings `action`'s outputs into `base`, writing only what differs. `roots` are the output
    /// paths the build declared (relative to `base`): anything below them the entry does not
    /// name is removed, so no stale file stays beside the restored ones. `Ok(None)` when a blob
    /// is gone (evicted meanwhile): the caller runs the tool as on a miss, and nothing was
    /// removed.
    pub fn sync(
        &self,
        action: &Action,
        base: &Path,
        roots: &[String],
    ) -> io::Result<Option<Synced>> {
        if !self.restorable(action)? {
            return Ok(None);
        }
        let named: BTreeSet<&str> = action.outputs.iter().map(|o| o.path.as_str()).collect();
        let mut done = Synced::default();
        for root in roots {
            check_root(root)?;
            done.removed += prune(base, root, &named)?;
        }
        for o in &action.outputs {
            if self.in_place(o, &base.join(&o.path))? {
                done.kept += 1;
                continue;
            }
            if !self.restore_one(o, base)? {
                return Ok(None);
            }
            done.written += 1;
        }
        Ok(Some(done))
    }

    /// Whether `dest` already is output `o`: the same kind, size and content.
    fn in_place(&self, o: &Output, dest: &Path) -> io::Result<bool> {
        let Ok(meta) = fs::symlink_metadata(dest) else {
            return Ok(false);
        };
        if o.kind == OutputKind::Link {
            if !meta.file_type().is_symlink() {
                return Ok(false);
            }
            let target = self.link_target(&o.blob)?;
            return Ok(fs::read_link(dest).is_ok_and(|t| t.to_str() == Some(target.as_str())));
        }
        if !meta.is_file() || meta.len() != o.size {
            return Ok(false);
        }
        if cfg!(unix) && executable(&meta) != (o.kind == OutputKind::Exec) {
            return Ok(false);
        }
        let Some(id) = unhex(&o.blob) else {
            return Ok(false);
        };
        Ok(file_id(dest)? == id)
    }
}

fn file_id(path: &Path) -> io::Result<[u8; 32]> {
    let mut f = fs::File::open(path)?;
    let mut h = FileHasher::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(*h.finalize().as_bytes());
        }
        h.update(&buf[..n]);
    }
}

fn check_root(root: &str) -> io::Result<()> {
    let ok = !root.is_empty()
        && !root.starts_with('/')
        && !root.contains('\\')
        && !root.contains(':')
        && root
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..");
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("output root '{root}' is not a relative path"),
        ))
    }
}

/// Removes what lies at or below `rel` and is not named (nor a directory holding something
/// named). Never follows a link: a link is a leaf. Returns the files and links removed.
fn prune(base: &Path, rel: &str, named: &BTreeSet<&str>) -> io::Result<usize> {
    let abs = base.join(rel);
    let meta = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        if named.contains(rel) {
            return Ok(0);
        }
        fs::remove_file(&abs)?;
        return Ok(1);
    }
    let prefix = format!("{rel}/");
    let holds = named
        .range(prefix.as_str()..)
        .next()
        .is_some_and(|p| p.starts_with(&prefix));
    if !holds {
        // Nothing named lives below: the whole directory is stale (or a file is due here).
        let mut n = 0;
        count_files(&abs, &mut n)?;
        fs::remove_dir_all(&abs)?;
        return Ok(n);
    }
    let mut removed = 0;
    for e in fs::read_dir(&abs)? {
        let e = e?;
        let Ok(name) = e.file_name().into_string() else {
            // A name that is not UTF-8 is never an output.
            let p = e.path();
            if fs::symlink_metadata(&p)?.is_dir() {
                let mut n = 0;
                count_files(&p, &mut n)?;
                fs::remove_dir_all(&p)?;
                removed += n;
            } else {
                fs::remove_file(&p)?;
                removed += 1;
            }
            continue;
        };
        removed += prune(base, &format!("{rel}/{name}"), named)?;
    }
    Ok(removed)
}

fn count_files(dir: &Path, n: &mut usize) -> io::Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            count_files(&e.path(), n)?;
        } else {
            *n += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::ActionKey;
    use crate::store::Origin;
    use std::time::{Duration, SystemTime};

    fn write(base: &Path, files: &[(&str, &[u8])]) {
        for (p, b) in files {
            let p = base.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b).unwrap();
        }
    }

    fn entry(s: &Store, base: &Path, files: &[(&str, &[u8])]) -> Action {
        let w = base.join("src");
        let _ = fs::remove_dir_all(&w);
        write(&w, files);
        let mut a = Action::new("vite", "web", Origin::default());
        a.outputs = s.put_outputs(&w, &["dist".into()]).unwrap();
        s.put_action(&ActionKey([1; 32]), &a).unwrap();
        a
    }

    fn mtime(p: &Path) -> SystemTime {
        fs::metadata(p).unwrap().modified().unwrap()
    }

    #[test]
    fn only_the_changed_file_is_written_and_stale_ones_go() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("bc")).unwrap();
        let v1 = entry(
            &s,
            d.path(),
            &[
                ("dist/a.js", b"a1"),
                ("dist/b.js", b"b1"),
                ("dist/old/x.js", b"x"),
            ],
        );
        let w = d.path().join("w");
        let roots = vec!["dist".to_owned()];
        let first = s.sync(&v1, &w, &roots).unwrap().unwrap();
        assert_eq!(
            first,
            Synced {
                written: 3,
                kept: 0,
                removed: 0
            }
        );

        // Age the tree so a rewrite shows in the modification time.
        let old = SystemTime::now() - Duration::from_secs(3600);
        for p in ["dist/a.js", "dist/b.js"] {
            fs::File::options()
                .write(true)
                .open(w.join(p))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        write(&w, &[("dist/leftover.map", b"m")]);
        let v2 = entry(
            &s,
            d.path(),
            &[("dist/a.js", b"a1"), ("dist/b.js", b"b2, changed")],
        );
        let second = s.sync(&v2, &w, &roots).unwrap().unwrap();
        assert_eq!(
            second,
            Synced {
                written: 1,
                kept: 1,
                removed: 2
            }
        );
        assert_eq!(
            mtime(&w.join("dist/a.js")),
            old,
            "an unchanged file is not touched"
        );
        assert_ne!(mtime(&w.join("dist/b.js")), old);
        assert_eq!(fs::read(w.join("dist/b.js")).unwrap(), b"b2, changed");
        assert!(!w.join("dist/old").exists());
        assert!(!w.join("dist/leftover.map").exists());
        // Same size, other bytes: the content decides, not the size.
        fs::write(w.join("dist/a.js"), b"zz").unwrap();
        assert_eq!(s.sync(&v2, &w, &roots).unwrap().unwrap().written, 1);
        assert_eq!(fs::read(w.join("dist/a.js")).unwrap(), b"a1");
    }

    #[test]
    fn a_missing_blob_removes_nothing_and_bad_roots_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("bc")).unwrap();
        let v = entry(&s, d.path(), &[("dist/a.js", b"a")]);
        let w = d.path().join("w");
        write(&w, &[("dist/keep.js", b"k")]);
        let id = unhex(&v.outputs[0].blob).unwrap();
        fs::remove_file(s.blob_path(&id)).unwrap();
        assert!(s.sync(&v, &w, &["dist".into()]).unwrap().is_none());
        assert!(w.join("dist/keep.js").exists());
        for root in ["../x", "/abs", "a/../b", "", "c:/x"] {
            assert!(
                s.sync(
                    &Action::new("t", "l", Origin::default()),
                    &w,
                    &[root.into()]
                )
                .is_err(),
                "{root}"
            );
        }
    }
}
