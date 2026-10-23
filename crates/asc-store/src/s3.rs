//! S3 bucket construction and the resilient put layer (SPEC §3.3).
//!
//! [`open`] builds an `AmazonS3` store from `s3://bucket/prefix?endpoint=&region=&path_style=`.
//! [`Resilient`] wraps any `ObjectStore` with the `PutMode::Create` fallback, multipart upload
//! above 32 MiB and `backon` retries for 5xx and timeouts.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use bytes::Bytes;
use futures::stream::BoxStream;
use object_store::aws::AmazonS3Builder;
use object_store::client::{HttpError, HttpErrorKind};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult, RenameOptions, WriteMultipart,
};

use asc_core::transport_policy::{Policy, Transport, require_network};

use crate::objects::{Error, Result};

/// Values larger than this are uploaded as a multipart upload.
pub const MULTIPART_THRESHOLD: usize = 32 * 1024 * 1024;

/// Attempts per request (1 try + 4 retries).
const MAX_RETRIES: usize = 4;

/// Builds the S3 store and key root for an `s3://` URL.
///
/// Credentials come from the environment (`AWS_*`) and IMDS. This crate never spawns git.
pub fn open(url: &url::Url) -> Result<(Arc<dyn ObjectStore>, Path)> {
    open_with(url, None)
}

/// Static S3 credentials injected by the caller.
#[derive(Clone)]
pub struct S3Credentials {
    pub key_id: String,
    pub secret: String,
}

impl fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Credentials")
            .field("key_id", &"<redacted>")
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Like [`open`], with optional caller-supplied static credentials.
pub fn open_with(
    url: &url::Url,
    credentials: Option<S3Credentials>,
) -> Result<(Arc<dyn ObjectStore>, Path)> {
    open_inner(url, credentials, true)
}

/// Like [`open_with`], but the store never downgrades `PutMode::Create` to an overwrite.
///
/// A store without conditional writes then fails with `NotImplemented` instead of silently
/// losing exclusivity. `BucketRefs` requires this (SPEC §3): the downgrade is harmless for
/// content-addressed objects but loses ref updates.
pub fn open_strict(
    url: &url::Url,
    credentials: Option<S3Credentials>,
) -> Result<(Arc<dyn ObjectStore>, Path)> {
    open_inner(url, credentials, false)
}

fn open_inner(
    url: &url::Url,
    credentials: Option<S3Credentials>,
    create_fallback: bool,
) -> Result<(Arc<dyn ObjectStore>, Path)> {
    open_under(url, credentials, create_fallback, Policy::current())
}

/// [`open_inner`] under an explicit transport policy: an `endpoint` is https, or plain http to
/// the local machine when the policy allows it (U89). The server's own `store` and turbo
/// endpoints follow the same rule as a client's.
fn open_under(
    url: &url::Url,
    credentials: Option<S3Credentials>,
    create_fallback: bool,
    policy: Policy,
) -> Result<(Arc<dyn ObjectStore>, Path)> {
    let bucket = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| Error::Url("s3 url has no bucket".into()))?;
    let mut builder = AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        // `Resilient` owns retries.
        .with_retry(object_store::RetryConfig {
            max_retries: 0,
            ..Default::default()
        });
    let (mut endpoint, mut region, mut path_style) = (None, None, None);
    for (k, v) in url.query_pairs() {
        match &*k {
            "endpoint" if !v.is_empty() => endpoint = Some(v.into_owned()),
            "region" if !v.is_empty() => region = Some(v.into_owned()),
            "path_style" if !v.is_empty() => path_style = Some(parse_bool(&v)?),
            "endpoint" | "region" | "path_style" => {}
            other => return Err(Error::Url(format!("unknown s3 url option `{other}`"))),
        }
    }
    if let Some(e) = endpoint {
        let allow_http = endpoint_allows_http(&e, policy)?;
        builder = builder.with_allow_http(allow_http).with_endpoint(e);
    }
    if let Some(r) = region {
        builder = builder.with_region(r);
    } else if std::env::var_os("AWS_REGION").is_none()
        && std::env::var_os("AWS_DEFAULT_REGION").is_none()
    {
        builder = builder.with_region("us-east-1");
    }
    if let Some(ps) = path_style {
        builder = builder.with_virtual_hosted_style_request(!ps);
    }
    // Caller-supplied static credentials override the environment; otherwise `object_store`
    // keeps its env and IMDS behaviour.
    if let Some(c) = credentials {
        builder = builder
            .with_access_key_id(c.key_id)
            .with_secret_access_key(c.secret);
    }
    let store = builder.build()?;
    let root = Path::from(url.path().trim_matches('/'));
    let mut store = Resilient::new(store);
    if !create_fallback {
        store = store.without_create_fallback();
    }
    Ok((Arc::new(store), root))
}

