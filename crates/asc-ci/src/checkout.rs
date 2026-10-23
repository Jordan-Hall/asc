//! `asc ci checkout` (SPEC §12.3): the parts that do not need a repository. The plan (validated
//! inputs and derived paths), the cache mode decided from the event, the exports for later
//! steps, the tier stack written into the repository config, the `HostCacheRO` layer and the
//! composition grant check. The CLI drives clone, fetch and checkout around this plan.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

use asc_store::cachedir::CacheDir;
use asc_store::codec::encode_stored;
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::{Objects, Result as ObjResult};
use asc_store::tiered::Tiered;

use crate::env::{self, EnvError};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CheckoutError {
    #[error("{0}")]
    Input(String),
    /// SPEC §15.5: a task token is bound to its repository; composition needs a grant.
    #[error("PATH_POLICY: {path} needs read on {child}")]
    PathPolicy { path: String, child: String },
    #[error(transparent)]
    Env(#[from] EnvError),
}

/// Whether the job may write the shared turbo remote cache's `trusted` scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMode {
    /// A push event: `TURBO_CACHE=local:rw,remote:rw`.
    Trusted,
    /// Anything else (pull requests, unknown events): `local:rw,remote:r`.
    Pr,
}

impl CacheMode {
    /// Decided from `GITHUB_EVENT_NAME`. Only `push` is trusted; an unknown event fails closed
    /// to the PR mode, and the server still decides the write scope from the token (SPEC §12.4).
    pub fn from_event(event: &str) -> Self {
        if event == "push" {
            Self::Trusted
        } else {
            Self::Pr
        }
    }

    pub fn turbo_cache(self) -> &'static str {
        match self {
            Self::Trusted => "local:rw,remote:rw",
            Self::Pr => "local:rw,remote:r",
        }
    }
}

/// Raw inputs of the command.
#[derive(Clone, Debug)]
pub struct Options {
    /// `owner/repo` (the server's repository slug).
    pub repo: String,
    pub rev: String,
    pub base: Option<String>,
    pub dir: PathBuf,
    /// `plain` is the only mode built (SPEC §12.3 "Virtual CI").
    pub mode: String,
    pub sparse: Vec<String>,
    pub event: String,
    pub runner_temp: PathBuf,
    /// The runner host's read-only cache (`/var/cache/asc/cache`).
    pub host_cache: Option<PathBuf>,
    /// The asc server's public URL (`https`, or loopback `http`).
    pub server: String,
    pub token: String,
    pub shim_dir: PathBuf,
}

/// Validated inputs plus the paths derived from them.
#[derive(Clone, Debug)]
pub struct Plan {
    pub opts: Options,
    pub cache_mode: CacheMode,
    pub job_cache: PathBuf,
    pub asc_home: PathBuf,
}

fn no_ctl(s: &str) -> bool {
    !s.chars().any(char::is_control)
}

fn slug_ok(s: &str) -> bool {
    let mut parts = s.split('/');
    let part = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
    };
    part(parts.next()) && part(parts.next()) && parts.next().is_none()
}

fn ref_arg_ok(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && no_ctl(s) && !s.contains(' ') && s.len() <= 1024
}

fn loopback_http(lower: &str) -> bool {
    ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| {
            lower
                .strip_prefix(p)
                .is_some_and(|r| r.is_empty() || r.starts_with(':') || r.starts_with('/'))
        })
}

