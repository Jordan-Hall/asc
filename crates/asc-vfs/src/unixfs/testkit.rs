//! The base trees and recorders the merged file system's front-end tests share.

use std::io;
use std::sync::Mutex;

use asc_core::codec::Enveloped;
use asc_core::id::{FileId, GitOid, TreeId};
use asc_core::object::{Entry, Kind, Tree};
use asc_core::path::{Name, RepoPath};
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_wc::journal::Op;

use super::{JournalSink, TimeSource};

pub(crate) const EPOCH_TIME: i64 = 1_700_000_000;

pub(crate) struct FixedTimes;

impl TimeSource for FixedTimes {
    fn times(&self, paths: &[RepoPath]) -> io::Result<Vec<i64>> {
        Ok(vec![EPOCH_TIME; paths.len()])
    }
}

#[derive(Default)]
pub(crate) struct Recorded(Mutex<Vec<(Op, Vec<u8>)>>);

impl JournalSink for Recorded {
    fn record(&self, op: Op, path: &[u8]) {
        self.0.lock().unwrap().push((op, path.to_vec()));
    }
}

impl Recorded {
    pub(crate) fn ops(&self) -> Vec<(Op, String)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(o, p)| (*o, String::from_utf8_lossy(p).into_owned()))
            .collect()
    }
}

pub(crate) fn entry(name: &str, kind: Kind, id: [u8; 32], size: u64) -> Entry {
    Entry {
        name: Name::new(name).unwrap(),
        kind,
        id,
        size,
        git: GitOid::from_bytes([0; 20]),
    }
}

pub(crate) async fn put(store: &BucketObjects, key: ObjKey, data: &[u8]) {
    store.put(&key, encode_stored(data)).await.unwrap();
}

pub(crate) async fn tree(store: &BucketObjects, mut entries: Vec<Entry>) -> TreeId {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let t = Tree::new(entries, true).unwrap();
    put(store, ObjKey::Tree(t.id()), &t.to_envelope()).await;
    t.id()
}

pub(crate) async fn blob(store: &BucketObjects, name: &str, kind: Kind, data: &[u8]) -> Entry {
    let id = FileId::of(data);
    put(store, ObjKey::File(id), data).await;
    entry(name, kind, *id.as_bytes(), data.len() as u64)
}

/// `README`, `run.sh`, `link`, `src/{a.txt,b.txt,sub/deep.txt}`; the second tree changes
/// `a.txt` and `deep.txt`, drops `b.txt` and adds `c.txt`.
pub(crate) async fn build(store: &BucketObjects) -> (TreeId, TreeId) {
    let mut roots = Vec::new();
    for second in [false, true] {
        let a: &[u8] = if second {
            b"alpha two, longer\n"
        } else {
            b"alpha\n"
        };
        let deep: &[u8] = if second { b"deep two\n" } else { b"deep\n" };
        let sub = tree(store, vec![blob(store, "deep.txt", Kind::File, deep).await]).await;
        let mut src = vec![
            blob(store, "a.txt", Kind::File, a).await,
            entry("sub", Kind::Dir, *sub.as_bytes(), 0),
        ];
        if second {
            src.push(blob(store, "c.txt", Kind::File, b"gamma\n").await);
        } else {
            src.push(blob(store, "b.txt", Kind::File, b"beta\n").await);
        }
        let src = tree(store, src).await;
        roots.push(
            tree(
                store,
                vec![
                    blob(store, "README", Kind::File, b"read me\n").await,
                    blob(store, "run.sh", Kind::Exec, b"#!/bin/sh\n").await,
                    blob(store, "link", Kind::Symlink, b"README").await,
                    entry("src", Kind::Dir, *src.as_bytes(), 0),
                ],
            )
            .await,
        );
    }
    (roots[0], roots[1])
}
