//! Git import as the simulation sees it (SPEC 14.4): objects, then the `cs_graph` row, then the
//! `git_map` row last (I3). A crash at `mid_import` is followed by a rerun that must converge.

use std::collections::BTreeSet;

use asc_store::key::KeyPrefix;
use futures::StreamExt;

use crate::faults::CrashPoint;
use crate::model::{Cs, Fail, R, SimKey, oerr, viol};
use crate::rng::Rng;
use crate::world::World;

/// A git commit to import: files, and the parent commit id.
#[derive(Clone, Debug)]
pub struct GitCommit {
    pub id: u64,
    pub parent: Option<u64>,
    pub files: Vec<(u8, Vec<u8>)>,
}

/// A deterministic linear history of `n` commits.
pub fn history(rng: &mut Rng, n: usize) -> Vec<GitCommit> {
    (0..n)
        .map(|i| GitCommit {
            id: 1000 + i as u64,
            parent: (i > 0).then(|| 999 + i as u64),
            files: (0..1 + rng.index(3))
                .map(|k| {
                    (
                        k as u8,
                        format!("commit {i} file {k} {}", rng.next_u64()).into_bytes(),
                    )
                })
                .collect(),
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct Importer {
    /// The commit being worked on, and the micro-step inside it: files, changeset, graph row,
    /// map row. A different next commit (another importer finished this one) restarts at zero.
    cur: Option<u64>,
    phase: usize,
}

impl World {
    /// The first commit without a `git_map` row (incremental import's selection).
    fn next_commit<'a>(&self, all: &'a [GitCommit]) -> Option<&'a GitCommit> {
        all.iter().find(|c| !self.db.git_map.contains_key(&c.id))
    }

    pub fn import_done(&self, all: &[GitCommit]) -> bool {
        self.next_commit(all).is_none()
    }

    /// One micro-step of one importer. `Err(Crashed)` means the importer died.
    pub async fn import_step(&mut self, imp: &mut Importer, all: &[GitCommit]) -> R<()> {
        let Some(c) = self.next_commit(all).cloned() else {
            return Ok(());
        };
        if imp.cur != Some(c.id) {
            imp.cur = Some(c.id);
            imp.phase = 0;
        }
        let nfiles = c.files.len();
        if imp.phase < nfiles {
            let body = &c.files[imp.phase].1;
            self.put_obj(SimKey::file(body), body).await?;
        } else if imp.phase == nfiles {
            let cs = self.commit_cs(&c)?;
            self.put_obj(SimKey::Cs(cs.id()), &cs.encode()).await?;
        } else if imp.phase == nfiles + 1 {
            // I2: the row of every parent exists before this one.
            let cs = self.commit_cs(&c)?;
            let parents: Vec<[u8; 32]> = cs.parent.into_iter().collect();
            if let Some(p) = parents.first()
                && !self.db.cs_graph.contains_key(p)
            {
                return viol("I2: a cs_graph row is written before its parent's row");
            }
            // I5: INSERT .. ON CONFLICT DO NOTHING, and a conflicting row is a bug.
            match self.db.cs_graph.get(&cs.id()) {
                Some(old) if *old != parents => {
                    return viol("I5: two importers wrote different rows");
                }
                Some(_) => {}
                None => {
                    self.db.cs_graph.insert(cs.id(), parents);
                }
            }
        } else {
            // I3: the commit point, last.
            let cs = self.commit_cs(&c)?;
            if !self.db.cs_graph.contains_key(&cs.id()) {
                return viol("I3: git_map row before the cs_graph row");
            }
            if self
                .raw_has(&[SimKey::Cs(cs.id())])
                .await?
                .iter()
                .any(|h| !h)
            {
                return viol("I3: git_map row before the changeset object");
            }
            self.db.git_map.entry(c.id).or_insert(cs.id());
            imp.phase = 0;
            return Ok(());
        }
        imp.phase += 1;
        if self.crash.hit(CrashPoint::MidImport) {
            return Err(Fail::Crashed(CrashPoint::MidImport));
        }
        Ok(())
    }

    fn commit_cs(&self, c: &GitCommit) -> R<Cs> {
        let parent = match c.parent {
            None => None,
            Some(p) => match self.db.git_map.get(&p) {
                Some(id) => Some(*id),
                None => return viol("I2: a commit is imported before its parent"),
            },
        };
        Ok(Cs {
            parent,
            files: c
                .files
                .iter()
                .map(|(p, b)| match SimKey::file(b) {
                    SimKey::File(id) => (*p, id),
                    SimKey::Cs(id) => (*p, id),
                })
                .collect(),
            accepted: vec![],
            nonce: c.id,
        })
    }

    /// Everything that must be identical between a crashed-and-rerun import and a clean one.
    pub async fn fingerprint(
        &self,
    ) -> R<(
        BTreeSet<SimKey>,
        Vec<(u64, [u8; 32])>,
        Vec<([u8; 32], Vec<[u8; 32]>)>,
    )> {
        let mut keys = BTreeSet::new();
        let mut s = self.raw_objects.list(KeyPrefix::all());
        while let Some(item) = s.next().await {
            let (k, _) = item.map_err(oerr)?;
            if let Some(sk) = SimKey::of_obj(&k) {
                keys.insert(sk);
            }
        }
        Ok((
            keys,
            self.db.git_map.iter().map(|(a, b)| (*a, *b)).collect(),
            self.db
                .cs_graph
                .iter()
                .map(|(a, b)| (*a, b.clone()))
                .collect(),
        ))
    }
}

use asc_store::objects::Objects as _;
