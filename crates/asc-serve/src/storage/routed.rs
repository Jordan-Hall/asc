//! `RoutedStore`: one repository namespace spread over up to three places.
//!
//! The registry hands `BucketObjects` a single store and a single root, but a routed repository
//! may keep objects in its target, in the place they were before an assignment (`fallback`) and,
//! while a migration copies, in the destination (`mirror`). `RoutedStore` is that one store:
//!
//! * **reads** try the primary, then the fallback, and only on "not found";
//! * **writes** go to the primary and, during a migration, also to the mirror. Both must
//!   succeed: a write that reached only one place would leave the copy short;
//! * **deletes** go everywhere the object can be;
//! * **lists** are the union of the places that can hold objects.
//!
//! Each place has its own root (a target's prefix, or the instance bucket's root), so every path
//! is translated from the namespace root the registry uses (`ns/<hex>`) to the member's.

use std::collections::HashSet;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use futures::{StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};

/// Concurrent deletes of one `delete_stream`.
const DELETE_CONCURRENCY: usize = 16;

/// One place objects can be, with the root its keys live under.
#[derive(Clone)]
pub struct Member {
    pub store: Arc<dyn ObjectStore>,
    /// Where this member keeps the namespace: the `real_root` paths are rewritten to.
    pub root: Path,
}

impl fmt::Debug for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Member").finish_non_exhaustive()
    }
}

/// See the module documentation.
#[derive(Clone)]
pub struct RoutedStore {
    /// The root the registry's `BucketObjects` uses (`ns/<hex>`).
    virtual_root: Path,
    writes: Vec<Member>,
    reads: Vec<Member>,
}

impl fmt::Debug for RoutedStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoutedStore")
            .field("writes", &self.writes.len())
            .field("reads", &self.reads.len())
            .finish()
    }
}

impl fmt::Display for RoutedStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RoutedStore")
    }
}

fn generic(msg: &'static str) -> object_store::Error {
    object_store::Error::Generic {
        store: "RoutedStore",
        source: msg.into(),
    }
}

impl RoutedStore {
    /// `writes[0]` is authoritative; `reads` is tried in order. Both are non-empty.
    pub fn new(virtual_root: Path, writes: Vec<Member>, reads: Vec<Member>) -> Option<Self> {
        if writes.is_empty() || reads.is_empty() {
            return None;
        }
        Some(Self {
            virtual_root,
            writes,
            reads,
        })
    }

    pub fn virtual_root(&self) -> &Path {
        &self.virtual_root
    }

    /// Every distinct member, writes first.
    fn all(&self) -> Vec<Member> {
        let mut out: Vec<Member> = Vec::new();
        for m in self.writes.iter().chain(self.reads.iter()) {
            if !out
                .iter()
                .any(|o| Arc::ptr_eq(&o.store, &m.store) && o.root == m.root)
            {
                out.push(m.clone());
            }
        }
        out
    }

    fn to_member(&self, m: &Member, p: &Path) -> object_store::Result<Path> {
        let rest = p
            .prefix_match(&self.virtual_root)
            .ok_or_else(|| generic("path outside the repository namespace"))?;
        Ok(m.root.parts().chain(rest).collect())
    }

    fn to_virtual(&self, m: &Member, p: &Path) -> Option<Path> {
        let rest = p.prefix_match(&m.root)?;
        Some(self.virtual_root.parts().chain(rest).collect())
    }

    fn translate_meta(&self, m: &Member, mut meta: ObjectMeta) -> Option<ObjectMeta> {
        meta.location = self.to_virtual(m, &meta.location)?;
        Some(meta)
    }
}

