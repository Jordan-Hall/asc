//! The production [`SeedSource`] (SPEC §12.3): the repositories of the registry, cut at the tip
//! of each default branch.
//!
//! A seed holds what a fresh clone would otherwise download first: the changesets of the tip's
//! history window, every tree of the tip, and the hot files, which are every manifest or lockfile
//! a detector claims (§12A.2) and the files the `.asc/prefetch` globs match (default
//! `**/*.{ts,tsx,rs,py,cs,toml,json}`, §10.2 item 5a). Files over the inline limit are chunked;
//! their chunk lists cannot be verified from the key (§12.3), so a seed leaves them out and the
//! client reads them on demand. The objects are staged in a private cache directory, which the
//! task removes once it has packed it.
//!
//! A seed is handed to every reader of the repo, so a repo with any path rule (§13.4) gets none:
//! the trees and hot files would reveal what the rules hide. Without the path rules store the
//! source seeds nothing (fail closed). [`RegistrySeeds`] is also the [`SeedGate`]: at each request
//! the repo must still exist with the namespace the seed was cut from and have no path rules.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use asc_core::codec::decode_envelope;
use asc_core::content::CHUNK_THRESHOLD;
use asc_core::id::{ChangesetId, FileId, RefTarget, TreeId};
use asc_core::object::{Changeset, Kind, Tree};
use asc_core::path::{RefName, RepoPath};
use asc_graph::detect::{Claim, Detector};
use asc_rpc::server::RepoName;
use asc_store::cachedir::CacheDir;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use async_trait::async_trait;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use sqlx::Row;

use crate::pathrules::PathRulesSlot;
use crate::registry::Registry;
use crate::seeds::{SeedGate, SeedPlan, SeedSource, SeedsError};
use crate::turbo::Slug;

/// The prefetch globs of a repository without `.asc/prefetch`.
pub const DEFAULT_PREFETCH: &str = "**/*.{ts,tsx,rs,py,cs,toml,json}";
/// Changesets of history a seed carries below the tip (the client's default window).
pub const WINDOW: u32 = asc_engine::history::DEFAULT_WINDOW;
/// Hot file bytes one seed may carry; past it the remaining files are left to the clone.
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Trees one seed may carry (a guard against a pathological repository).
pub const MAX_TREES: usize = 1_000_000;
/// The largest `.asc/prefetch` file read.
const MAX_PREFETCH_BYTES: u64 = 64 * 1024;

static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Every repository of the registry, seeded from its default branch.
pub struct RegistrySeeds {
    registry: Arc<Registry>,
    rules: Arc<PathRulesSlot>,
    /// Staging directories are made here.
    tmp: PathBuf,
    window: u32,
    max_file_bytes: u64,
}

impl RegistrySeeds {
    /// `rules` is the server's path rules slot; a repo with rules is never seeded.
    pub fn new(registry: Arc<Registry>, rules: Arc<PathRulesSlot>, tmp: PathBuf) -> Self {
        Self {
            registry,
            rules,
            tmp,
            window: WINDOW,
            max_file_bytes: MAX_FILE_BYTES,
        }
    }

    /// Changes the history window and the hot file budget (tests).
    #[must_use]
    pub fn with_limits(mut self, window: u32, max_file_bytes: u64) -> Self {
        self.window = window.max(1);
        self.max_file_bytes = max_file_bytes;
        self
    }

    /// Whether `repo` has path rules; an unset store counts as rules (fail closed).
    async fn has_rules(&self, repo: &str) -> Result<bool, SeedsError> {
        let Some(hub) = self.rules.get() else {
            return Ok(true);
        };
        hub.get(repo)
            .await
            .map(|set| !set.rules.is_empty())
            .map_err(|e| SeedsError::Source(format!("path rules: {e:?}")))
    }