impl Plan {
    pub fn new(opts: Options) -> Result<Self, CheckoutError> {
        let bad = |m: &str| Err(CheckoutError::Input(m.to_owned()));
        if !slug_ok(&opts.repo) {
            return bad("--repo must be <owner>/<repo>");
        }
        if !ref_arg_ok(&opts.rev) {
            return bad("--rev is not a usable revision");
        }
        if let Some(b) = &opts.base
            && !ref_arg_ok(b)
        {
            return bad("--base is not a usable revision");
        }
        match opts.mode.as_str() {
            "plain" => {}
            "virtual" => return bad("--mode virtual is not built; use plain"),
            _ => return bad("--mode must be plain"),
        }
        if opts.sparse.iter().any(|s| !ref_arg_ok(s)) {
            return bad("--sparse takes cone directories");
        }
        if opts.token.is_empty() || !no_ctl(&opts.token) {
            return bad("no Actions task token: GITHUB_TOKEN is empty");
        }
        let lower = opts.server.to_ascii_lowercase();
        if !(lower.starts_with("https://") || loopback_http(&lower)) || !no_ctl(&opts.server) {
            return bad("the server URL must be https (plain http only to loopback)");
        }
        if opts.runner_temp.as_os_str().is_empty() {
            return bad("RUNNER_TEMP is not set");
        }
        let cache_mode = CacheMode::from_event(&opts.event);
        let job_cache = opts.runner_temp.join("asc-cache");
        let asc_home = opts.runner_temp.join("asc-home");
        Ok(Self {
            opts,
            cache_mode,
            job_cache,
            asc_home,
        })
    }

    /// `TURBO_TEAM`: `<owner>.<repo>`.
    pub fn team(&self) -> String {
        self.opts.repo.replace('/', ".")
    }

    /// Whether the host cache is absent or holds no objects, so a seed should be unpacked
    /// into the job cache first (U72). The host cache itself is never written.
    pub fn host_cache_is_cold(&self) -> bool {
        let Some(h) = &self.opts.host_cache else {
            return true;
        };
        !["c", "t", "f"]
            .iter()
            .any(|class| std::fs::read_dir(h.join(class)).is_ok_and(|mut d| d.next().is_some()))
    }

    /// The repository config keys of step 6 (SPEC §3.5): `asc.cache.dir`, `asc.cache.readOnly`
    /// and `remote.origin.ascServer`.
    pub fn config_entries(&self) -> Vec<(String, String)> {
        let slash = |p: &Path| p.display().to_string().replace('\\', "/");
        let mut v = vec![("asc.cache.dir".to_owned(), slash(&self.job_cache))];
        if let Some(h) = &self.opts.host_cache {
            v.push(("asc.cache.readOnly".to_owned(), slash(h)));
        }
        v.push((
            "remote.origin.ascServer".to_owned(),
            self.opts.server.clone(),
        ));
        v
    }

    /// The same keys as `GIT_CONFIG_COUNT` variables, so the clone itself already reads the
    /// host cache before the repository config exists.
    pub fn config_env(&self) -> Vec<(String, String)> {
        let entries = self.config_entries();
        let mut v = vec![("GIT_CONFIG_COUNT".to_owned(), entries.len().to_string())];
        for (i, (k, val)) in entries.into_iter().enumerate() {
            v.push((format!("GIT_CONFIG_KEY_{i}"), k));
            v.push((format!("GIT_CONFIG_VALUE_{i}"), val));
        }
        v
    }

    /// Step 5 and step 6 exports for the `GITHUB_ENV` file. `merge_base` is the git sha of the
    /// merge base, when a base was given.
    pub fn exports(&self, merge_base: Option<&str>) -> Vec<(String, String)> {
        let mut v = Vec::new();
        if let Some(mb) = merge_base {
            v.push(("TURBO_SCM_BASE".to_owned(), mb.to_owned()));
        }
        v.push(("TURBO_SCM_HEAD".to_owned(), "HEAD".to_owned()));
        v.push(("TURBO_API".to_owned(), self.opts.server.clone()));
        v.push(("TURBO_TOKEN".to_owned(), self.opts.token.clone()));
        v.push(("TURBO_TEAM".to_owned(), self.team()));
        v.push((
            "TURBO_CACHE".to_owned(),
            self.cache_mode.turbo_cache().to_owned(),
        ));
        v.push(("ASC_HOME".to_owned(), self.asc_home.display().to_string()));
        v.push(("ASC_TOKEN".to_owned(), self.opts.token.clone()));
        v
    }