/// Whether the HTTP client may speak plain http to an `endpoint`: false for https, true for
/// loopback http under the opt-in, an error for anything else. The error names
/// `scheme://host:port` only.
fn endpoint_allows_http(endpoint: &str, policy: Policy) -> Result<bool> {
    match require_network(endpoint, policy) {
        Ok(Transport::Https) => Ok(false),
        Ok(Transport::LoopbackHttp) => Ok(true),
        Ok(_) => Err(Error::Url("s3 endpoint must be an http(s) url".into())),
        Err(why) => Err(Error::Url(format!("s3 endpoint: {why}"))),
    }
}

/// [`endpoint_allows_http`] under the current transport policy, for code that builds its own
/// S3 client from the same `s3://` URL (the turbo presigner).
pub fn endpoint_allows_http_now(endpoint: &str) -> Result<bool> {
    endpoint_allows_http(endpoint, Policy::current())
}

fn parse_bool(v: &str) -> Result<bool> {
    match v {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(Error::Url(format!("invalid boolean `{v}` in s3 url"))),
    }
}

/// The HTTP status of a failed request and the error text, read from the error message.
///
/// `object_store` keeps its request error types private, so the status is parsed from the
/// `Server returned non-2xx status code: NNN` text of the source chain.
fn http_status(e: &object_store::Error) -> Option<(u16, String)> {
    const MARK: &str = "non-2xx status code: ";
    let text = e.to_string();
    let at = text.find(MARK)? + MARK.len();
    let status = text.get(at..at + 3)?.parse().ok()?;
    Some((status, text.to_ascii_lowercase()))
}

/// True for 5xx (except 501) and transport failures (connect, timeout, reset).
pub fn is_transient(e: &object_store::Error) -> bool {
    if let Some((status, _)) = http_status(e) {
        return status >= 500 && status != 501;
    }
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(h) = err.downcast_ref::<HttpError>() {
            return matches!(
                h.kind(),
                HttpErrorKind::Connect
                    | HttpErrorKind::Request
                    | HttpErrorKind::Timeout
                    | HttpErrorKind::Interrupted
            );
        }
        cur = err.source();
    }
    false
}

/// True when the store does not support `PutMode::Create`: `NotImplemented`, 501, or a 400 that
/// names the conditional header.
pub fn is_create_unsupported(e: &object_store::Error) -> bool {
    match e {
        object_store::Error::NotImplemented { .. } | object_store::Error::NotSupported { .. } => {
            true
        }
        _ => http_status(e).is_some_and(|(status, text)| match status {
            501 => true,
            400 => ["if-none-match", "conditional", "precondition"]
                .iter()
                .any(|m| text.contains(m)),
            _ => false,
        }),
    }
}

/// An `ObjectStore` wrapper adding the create fallback, multipart and retries.
pub struct Resilient<T> {
    inner: T,
    create_unsupported: AtomicBool,
    /// Whether `Create` may be downgraded to `Overwrite` when unsupported.
    create_fallback: bool,
    min_delay: Duration,
    retryable: fn(&object_store::Error) -> bool,
}

impl<T: ObjectStore> Resilient<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            create_unsupported: AtomicBool::new(false),
            create_fallback: true,
            min_delay: Duration::from_millis(200),
            retryable: is_transient,
        }
    }

    /// Disables the `Create` to `Overwrite` downgrade: an unsupported `Create` stays an error.
    pub fn without_create_fallback(mut self) -> Self {
        self.create_fallback = false;
        self
    }

    /// Replaces the first backoff delay and the retry predicate (tests).
    pub fn with_retry(
        mut self,
        min_delay: Duration,
        retryable: fn(&object_store::Error) -> bool,
    ) -> Self {
        self.min_delay = min_delay;
        self.retryable = retryable;
        self
    }

    /// Whether `PutMode::Create` has been found unsupported by this store.
    pub fn create_unsupported(&self) -> bool {
        self.create_unsupported.load(Ordering::Relaxed)
    }

    async fn retrying<R, F, Fut>(&self, op: F) -> object_store::Result<R>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = object_store::Result<R>>,
    {
        let backoff = ExponentialBuilder::default()
            .with_min_delay(self.min_delay)
            .with_max_times(MAX_RETRIES);
        op.retry(backoff).when(self.retryable).await
    }

    async fn put_once(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        if payload.content_length() > MULTIPART_THRESHOLD {
            return self.put_large(location, payload, opts).await;
        }
        self.retrying(|| self.inner.put_opts(location, payload.clone(), opts.clone()))
            .await
    }

    /// Multipart upload. Objects are content-addressed, so the lack of a create-only guarantee
    /// is safe; a retried upload restarts from the first part.
    async fn put_large(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let PutOptions {
            tags, attributes, ..
        } = opts;
        let mopts = PutMultipartOptions {
            tags,
            attributes,
            ..Default::default()
        };
        self.retrying(|| async {
            let upload = self
                .inner
                .put_multipart_opts(location, mopts.clone())
                .await?;
            let mut w = WriteMultipart::new_with_chunk_size(upload, MULTIPART_THRESHOLD);
            for part in payload.iter() {
                w.put(part.clone());
            }
            w.finish().await
        })
        .await
    }
}