    async fn tip_of(&self, repo: &Slug) -> Result<(RepoName, ChangesetId, u64), SeedsError> {
        let src = |e: &dyn std::fmt::Display| SeedsError::Source(e.to_string());
        let name = RepoName::parse(&repo.repo_name()).map_err(|e| src(&e))?;
        if self.has_rules(name.as_str()).await? {
            return Err(SeedsError::Source(
                "the repository has path rules (or none are loaded): no seed".into(),
            ));
        }
        let entry = self.registry.open(&name).await.map_err(|e| src(&e))?;
        let branch = format!("refs/heads/{}", entry.default_branch);
        let ref_name = RefName::new(branch.as_bytes()).map_err(|e| src(&e))?;
        let tip = match self.registry.refs().get(entry.key, &ref_name).await {
            Ok(Some(r)) => match r.target {
                RefTarget::Changeset(id) => id,
                RefTarget::Tag(_) => {
                    return Err(SeedsError::Source(format!("{branch} names a tag")));
                }
            },
            Ok(None) => return Err(SeedsError::Source(format!("{branch} does not exist"))),
            Err(e) => return Err(src(&e)),
        };
        // Every move of the branch is one row of the ref log: the 50-changeset trigger counts them.
        let landed: i64 =
            sqlx::query("SELECT COUNT(*) AS n FROM ref_log WHERE repo_id = $1 AND name = $2")
                .bind(entry.key.0)
                .bind(&branch)
                .fetch_one(self.registry.pool())
                .await
                .and_then(|r| r.try_get("n"))
                .map_err(|e| src(&e))?;
        Ok((name, tip, u64::try_from(landed).unwrap_or(0)))
    }
}

#[async_trait]
impl SeedSource for RegistrySeeds {
    async fn repos(&self) -> Vec<Slug> {
        let rows = match sqlx::query("SELECT name FROM repos WHERE ready ORDER BY name")
            .fetch_all(self.registry.pool())
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(event = "seeds.repos", error = %e);
                return Vec::new();
            }
        };
        rows.iter()
            .filter_map(|r| r.try_get::<String, _>("name").ok())
            // A seed directory is `owner.repo`, so an owner with a dot cannot have one.
            .filter_map(|n| {
                let (owner, repo) = n.split_once('/')?;
                Slug::parse(&format!("{owner}.{repo}")).ok()
            })
            .collect()
    }

    async fn tip(&self, repo: &Slug) -> Result<(String, u64), SeedsError> {
        let (_, tip, landed) = self.tip_of(repo).await?;
        Ok((tip.to_string(), landed))
    }

    async fn plan(&self, repo: &Slug) -> Result<SeedPlan, SeedsError> {
        let (name, tip, landed) = self.tip_of(repo).await?;
        let entry = self
            .registry
            .open(&name)
            .await
            .map_err(|e| SeedsError::Source(e.to_string()))?;
        let seq = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = self
            .tmp
            .join(format!("stage-{}-{}-{seq}", repo.dir(), std::process::id()));
        let stage = CacheDir::open(root.join("cache"), root.join("tmp"))
            .map_err(|e| SeedsError::Source(e.to_string()))?;
        let objects: &dyn Objects = entry.objects.as_ref();
        let built = Stager {
            from: objects,
            to: &stage,
            files: Vec::new(),
        }
        .run(tip, self.window, self.max_file_bytes)
        .await;
        match built {
            Ok(files) => Ok(SeedPlan {
                changeset: tip.to_string(),
                repo_id: entry.ns.hex(),
                landed,
                cache: root.join("cache"),
                files,
                staged: Some(root),
            }),
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&root).await;
                Err(e)
            }
        }
    }
}

#[async_trait]
impl SeedGate for RegistrySeeds {
    async fn serve(&self, repo: &Slug, repo_id: &str) -> Result<bool, SeedsError> {
        let name = repo.repo_name();
        // Read the row, not the registry's cache: a repo deleted a moment ago is gone now.
        let row = sqlx::query("SELECT ns FROM repos WHERE name = $1 AND ready")
            .bind(&name)
            .fetch_optional(self.registry.pool())
            .await
            .map_err(|e| SeedsError::Source(e.to_string()))?;
        let Some(row) = row else {
            return Ok(false);
        };
        let ns: Vec<u8> = row
            .try_get("ns")
            .map_err(|e| SeedsError::Source(e.to_string()))?;
        let hex: String = ns.iter().map(|b| format!("{b:02x}")).collect();
        if hex != repo_id {
            return Ok(false);
        }
        Ok(!self.has_rules(&name).await?)
    }
}