#[async_trait]
impl ObjectStore for RoutedStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let mut first: Option<PutResult> = None;
        for (i, m) in self.writes.iter().enumerate() {
            let path = self.to_member(m, location)?;
            match m.store.put_opts(&path, payload.clone(), opts.clone()).await {
                Ok(r) => {
                    first.get_or_insert(r);
                }
                // The object is already in a mirror (copied earlier): content-addressed, so equal.
                Err(object_store::Error::AlreadyExists { .. }) if i > 0 => {}
                Err(e) => return Err(e),
            }
        }
        first.ok_or_else(|| generic("no write target"))
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        match self.writes.as_slice() {
            [only] => {
                only.store
                    .put_multipart_opts(&self.to_member(only, location)?, opts)
                    .await
            }
            _ => Err(generic(
                "multipart upload is not available while a migration runs",
            )),
        }
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let mut last = None;
        for m in &self.reads {
            let path = self.to_member(m, location)?;
            match m.store.get_opts(&path, options.clone()).await {
                Ok(mut r) => {
                    r.meta.location = location.clone();
                    return Ok(r);
                }
                Err(e @ object_store::Error::NotFound { .. }) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| generic("no read source")))
    }

    async fn get_ranges(
        &self,
        location: &Path,
        ranges: &[Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        let mut last = None;
        for m in &self.reads {
            let path = self.to_member(m, location)?;
            match m.store.get_ranges(&path, ranges).await {
                Ok(r) => return Ok(r),
                Err(e @ object_store::Error::NotFound { .. }) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| generic("no read source")))
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        let this = self.clone();
        locations
            .map(move |loc| {
                let this = this.clone();
                async move {
                    let loc = loc?;
                    for m in this.all() {
                        let path = this.to_member(&m, &loc)?;
                        match m.store.delete(&path).await {
                            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(loc)
                }
            })
            .buffered(DELETE_CONCURRENCY)
            .boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.list_inner(prefix, None)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.list_inner(prefix, Some(offset))
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        let members = self.reads_distinct();
        let mut out = ListResult {
            common_prefixes: Vec::new(),
            objects: Vec::new(),
            extensions: object_store::Extensions::default(),
        };
        let (mut seen_p, mut seen_o) = (HashSet::new(), HashSet::new());
        for m in members {
            let mapped = match prefix {
                Some(p) => Some(self.to_member(&m, p)?),
                None => Some(self.to_member(&m, &self.virtual_root)?),
            };
            let r = m.store.list_with_delimiter(mapped.as_ref()).await?;
            for p in r.common_prefixes {
                if let Some(v) = self.to_virtual(&m, &p)
                    && seen_p.insert(v.clone())
                {
                    out.common_prefixes.push(v);
                }
            }
            for o in r.objects {
                if let Some(o) = self.translate_meta(&m, o)
                    && seen_o.insert(o.location.clone())
                {
                    out.objects.push(o);
                }
            }
        }
        Ok(out)
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        match (self.writes.as_slice(), self.reads.as_slice()) {
            ([w], [r]) if Arc::ptr_eq(&w.store, &r.store) && w.root == r.root => {
                w.store
                    .copy_opts(&self.to_member(w, from)?, &self.to_member(w, to)?, options)
                    .await
            }
            _ => Err(generic(
                "copy is not available for a repository in several places",
            )),
        }
    }
}

impl RoutedStore {
    fn reads_distinct(&self) -> Vec<Member> {
        let mut out: Vec<Member> = Vec::new();
        for m in &self.reads {
            if !out
                .iter()
                .any(|o| Arc::ptr_eq(&o.store, &m.store) && o.root == m.root)
            {
                out.push(m.clone());
            }
        }
        out
    }

    fn list_inner(
        &self,
        prefix: Option<&Path>,
        offset: Option<&Path>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let members = self.reads_distinct();
        let single = members.len() == 1;
        let mut streams: Vec<BoxStream<'static, object_store::Result<ObjectMeta>>> = Vec::new();
        for m in members {
            let mapped_prefix = match prefix {
                Some(p) => self.to_member(&m, p),
                None => self.to_member(&m, &self.virtual_root),
            };
            let mapped_offset = offset.map(|o| self.to_member(&m, o)).transpose();
            let (mp, mo) = match (mapped_prefix, mapped_offset) {
                (Ok(p), Ok(o)) => (p, o),
                (Err(e), _) | (_, Err(e)) => return stream::once(async { Err(e) }).boxed(),
            };
            let inner = match mo {
                Some(o) => m.store.list_with_offset(Some(&mp), &o),
                None => m.store.list(Some(&mp)),
            };
            let this = self.clone();
            let m2 = m.clone();
            streams.push(
                inner
                    .try_filter_map(move |meta| {
                        let out = this.translate_meta(&m2, meta);
                        async move { Ok(out) }
                    })
                    .boxed(),
            );
        }
        if single {
            return streams.pop().unwrap_or_else(|| stream::empty().boxed());
        }
        // Several places: the union, each location once.
        let mut seen = HashSet::new();
        stream::iter(streams)
            .flatten()
            .try_filter(move |meta| {
                let fresh = seen.insert(meta.location.clone());
                async move { fresh }
            })
            .boxed()
    }
}

#[cfg(test)]
mod tests {
    use object_store::memory::InMemory;

    use super::*;

    fn mem() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    fn member(store: &Arc<dyn ObjectStore>, root: &str) -> Member {
        Member {
            store: store.clone(),
            root: Path::from(root),
        }
    }

    async fn put(m: &Arc<dyn ObjectStore>, p: &str, body: &'static [u8]) {
        m.put(&Path::from(p), PutPayload::from_static(body))
            .await
            .unwrap();
    }

    async fn names(s: &RoutedStore, prefix: &str) -> Vec<String> {
        let mut v: Vec<String> = s
            .list(Some(&Path::from(prefix)))
            .map_ok(|m| m.location.to_string())
            .try_collect()
            .await
            .unwrap();
        v.sort();
        v
    }

    #[tokio::test]
    async fn paths_move_between_the_namespace_root_and_each_members_root() {
        let a = mem();
        let s = RoutedStore::new(
            Path::from("ns/abc"),
            vec![member(&a, "tenant/pre/ns/abc")],
            vec![member(&a, "tenant/pre/ns/abc")],
        )
        .unwrap();
        s.put(&Path::from("ns/abc/f/1"), PutPayload::from_static(b"one"))
            .await
            .unwrap();
        assert_eq!(
            &a.get(&Path::from("tenant/pre/ns/abc/f/1"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()[..],
            b"one"
        );
        assert_eq!(
            &s.get(&Path::from("ns/abc/f/1"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()[..],
            b"one"
        );
        assert_eq!(names(&s, "ns/abc").await, vec!["ns/abc/f/1"]);
        let head = s.head(&Path::from("ns/abc/f/1")).await.unwrap();
        assert_eq!(
            (head.size, head.location.to_string().as_str()),
            (3, "ns/abc/f/1")
        );
        // Outside the namespace is refused, not translated.
        assert!(s.get(&Path::from("ns/other/f/1")).await.is_err());
        assert!(
            s.put(&Path::from("elsewhere/x"), PutPayload::from_static(b"x"))
                .await
                .is_err()
        );
        s.delete(&Path::from("ns/abc/f/1")).await.unwrap();
        assert!(matches!(
            a.get(&Path::from("tenant/pre/ns/abc/f/1")).await,
            Err(object_store::Error::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn reads_fall_back_only_on_not_found_and_writes_go_to_the_primary() {
        let (primary, old) = (mem(), mem());
        put(&old, "ns/abc/f/old", b"old").await;
        let s = RoutedStore::new(
            Path::from("ns/abc"),
            vec![member(&primary, "ns/abc")],
            vec![member(&primary, "ns/abc"), member(&old, "ns/abc")],
        )
        .unwrap();
        assert_eq!(
            &s.get(&Path::from("ns/abc/f/old"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()[..],
            b"old"
        );
        s.put(&Path::from("ns/abc/f/new"), PutPayload::from_static(b"new"))
            .await
            .unwrap();
        assert!(primary.head(&Path::from("ns/abc/f/new")).await.is_ok());
        assert!(old.head(&Path::from("ns/abc/f/new")).await.is_err());
        assert!(matches!(
            s.get(&Path::from("ns/abc/f/none")).await,
            Err(object_store::Error::NotFound { .. })
        ));
        assert_eq!(
            names(&s, "ns/abc").await,
            vec!["ns/abc/f/new", "ns/abc/f/old"]
        );
        // A delete reaches both places.
        s.delete(&Path::from("ns/abc/f/old")).await.unwrap();
        assert!(old.head(&Path::from("ns/abc/f/old")).await.is_err());
    }

    #[tokio::test]
    async fn mirrored_writes_reach_both_places_and_a_failing_mirror_fails_the_write() {
        let (src, dst) = (mem(), mem());
        let s = RoutedStore::new(
            Path::from("ns/abc"),
            vec![member(&src, "ns/abc"), member(&dst, "x/ns/abc")],
            vec![member(&src, "ns/abc")],
        )
        .unwrap();
        s.put(&Path::from("ns/abc/f/1"), PutPayload::from_static(b"1"))
            .await
            .unwrap();
        assert!(src.head(&Path::from("ns/abc/f/1")).await.is_ok());
        assert!(dst.head(&Path::from("x/ns/abc/f/1")).await.is_ok());
        // Create mode against an object that is already in the mirror is not an error there.
        put(&dst, "x/ns/abc/f/2", b"2").await;
        let create = PutOptions {
            mode: object_store::PutMode::Create,
            ..Default::default()
        };
        s.put_opts(
            &Path::from("ns/abc/f/2"),
            PutPayload::from_static(b"2"),
            create.clone(),
        )
        .await
        .unwrap();
        // Creating what the primary already has is the primary's error.
        assert!(matches!(
            s.put_opts(
                &Path::from("ns/abc/f/2"),
                PutPayload::from_static(b"2"),
                create
            )
            .await,
            Err(object_store::Error::AlreadyExists { .. })
        ));
        s.delete(&Path::from("ns/abc/f/1")).await.unwrap();
        assert!(dst.head(&Path::from("x/ns/abc/f/1")).await.is_err());
        // Reads stay on the primary during the copy: the mirror is not trusted until it is switched to.
        put(&dst, "x/ns/abc/f/only-in-mirror", b"m").await;
        assert!(s.get(&Path::from("ns/abc/f/only-in-mirror")).await.is_err());
    }

    #[tokio::test]
    async fn listing_with_an_offset_and_delimiter() {
        let a = mem();
        for k in ["f/1", "f/2", "t/1"] {
            put(&a, &format!("root/ns/abc/{k}"), b"x").await;
        }
        let s = RoutedStore::new(
            Path::from("ns/abc"),
            vec![member(&a, "root/ns/abc")],
            vec![member(&a, "root/ns/abc")],
        )
        .unwrap();
        let after: Vec<String> = s
            .list_with_offset(Some(&Path::from("ns/abc")), &Path::from("ns/abc/f/1"))
            .map_ok(|m| m.location.to_string())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(after, vec!["ns/abc/f/2", "ns/abc/t/1"]);
        let d = s
            .list_with_delimiter(Some(&Path::from("ns/abc")))
            .await
            .unwrap();
        let mut dirs: Vec<String> = d.common_prefixes.iter().map(ToString::to_string).collect();
        dirs.sort();
        assert_eq!(dirs, vec!["ns/abc/f", "ns/abc/t"]);
    }

    #[test]
    fn needs_a_member_to_write_and_to_read() {
        let a = mem();
        assert!(RoutedStore::new(Path::from("ns/a"), vec![], vec![member(&a, "x")]).is_none());
        assert!(RoutedStore::new(Path::from("ns/a"), vec![member(&a, "x")], vec![]).is_none());
    }
}