    /// Writes the exports to the `GITHUB_ENV` file and the shim directory to `GITHUB_PATH`.
    pub fn write_exports(
        &self,
        github_env: &Path,
        github_path: &Path,
        merge_base: Option<&str>,
    ) -> Result<(), CheckoutError> {
        env::append_env(github_env, &self.exports(merge_base))?;
        env::append_path(github_path, &[self.opts.shim_dir.display().to_string()])?;
        Ok(())
    }
}

/// A composition grant from `asc-serve.toml [[composition_grants]]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub parent: String,
    pub child: String,
}

/// Fails with `PATH_POLICY: <path> needs read on <child>` for the first composed child of
/// `parent` that no grant covers (SPEC §15.5). `composed` lists `(path, child repo)`.
pub fn check_grants(
    parent: &str,
    composed: &[(String, String)],
    grants: &[Grant],
) -> Result<(), CheckoutError> {
    for (path, child) in composed {
        if !grants
            .iter()
            .any(|g| g.parent == parent && &g.child == child)
        {
            return Err(CheckoutError::PathPolicy {
                path: path.clone(),
                child: child.clone(),
            });
        }
    }
    Ok(())
}

/// `HostCacheRO` layered over `JobCache` and the remote (SPEC §12.3 step 3). Reads try the
/// host cache first; a missing or corrupt host file is a miss (the file stays, the next tier
/// supplies verified bytes). All writes and fills go to the job cache, never the host cache.
pub struct Layered<R> {
    host: CacheDir,
    job: Tiered<CacheDir, R>,
}

impl<R: Objects> Layered<R> {
    /// `host_root` is opened read-only; `job` is the writable job cache.
    pub fn new(host_root: impl Into<PathBuf>, job: CacheDir, remote: R) -> ObjResult<Self> {
        Ok(Self {
            host: CacheDir::open_read_only(host_root)?,
            job: Tiered::new(job, remote),
        })
    }
}

#[async_trait]
impl<R: Objects> Objects for Layered<R> {
    async fn get(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
        if let Ok(Some(data)) = self.host.get(key).await {
            return Ok(Some(data));
        }
        self.job.get(key).await
    }

    async fn get_stored(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
        if let Ok(Some(data)) = self.host.get(key).await {
            return Ok(Some(encode_stored(&data)));
        }
        self.job.get_stored(key).await
    }

    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> ObjResult<Option<Bytes>> {
        if let Ok(Some(data)) = self.host.get_range(key, r.clone()).await {
            return Ok(Some(data));
        }
        self.job.get_range(key, r).await
    }

    async fn put(&self, key: &ObjKey, stored: Bytes) -> ObjResult<()> {
        self.job.put(key, stored).await
    }

    async fn has_many(&self, keys: &[ObjKey]) -> ObjResult<Vec<bool>> {
        let mut found = self
            .host
            .has_many(keys)
            .await
            .unwrap_or_else(|_| vec![false; keys.len()]);
        let missing: Vec<usize> = (0..keys.len()).filter(|&i| !found[i]).collect();
        if missing.is_empty() {
            return Ok(found);
        }
        let ask: Vec<ObjKey> = missing.iter().map(|&i| keys[i]).collect();
        for (i, present) in missing.into_iter().zip(self.job.has_many(&ask).await?) {
            found[i] = present;
        }
        Ok(found)
    }

    async fn delete(&self, key: &ObjKey) -> ObjResult<()> {
        self.job.delete(key).await
    }

    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, ObjResult<(ObjKey, SystemTime)>> {
        self.job.list(prefix)
    }
}

#[cfg(test)]
mod tests {
    use asc_core::id::FileId;
    use asc_store::bucket::BucketObjects;

    use super::*;