/// Copies the objects of one seed from the repository into the staging cache.
struct Stager<'a> {
    from: &'a dyn Objects,
    to: &'a CacheDir,
    files: Vec<String>,
}

fn store_err(e: impl std::fmt::Display) -> SeedsError {
    SeedsError::Source(e.to_string())
}

impl Stager<'_> {
    /// Copies `key` and returns its decoded bytes.
    async fn copy(&mut self, key: ObjKey) -> Result<bytes::Bytes, SeedsError> {
        let missing = || SeedsError::Source(format!("object {} is missing", key.path()));
        let data = self
            .from
            .get(&key)
            .await
            .map_err(store_err)?
            .ok_or_else(missing)?;
        let stored = self
            .from
            .get_stored(&key)
            .await
            .map_err(store_err)?
            .ok_or_else(missing)?;
        self.to.put(&key, stored).await.map_err(store_err)?;
        self.files.push(key.path());
        Ok(data)
    }

    async fn run(
        mut self,
        tip: ChangesetId,
        window: u32,
        max_file_bytes: u64,
    ) -> Result<Vec<String>, SeedsError> {
        // The history window: the newest `window` changesets reachable from the tip.
        let mut root = None;
        let mut seen = HashSet::from([tip]);
        let mut queue = VecDeque::from([tip]);
        let mut taken = 0u32;
        while let Some(id) = queue.pop_front() {
            if taken >= window {
                break;
            }
            let bytes = self.copy(ObjKey::Changeset(id)).await?;
            let cs: Changeset = decode_envelope(&bytes).map_err(store_err)?;
            taken += 1;
            if id == tip {
                root = Some(cs.root);
            }
            for p in cs.parents {
                if seen.insert(p) {
                    queue.push_back(p);
                }
            }
        }
        let root = root.ok_or_else(|| SeedsError::Source("the tip was not read".into()))?;

        // Every tree of the tip, and the files the hot rules pick.
        let mut trees = VecDeque::from([(String::new(), TreeId::from_bytes(*root.as_bytes()))]);
        let mut seen_trees = HashSet::new();
        let mut candidates: Vec<(String, FileId, u64)> = Vec::new();
        let mut prefetch: Option<FileId> = None;
        while let Some((dir, id)) = trees.pop_front() {
            if !seen_trees.insert(id) {
                continue;
            }
            if seen_trees.len() > MAX_TREES {
                return Err(SeedsError::Source(format!(
                    "more than {MAX_TREES} trees at the tip"
                )));
            }
            let bytes = self.copy(ObjKey::Tree(id)).await?;
            let tree: Tree = decode_envelope(&bytes).map_err(store_err)?;
            for e in tree.entries() {
                let name = String::from_utf8_lossy(e.name.as_bytes());
                let path = if dir.is_empty() {
                    name.into_owned()
                } else {
                    format!("{dir}/{name}")
                };
                match e.kind {
                    Kind::Dir => trees.push_back((path, TreeId::from_bytes(e.id))),
                    Kind::File | Kind::Exec => {
                        let file = FileId::from_bytes(e.id);
                        if path == ".asc/prefetch" {
                            prefetch = Some(file).filter(|_| e.size <= MAX_PREFETCH_BYTES);
                        }
                        candidates.push((path, file, e.size));
                    }
                    Kind::Symlink | Kind::Gitlink => {}
                }
            }
        }

        let globs = match prefetch {
            Some(id) => match self.from.get(&ObjKey::File(id)).await.map_err(store_err)? {
                Some(b) => globs(&String::from_utf8_lossy(&b)),
                None => globs(""),
            },
            None => globs(""),
        };
        let detectors = detectors();
        let mut budget = max_file_bytes;
        let mut copied = HashSet::new();
        // Manifests first: they are what a build reads before anything else.
        candidates.sort_by_key(|(p, _, _)| !claimed(&detectors, p));
        for (path, file, size) in candidates {
            if size > CHUNK_THRESHOLD || size > budget || copied.contains(&file) {
                continue;
            }
            if !(claimed(&detectors, &path) || globs.is_match(&path)) {
                continue;
            }
            self.copy(ObjKey::File(file)).await?;
            copied.insert(file);
            budget -= size;
        }
        Ok(self.files)
    }
}