impl<T> fmt::Debug for Resilient<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resilient").finish_non_exhaustive()
    }
}

impl<T: ObjectStore> fmt::Display for Resilient<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Resilient({})", self.inner)
    }
}

#[async_trait]
impl<T: ObjectStore> ObjectStore for Resilient<T> {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        mut opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        if self.create_fallback && matches!(opts.mode, PutMode::Create) && self.create_unsupported()
        {
            opts.mode = PutMode::Overwrite;
        }
        match self.put_once(location, payload.clone(), opts.clone()).await {
            Err(e)
                if self.create_fallback
                    && matches!(opts.mode, PutMode::Create)
                    && is_create_unsupported(&e) =>
            {
                self.create_unsupported.store(true, Ordering::Relaxed);
                opts.mode = PutMode::Overwrite;
                self.put_once(location, payload, opts).await
            }
            r => r,
        }
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.retrying(|| self.inner.get_opts(location, options.clone()))
            .await
    }

    async fn get_ranges(
        &self,
        location: &Path,
        ranges: &[Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        self.retrying(|| self.inner.get_ranges(location, ranges))
            .await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }

    async fn rename_opts(
        &self,
        from: &Path,
        to: &Path,
        options: RenameOptions,
    ) -> object_store::Result<()> {
        self.inner.rename_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use object_store::memory::InMemory;
    use object_store::{ObjectStoreExt, PutMode};

    use super::*;
    use crate::bucket::BucketObjects;
    use crate::codec::encode_stored;
    use crate::key::ObjKey;
    use crate::objects::Objects;
    use asc_core::id::FileId;

    /// Counts calls by mode and fails per a script.
    struct Scripted {
        inner: InMemory,
        creates: AtomicUsize,
        overwrites: AtomicUsize,
        /// Errors returned before the real put, newest last.
        failures: std::sync::Mutex<Vec<object_store::Error>>,
        create_error: fn() -> Option<object_store::Error>,
    }

    impl Scripted {
        fn new(create_error: fn() -> Option<object_store::Error>) -> Self {
            Self {
                inner: InMemory::new(),
                creates: AtomicUsize::new(0),
                overwrites: AtomicUsize::new(0),
                failures: Default::default(),
                create_error,
            }
        }
    }

    impl fmt::Debug for Scripted {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("Scripted")
        }
    }

    impl fmt::Display for Scripted {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("Scripted")
        }
    }

    #[async_trait]
    impl ObjectStore for Scripted {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            if let Some(e) = self.failures.lock().unwrap().pop() {
                return Err(e);
            }
            if matches!(opts.mode, PutMode::Create) {
                self.creates.fetch_add(1, Ordering::Relaxed);
                if let Some(e) = (self.create_error)() {
                    return Err(e);
                }
            } else {
                self.overwrites.fetch_add(1, Ordering::Relaxed);
            }
            self.inner.put_opts(location, payload, opts).await
        }
        async fn put_multipart_opts(
            &self,
            location: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }
        async fn get_opts(&self, l: &Path, o: GetOptions) -> object_store::Result<GetResult> {
            self.inner.get_opts(l, o).await
        }
        async fn get_ranges(&self, l: &Path, r: &[Range<u64>]) -> object_store::Result<Vec<Bytes>> {
            self.inner.get_ranges(l, r).await
        }
        fn delete_stream(
            &self,
            l: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(l)
        }
        fn list(&self, p: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(p)
        }
        async fn list_with_delimiter(&self, p: Option<&Path>) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(p).await
        }
        async fn copy_opts(&self, f: &Path, t: &Path, o: CopyOptions) -> object_store::Result<()> {
            self.inner.copy_opts(f, t, o).await
        }
        async fn rename_opts(
            &self,
            f: &Path,
            t: &Path,
            o: RenameOptions,
        ) -> object_store::Result<()> {
            self.inner.rename_opts(f, t, o).await
        }
    }

    fn not_implemented() -> Option<object_store::Error> {
        Some(object_store::Error::NotImplemented {
            operation: "put_opts(Create)".into(),
            implementer: "test".into(),
        })
    }

    fn generic() -> object_store::Error {
        object_store::Error::Generic {
            store: "test",
            source: "boom".into(),
        }
    }

    fn quick<T: ObjectStore>(s: T) -> Resilient<T> {
        s.into_resilient()
    }

    trait IntoResilient: ObjectStore + Sized {
        fn into_resilient(self) -> Resilient<Self> {
            Resilient::new(self).with_retry(Duration::from_millis(1), |e| {
                matches!(e, object_store::Error::Generic { .. })
            })
        }
    }
    impl<T: ObjectStore> IntoResilient for T {}

    #[tokio::test]
    async fn create_fallback_sets_flag_once() {
        let store = Arc::new(quick(Scripted::new(not_implemented)));
        let b = BucketObjects::new(store.clone(), Path::default());
        let (k1, k2) = (
            ObjKey::File(FileId::of(b"one")),
            ObjKey::File(FileId::of(b"two")),
        );
        assert!(!store.create_unsupported());
        b.put(&k1, encode_stored(b"one")).await.unwrap();
        assert!(store.create_unsupported());
        b.put(&k2, encode_stored(b"two")).await.unwrap();
        assert_eq!(&b.get(&k1).await.unwrap().unwrap()[..], b"one");
        assert_eq!(&b.get(&k2).await.unwrap().unwrap()[..], b"two");
        // One failed Create probe, then only overwrites.
        assert_eq!(store.inner.creates.load(Ordering::Relaxed), 1);
        assert_eq!(store.inner.overwrites.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn strict_store_never_downgrades_create() {
        let store = quick(Scripted::new(not_implemented)).without_create_fallback();
        let opts = || PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        let p = Path::from("k");
        for _ in 0..2 {
            let err = store
                .put_opts(&p, PutPayload::from_static(b"x"), opts())
                .await
                .unwrap_err();
            assert!(is_create_unsupported(&err), "{err}");
        }
        assert!(!store.create_unsupported());
        assert_eq!(store.inner.overwrites.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn supported_create_keeps_create_only() {
        let store = Arc::new(quick(Scripted::new(|| None)));
        let b = BucketObjects::new(store.clone(), Path::default());
        let k = ObjKey::File(FileId::of(b"x"));
        b.put(&k, encode_stored(b"x")).await.unwrap();
        b.put(&k, encode_stored(b"x")).await.unwrap();
        assert!(!store.create_unsupported());
        assert_eq!(store.inner.overwrites.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn retries_transient_errors_then_succeeds() {
        let s = Scripted::new(|| None);
        s.failures.lock().unwrap().extend([generic(), generic()]);
        let store = quick(s);
        store
            .put_opts(
                &Path::from("a"),
                PutPayload::from_static(b"v"),
                PutOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            &store
                .get(&Path::from("a"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()[..],
            b"v"
        );
    }

    #[tokio::test]
    async fn gives_up_after_five_attempts() {
        let s = Scripted::new(|| None);
        s.failures.lock().unwrap().extend((0..5).map(|_| generic()));
        let store = quick(s);
        let p = Path::from("a");
        let r = store
            .put_opts(&p, PutPayload::from_static(b"v"), PutOptions::default())
            .await;
        assert!(r.is_err());
        assert!(store.inner.failures.lock().unwrap().is_empty());
        // The next call succeeds: exactly five attempts were consumed.
        store
            .put_opts(&p, PutPayload::from_static(b"v"), PutOptions::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn non_transient_errors_are_not_retried() {
        let s = Scripted::new(|| None);
        s.failures
            .lock()
            .unwrap()
            .push(object_store::Error::NotFound {
                path: "a".into(),
                source: "gone".into(),
            });
        let store = Resilient::new(s).with_retry(Duration::from_millis(1), is_transient);
        let r = store
            .put_opts(
                &Path::from("a"),
                PutPayload::from_static(b"v"),
                PutOptions::default(),
            )
            .await;
        assert!(matches!(r, Err(object_store::Error::NotFound { .. })));
    }

    #[test]
    fn classifies_errors() {
        assert!(is_create_unsupported(&not_implemented().unwrap()));
        assert!(!is_create_unsupported(&generic()));
        assert!(!is_transient(&generic()));
        assert!(!is_transient(&object_store::Error::NotFound {
            path: "a".into(),
            source: "x".into()
        }));
    }

    #[tokio::test]
    async fn large_values_use_multipart() {
        let store = quick(InMemory::new());
        let big = Bytes::from(vec![7u8; MULTIPART_THRESHOLD + 5]);
        let p = Path::from("big");
        store
            .put_opts(
                &p,
                PutPayload::from_bytes(big.clone()),
                PutOptions::default(),
            )
            .await
            .unwrap();
        let got = store.get(&p).await.unwrap().bytes().await.unwrap();
        assert_eq!(got.len(), big.len());
        assert!(got == big);
    }

    #[test]
    fn credentials_debug_redacts() {
        let c = S3Credentials {
            key_id: "AKIA".into(),
            secret: "sekret".into(),
        };
        let d = format!("{c:?}");
        assert!(!d.contains("AKIA") && !d.contains("sekret"));
        let u = url::Url::parse("s3://bkt/p?region=eu-west-1").unwrap();
        assert!(open_with(&u, Some(c)).is_ok());
    }

    #[test]
    fn a_plain_http_endpoint_needs_the_opt_in_and_a_loopback_host() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        let strict = |s: &str| open_under(&u(s), None, true, Policy::STRICT).err();
        let dev = |s: &str| open_under(&u(s), None, true, Policy::LOOPBACK_HTTP).err();
        for e in [
            "http://minio.example:9000",
            "http://192.0.2.7:9000",
            "http://127.example.com",
        ] {
            let url = format!("s3://bkt/p?endpoint={e}&region=r");
            assert!(strict(&url).is_some(), "{e}");
            assert!(dev(&url).is_some(), "{e} stays refused under the opt-in");
        }
        let local = "s3://bkt/p?endpoint=http://127.0.0.1:9000&region=r";
        assert!(strict(local).is_some(), "loopback http without the opt-in");
        assert!(dev(local).is_none());
        let https = "s3://bkt/p?endpoint=https://minio.example:9000&region=r";
        assert!(strict(https).is_none());
        // The server's own store and turbo endpoints are not an exception: a private-network
        // host over plain http is refused (U89c), https and opted-in loopback pass.
        let private = "s3://bkt/p?endpoint=http://garage-abc:3900&region=r";
        assert!(strict(private).is_some());
        assert!(dev(private).is_some());
        assert!(strict("s3://bkt/p?endpoint=ftp://garage:3900&region=r").is_some());
        assert!(matches!(
            endpoint_allows_http("https://garage.apps.example", Policy::STRICT),
            Ok(false)
        ));
        assert!(matches!(
            endpoint_allows_http("http://localhost:3900", Policy::LOOPBACK_HTTP),
            Ok(true)
        ));
        // The refusal names the origin and keeps secrets out.
        let url = "s3://bkt/p?endpoint=http://u:hunter2@minio.example:9000/x&region=r";
        let why = strict(url).unwrap().to_string();
        assert!(why.contains("http://minio.example:9000"), "{why}");
        assert!(!why.contains("hunter2"), "{why}");
    }

    #[test]
    fn open_parses_url_options() {
        asc_core::transport_policy::set_config_allow_loopback_http(true);
        let u = |s: &str| url::Url::parse(s).unwrap();
        let (_, root) = open(&u(
            "s3://bkt/some/prefix/?endpoint=http://127.0.0.1:9000&region=eu-west-1&path_style=true",
        ))
        .unwrap();
        assert_eq!(root.as_ref(), "some/prefix");
        let (_, root) = open(&u("s3://bkt")).unwrap();
        assert!(root.as_ref().is_empty());
        assert!(open(&u("s3://bkt/?bogus=1")).is_err());
        assert!(open(&u("s3://bkt/?path_style=maybe")).is_err());
    }

    #[test]
    fn bucket_from_url_dispatches_s3() {
        asc_core::transport_policy::set_config_allow_loopback_http(true);
        assert!(BucketObjects::from_url("s3://bkt/p?endpoint=http://127.0.0.1:1").is_ok());
    }
}