    fn opts(t: &Path) -> Options {
        Options {
            repo: "asc/dds".into(),
            rev: "HEAD".into(),
            base: Some("master".into()),
            dir: t.join("w"),
            mode: "plain".into(),
            sparse: vec![],
            event: "pull_request".into(),
            runner_temp: t.join("tmp"),
            host_cache: Some(t.join("host")),
            server: "https://asc.example".into(),
            token: "tok".into(),
            shim_dir: t.join("shim"),
        }
    }

    fn key_of(data: &[u8]) -> ObjKey {
        ObjKey::File(FileId::of(data))
    }

    fn tree_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_owned()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push((p.clone(), std::fs::read(&p).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn cache_mode_follows_the_event() {
        assert_eq!(CacheMode::from_event("push"), CacheMode::Trusted);
        assert_eq!(CacheMode::from_event("pull_request"), CacheMode::Pr);
        assert_eq!(CacheMode::from_event("pull_request_target"), CacheMode::Pr);
        assert_eq!(CacheMode::from_event(""), CacheMode::Pr);
        assert_eq!(CacheMode::Trusted.turbo_cache(), "local:rw,remote:rw");
        assert_eq!(CacheMode::Pr.turbo_cache(), "local:rw,remote:r");
    }

    #[test]
    fn exports_and_config_follow_the_spec() {
        let t = tempfile::tempdir().unwrap();
        let p = Plan::new(opts(t.path())).unwrap();
        let ex: std::collections::HashMap<_, _> = p.exports(Some("abc")).into_iter().collect();
        assert_eq!(ex["TURBO_SCM_BASE"], "abc");
        assert_eq!(ex["TURBO_SCM_HEAD"], "HEAD");
        assert_eq!(ex["TURBO_API"], "https://asc.example");
        assert_eq!(ex["TURBO_TOKEN"], "tok");
        assert_eq!(ex["TURBO_TEAM"], "asc.dds");
        assert_eq!(ex["TURBO_CACHE"], "local:rw,remote:r");
        assert_eq!(ex["ASC_TOKEN"], "tok");
        assert!(ex["ASC_HOME"].ends_with("asc-home"));
        let keys: Vec<String> = p.config_entries().into_iter().map(|e| e.0).collect();
        assert_eq!(
            keys,
            [
                "asc.cache.dir",
                "asc.cache.readOnly",
                "remote.origin.ascServer"
            ]
        );
        let env: Vec<String> = p.config_env().into_iter().map(|e| e.0).collect();
        assert_eq!(env[0], "GIT_CONFIG_COUNT");
        assert_eq!(env.len(), 7);
        let (e, pa) = (t.path().join("env"), t.path().join("path"));
        p.write_exports(&e, &pa, None).unwrap();
        let text = std::fs::read_to_string(&e).unwrap();
        assert!(text.contains("TURBO_CACHE=local:rw,remote:r\n"));
        assert!(!text.contains("TURBO_SCM_BASE"));
        assert!(std::fs::read_to_string(&pa).unwrap().contains("shim"));
    }

    #[test]
    fn inputs_are_validated() {
        let t = tempfile::tempdir().unwrap();
        let with = |f: &dyn Fn(&mut Options)| {
            let mut o = opts(t.path());
            f(&mut o);
            Plan::new(o).unwrap_err()
        };
        with(&|o| o.repo = "../x".into());
        with(&|o| o.repo = "a/b/c".into());
        with(&|o| o.rev = "--upload-pack=x".into());
        with(&|o| o.base = Some("a\nb".into()));
        with(&|o| o.mode = "virtual".into());
        with(&|o| o.token = String::new());
        with(&|o| o.server = "http://asc.example".into());
        with(&|o| o.server = "http://127.0.0.1.evil.example".into());
        let mut ok = opts(t.path());
        ok.server = "http://127.0.0.1:8080".into();
        assert!(Plan::new(ok).is_ok());
    }

    #[test]
    fn cold_host_cache_is_detected() {
        let t = tempfile::tempdir().unwrap();
        let p = Plan::new(opts(t.path())).unwrap();
        assert!(p.host_cache_is_cold());
        std::fs::create_dir_all(t.path().join("host/f/ab")).unwrap();
        std::fs::write(t.path().join("host/f/ab/x"), b"x").unwrap();
        assert!(!p.host_cache_is_cold());
    }

    #[test]
    fn grant_missing_names_path_and_child() {
        let composed = vec![("sdk".to_owned(), "asc/soma".to_owned())];
        let e = check_grants("asc/dds", &composed, &[]).unwrap_err();
        assert_eq!(e.to_string(), "PATH_POLICY: sdk needs read on asc/soma");
        let g = Grant {
            parent: "asc/dds".into(),
            child: "asc/soma".into(),
        };
        assert!(check_grants("asc/dds", &composed, std::slice::from_ref(&g)).is_ok());
        assert!(check_grants("asc/other", &composed, &[g]).is_err());
    }

    #[tokio::test]
    async fn read_only_host_cache_is_never_written() {
        let t = tempfile::tempdir().unwrap();
        let host_root = t.path().join("host");
        let host_rw = CacheDir::open(&host_root, t.path().join("htmp")).unwrap();
        let on_host = key_of(b"host data");
        host_rw
            .put(&on_host, encode_stored(b"host data"))
            .await
            .unwrap();
        let before = tree_files(&host_root);
        let remote = BucketObjects::memory();
        let far = key_of(b"far data");
        remote.put(&far, encode_stored(b"far data")).await.unwrap();
        let job = CacheDir::open(t.path().join("job"), t.path().join("jtmp")).unwrap();
        let stack = Layered::new(&host_root, job.clone(), remote).unwrap();

        assert_eq!(
            &stack.get(&on_host).await.unwrap().unwrap()[..],
            b"host data"
        );
        assert_eq!(&stack.get(&far).await.unwrap().unwrap()[..], b"far data");
        let new = key_of(b"new");
        stack.put(&new, encode_stored(b"new")).await.unwrap();
        stack.delete(&on_host).await.unwrap();
        assert_eq!(tree_files(&host_root), before);
        assert_eq!(job.has_many(&[far, new]).await.unwrap(), [true, true]);
        assert_eq!(stack.has_many(&[on_host, new]).await.unwrap(), [true, true]);
    }

    #[tokio::test]
    async fn corrupt_host_file_is_detected_and_refetched() {
        let t = tempfile::tempdir().unwrap();
        let host_root = t.path().join("host");
        let host_rw = CacheDir::open(&host_root, t.path().join("htmp")).unwrap();
        let key = key_of(b"good");
        host_rw.put(&key, encode_stored(b"good")).await.unwrap();
        let path = host_rw.content_path(FileId::of(b"good")).unwrap();
        std::fs::write(&path, b"evil").unwrap();
        let remote = BucketObjects::memory();
        remote.put(&key, encode_stored(b"good")).await.unwrap();
        let job = CacheDir::open(t.path().join("job"), t.path().join("jtmp")).unwrap();
        let stack = Layered::new(&host_root, job.clone(), remote).unwrap();
        assert_eq!(&stack.get(&key).await.unwrap().unwrap()[..], b"good");
        // The bad host copy is left alone; the job cache holds verified bytes.
        assert_eq!(std::fs::read(&path).unwrap(), b"evil");
        assert_eq!(&job.get(&key).await.unwrap().unwrap()[..], b"good");
        // With no good source the corrupt copy is never served.
        let empty = Layered::new(
            &host_root,
            CacheDir::open(t.path().join("job2"), t.path().join("jtmp2")).unwrap(),
            BucketObjects::memory(),
        )
        .unwrap();
        assert!(empty.get(&key).await.unwrap().is_none());
    }
}