/// The production detectors (SPEC §12A.2).
fn detectors() -> Vec<&'static dyn Detector> {
    use asc_graph::detect::{dotnet::Dotnet, node::Node, python::Python, rust::Rust};
    vec![&Rust, &Node, &Python, &Dotnet]
}

fn claimed(detectors: &[&dyn Detector], path: &str) -> bool {
    let Ok(p) = RepoPath::new(path.as_bytes()) else {
        return false;
    };
    detectors
        .iter()
        .any(|d| matches!(d.claim(&p), Some(Claim::Manifest | Claim::Lockfile)))
}

/// The globs of a `.asc/prefetch` file: one per line, `#` starts a comment, blank lines are
/// skipped. An empty or unreadable file means the default.
pub fn globs(text: &str) -> GlobSet {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let build = |pats: &[&str]| {
        let mut b = GlobSetBuilder::new();
        for p in pats {
            match GlobBuilder::new(p).literal_separator(true).build() {
                Ok(g) => {
                    b.add(g);
                }
                Err(e) => tracing::warn!(event = "seeds.prefetch", glob = %p, error = %e),
            }
        }
        b.build().unwrap_or_else(|_| GlobSet::empty())
    };
    if lines.is_empty() {
        build(&[DEFAULT_PREFETCH])
    } else {
        build(&lines)
    }
}

#[cfg(test)]
mod tests {
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate};
    use object_store::memory::InMemory;
    use object_store::path::Path as StorePath;

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::seeds::{SeedStore, SeedTask};

    async fn move_master(reg: &Registry, e: &crate::registry::RepoEntry, to: ChangesetId) {
        let rn = RefName::new(b"refs/heads/master".to_vec()).unwrap();
        let cur = reg.refs().get(e.key, &rn).await.unwrap();
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: rn,
                expect: cur.map_or(Expect::Absent, |c| Expect::Value(c.target)),
                new: Some(RefTarget::Changeset(to)),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: uuid::Uuid::new_v4(),
        };
        reg.refs().update(e.key, &txn).await.unwrap();
    }

    /// A path rules slot with an empty store: every repo may be seeded.
    async fn no_rules() -> (Arc<PathRulesSlot>, Arc<crate::pathrules::PathRulesHub>) {
        let p = crate::migrate::connect("sqlite::memory:", 1).await.unwrap();
        crate::migrate::migrate(&p).await.unwrap();
        let hub = crate::pathrules::PathRulesHub::new(
            p,
            Arc::new(crate::settings::testkit::Recorder::default()),
        );
        let slot = Arc::new(PathRulesSlot::default());
        assert!(slot.set(hub.clone()));
        (slot, hub)
    }

    fn class(files: &[String], c: &str) -> usize {
        files
            .iter()
            .filter(|f| f.starts_with(&format!("{c}/")))
            .count()
    }

    #[tokio::test]
    async fn a_plan_holds_the_window_every_tree_and_the_hot_files_only() {
        let reg = registry().await;
        let e = repo(&reg).await;
        let tmp = tempfile::tempdir().unwrap();
        let (rules, _) = no_rules().await;
        let src = RegistrySeeds::new(reg.clone(), rules, tmp.path().join("stage"))
            .with_limits(2, 1 << 20);
        let slug = Slug::parse("o.r").unwrap();
        assert_eq!(src.repos().await, std::slice::from_ref(&slug));
        // No default branch yet: nothing to seed, and the task skips the repo quietly.
        assert!(src.tip(&slug).await.is_err());

        let one = commit(
            &e,
            &[],
            &[(
                "Cargo.toml",
                b"[package]
",
            )],
        )
        .await;
        move_master(&reg, &e, one).await;
        let two = commit(
            &e,
            &[one],
            &[(
                "src/main.rs",
                b"fn main() {}
",
            )],
        )
        .await;
        move_master(&reg, &e, two).await;
        let three = commit(
            &e,
            &[two],
            &[
                ("assets/logo.png", b"PNG-bytes"),
                (
                    "docs/guide.md",
                    b"# guide
",
                ),
            ],
        )
        .await;
        move_master(&reg, &e, three).await;
        assert_eq!(src.tip(&slug).await.unwrap(), (three.to_string(), 3));

        let plan = src.plan(&slug).await.unwrap();
        assert_eq!(plan.changeset, three.to_string());
        // The window is two changesets: the tip and its parent, not the root commit.
        assert_eq!(class(&plan.files, "c"), 2);
        assert!(plan.files.contains(&ObjKey::Changeset(three).path()));
        assert!(!plan.files.contains(&ObjKey::Changeset(one).path()));
        // The root, `src`, `assets` and `docs`.
        assert_eq!(class(&plan.files, "t"), 4);
        let file = |b: &[u8]| ObjKey::File(FileId::of(b)).path();
        assert!(
            plan.files.contains(&file(
                b"[package]
"
            )),
            "a claimed manifest"
        );
        assert!(
            plan.files.contains(&file(
                b"fn main() {}
"
            )),
            "a default prefetch glob"
        );
        assert!(!plan.files.contains(&file(b"PNG-bytes")));
        assert!(!plan.files.contains(&file(
            b"# guide
"
        )));
        let staged = plan.staged.clone().unwrap();
        assert!(staged.starts_with(tmp.path().join("stage")));
        // This plan was made by hand; the task makes and removes its own.
        std::fs::remove_dir_all(&staged).unwrap();

        // The task packs and publishes it, then removes the staging directory.
        let mem = Arc::new(InMemory::new());
        let store = Arc::new(SeedStore::new(mem.clone(), StorePath::from("root")));
        let task = SeedTask::new(Arc::new(src), store.clone(), tmp.path().join("out"));
        assert_eq!(task.tick(1_000).await, ["o/r"]);
        assert_eq!(
            store.latest(&slug).await.unwrap(),
            Some((three.to_string(), e.ns.hex()))
        );
        // What a client downloads passes its verifying unpack and installs every planned file.
        let key = StorePath::from(format!("root/seeds/o.r/{three}.tar.zst"));
        let bytes = object_store::ObjectStoreExt::get(mem.as_ref(), &key)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let file = tmp.path().join("seed.tar.zst");
        std::fs::write(&file, &bytes).unwrap();
        let got = asc_store::seed::unpack_str(
            &file.to_string_lossy(),
            &tmp.path().join("client"),
            Some("o/r"),
            asc_store::seed::Origin::Explicit,
        )
        .await
        .unwrap();
        assert_eq!(got.manifest.changeset, three.to_string());
        assert_eq!(got.installed as usize, plan.files.len());
        let leftover: Vec<_> = std::fs::read_dir(tmp.path().join("stage"))
            .map(|d| d.flatten().collect())
            .unwrap_or_default();
        assert!(leftover.is_empty(), "{leftover:?}");
        // An unchanged tip is not cut again.
        assert!(task.tick(1_000 + 7200).await.is_empty());
    }

    #[tokio::test]
    async fn a_prefetch_file_replaces_the_default_globs() {
        let reg = registry().await;
        let e = repo(&reg).await;
        let tmp = tempfile::tempdir().unwrap();
        let cs = commit(
            &e,
            &[],
            &[
                (
                    ".asc/prefetch",
                    b"# hot assets
assets/*.png
",
                ),
                ("assets/logo.png", b"PNG-bytes"),
                (
                    "src/main.rs",
                    b"fn main() {}
",
                ),
                ("package.json", b"{}"),
            ],
        )
        .await;
        move_master(&reg, &e, cs).await;
        let (rules, _) = no_rules().await;
        let src = RegistrySeeds::new(reg.clone(), rules.clone(), tmp.path().to_owned());
        let plan = src.plan(&Slug::parse("o.r").unwrap()).await.unwrap();
        let file = |b: &[u8]| ObjKey::File(FileId::of(b)).path();
        assert!(plan.files.contains(&file(b"PNG-bytes")));
        assert!(
            plan.files.contains(&file(b"{}")),
            "manifests are always hot"
        );
        assert!(!plan.files.contains(&file(
            b"fn main() {}
"
        )));
        // The budget leaves out what does not fit.
        let tight = RegistrySeeds::new(reg.clone(), rules, tmp.path().to_owned()).with_limits(1, 2);
        let plan = tight.plan(&Slug::parse("o.r").unwrap()).await.unwrap();
        assert!(plan.files.contains(&file(b"{}")));
        assert!(!plan.files.contains(&file(b"PNG-bytes")));
    }

    #[tokio::test]
    async fn a_repo_with_path_rules_or_no_rules_store_gets_no_seed() {
        let reg = registry().await;
        let e = repo(&reg).await;
        let tmp = tempfile::tempdir().unwrap();
        let cs = commit(&e, &[], &[("src/main.rs", b"fn main() {}\n")]).await;
        move_master(&reg, &e, cs).await;
        let slug = Slug::parse("o.r").unwrap();
        // No path rules store: fail closed.
        let unset = RegistrySeeds::new(
            reg.clone(),
            Arc::new(PathRulesSlot::default()),
            tmp.path().to_owned(),
        );
        assert!(unset.tip(&slug).await.is_err());
        assert!(unset.plan(&slug).await.is_err());
        assert!(!unset.serve(&slug, &e.ns.hex()).await.unwrap());
        // An empty store: seeded and served.
        let (rules, hub) = no_rules().await;
        let src = RegistrySeeds::new(reg.clone(), rules, tmp.path().to_owned());
        assert_eq!(src.tip(&slug).await.unwrap().0, cs.to_string());
        assert!(src.serve(&slug, &e.ns.hex()).await.unwrap());
        // One rule anywhere in the repo: neither cut nor served.
        let rule = [crate::pathrules::parse_rule("secret", "*", "none").unwrap()];
        hub.set("O/R", &rule, 0, "pat:admin").await.unwrap();
        let err = src.plan(&slug).await.unwrap_err().to_string();
        assert!(err.contains("path rules"), "{err}");
        assert!(!src.serve(&slug, &e.ns.hex()).await.unwrap());
    }

    #[tokio::test]
    async fn the_gate_refuses_a_seed_of_a_deleted_or_replaced_repo() {
        let reg = registry().await;
        let e = repo(&reg).await;
        let tmp = tempfile::tempdir().unwrap();
        let (rules, _) = no_rules().await;
        let src = RegistrySeeds::new(reg.clone(), rules, tmp.path().to_owned());
        let slug = Slug::parse("o.r").unwrap();
        let ns = e.ns.hex();
        assert!(src.serve(&slug, &ns).await.unwrap());
        assert!(
            !src.serve(&slug, &"0".repeat(32)).await.unwrap(),
            "another namespace"
        );
        assert!(
            !src.serve(&Slug::parse("o.gone").unwrap(), &ns)
                .await
                .unwrap()
        );
        // Renamed away: the old name no longer has the repo.
        sqlx::query("UPDATE repos SET name = 'o/moved' WHERE name = 'o/r'")
            .execute(reg.pool())
            .await
            .unwrap();
        assert!(!src.serve(&slug, &ns).await.unwrap());
        assert!(
            src.serve(&Slug::parse("o.moved").unwrap(), &ns)
                .await
                .unwrap()
        );
        // A new repo under the old name has a new namespace: the old seed stays refused.
        sqlx::query("UPDATE repos SET name = 'o/r', ns = $1 WHERE name = 'o/moved'")
            .bind(vec![7u8; 16])
            .execute(reg.pool())
            .await
            .unwrap();
        assert!(!src.serve(&slug, &ns).await.unwrap());
        assert!(src.serve(&slug, &"07".repeat(16)).await.unwrap());
    }

    #[test]
    fn prefetch_globs_default_and_custom() {
        let d = globs("");
        assert!(d.is_match("src/main.rs"));
        assert!(d.is_match("web/app/page.tsx"));
        assert!(d.is_match("Cargo.toml"));
        assert!(!d.is_match("assets/logo.png"));
        let c = globs("# only docs\n\ndocs/**/*.md\n");
        assert!(c.is_match("docs/a/b.md"));
        assert!(!c.is_match("src/main.rs"));
        assert!(!c.is_match("docs.md"));
    }

    #[test]
    fn manifests_and_lockfiles_are_claimed() {
        let d = detectors();
        for p in [
            "Cargo.toml",
            "crates/x/Cargo.toml",
            "package.json",
            "Cargo.lock",
        ] {
            assert!(claimed(&d, p), "{p}");
        }
        assert!(!claimed(&d, "src/main.rs"));
    }
}
