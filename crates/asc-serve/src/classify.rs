//! The `Classify` RPC (PLAN U67, SPEC §8.10).
//!
//! Classifies the files changed from `base` to `head` (or from their merge base). The class
//! rules (`.asc/review.toml`) and the `linguist-generated` attribute are read from the **base**
//! tree, never from the head being judged: a change cannot relax its own review. File content is
//! buffered through the [`TextBudget`](crate::TextBudget) before it is read.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use asc_core::id::{ChangesetId, FileId, TreeId};
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_engine::classify::{self, Admission, Gate, GateError, Rules};
use asc_engine::commit;
use asc_engine::graph;
use asc_engine::history::HistoryApi;
use asc_engine::tree;
use asc_rpc::server::{CallCtx, ClassifyHandler, MAX_STRING, wire};
use asc_store::objects;
use tokio::sync::Mutex;
use tonic::Status;

use crate::idmode::{Eng, IdCodec};
use crate::read::ReadHooks;
use crate::registry::{Extensions, Registry, RegistryError, RepoEntry};
use crate::{TextBudgetExhausted, TextBudgetSlot};

/// Largest `.asc/review.toml` and `.gitattributes` the server reads.
const MAX_CONFIG_FILE: u64 = 64 * 1024;
const MAX_ATTR_FILE: u64 = 1 << 20;
/// Per-directory attribute searches kept for one call.
const MAX_ATTR_DIRS: usize = 4096;
/// The review rules file, relative to the root.
const REVIEW_TOML: &str = ".asc/review.toml";

fn internal(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "internal error");
    Status::internal("internal error")
}

fn data_loss(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "stored data is damaged or missing");
    Status::data_loss("stored object is corrupt or missing")
}

fn store_status(e: objects::Error) -> Status {
    match e {
        objects::Error::Corrupt(_) | objects::Error::Codec(..) => data_loss("object", &e),
        e => internal("object store", &e),
    }
}

fn tree_status(e: tree::Error) -> Status {
    match e {
        tree::Error::MissingTree(_) | tree::Error::Codec(_) => data_loss("tree", &e),
        tree::Error::TooLarge(_) | tree::Error::OutputTooLarge(_) => {
            Status::resource_exhausted(e.to_string())
        }
        tree::Error::PathTooLong(_) | tree::Error::TooDeep => Status::out_of_range(e.to_string()),
        tree::Error::Store(e) => store_status(e),
        e => internal("tree", &e),
    }
}

fn commit_status(e: commit::Error) -> Status {
    match e {
        commit::Error::MissingChangeset(id) => {
            Status::not_found(format!("changeset {id} is missing"))
        }
        commit::Error::Store(e) => store_status(e),
        e => internal("changeset", &e),
    }
}

fn graph_status(e: graph::Error) -> Status {
    match e {
        graph::Error::Missing(id) => Status::not_found(format!("changeset {id} is missing")),
        graph::Error::Query(m) => Status::invalid_argument(m),
        graph::Error::Remote(m) => Status::invalid_argument(m),
        graph::Error::Engine(e) => commit_status(e),
        e => internal("history", &e),
    }
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

fn gate_status(e: GateError) -> Status {
    match e {
        GateError::Exhausted => {
            Status::resource_exhausted("text memory budget exhausted; retry shortly")
        }
        GateError::Failed(m) => internal("classify", &m),
    }
}

fn classify_status(e: classify::Error) -> Status {
    use asc_engine::diff::Error as D;
    match e {
        classify::Error::TooManyFiles => Status::resource_exhausted(e.to_string()),
        classify::Error::Gate(g) => gate_status(g),
        classify::Error::Diff(D::Tree(e)) => tree_status(e),
        classify::Error::Diff(D::Store(e)) => store_status(e),
        classify::Error::Diff(e @ (D::MissingFile(_) | D::Codec(_))) => data_loss("file", &e),
        classify::Error::Diff(e @ D::FileTooLarge(..)) => Status::resource_exhausted(e.to_string()),
        classify::Error::Diff(e) => internal("diff", &e),
    }
}

// ------------------------------------------------------------------------------ review.toml

/// The globs of `.asc/review.toml`; a key that is absent keeps its default.
#[derive(serde::Deserialize, Default)]
struct ReviewToml {
    docs: Option<Vec<String>>,
    test: Option<Vec<String>>,
    lockfile: Option<Vec<String>>,
}

/// Parses `review.toml`. A file that cannot be used gives no path classes at all (everything
/// not decided by content is `LOGIC`): failing open would let a typo relax review.
fn parse_rules(text: &[u8]) -> Rules {
    let parsed = std::str::from_utf8(text)
        .ok()
        .and_then(|t| toml::from_str::<ReviewToml>(t).ok())
        .and_then(|r| Rules::new(r.docs.as_deref(), r.test.as_deref(), r.lockfile.as_deref()).ok());
    parsed.unwrap_or_else(|| {
        tracing::warn!("{REVIEW_TOML} is not usable; no path classes apply");
        Rules::none()
    })
}

// ------------------------------------------------------------------------------ attributes

/// `linguist-generated` lookup at one revision: per directory `.ascattributes` else
/// `.gitattributes`, root to leaf, never merged (SPEC §2.4.1). An unreadable, linked or
/// oversized file reads as absent.
struct Attrs {
    engine: Arc<Eng>,
    root: TreeId,
    collection: gix_attributes::search::MetadataCollection,
    outcome: gix_attributes::search::Outcome,
    base: gix_attributes::Search,
    dirs: HashMap<Vec<u8>, Arc<gix_attributes::Search>>,
    any: bool,
}

impl Attrs {
    fn new(engine: Arc<Eng>, root: TreeId) -> Result<Self, Status> {
        let mut collection = gix_attributes::search::MetadataCollection::default();
        let mut buf = Vec::new();
        let base = gix_attributes::Search::new_globals(
            None::<std::path::PathBuf>,
            &mut buf,
            &mut collection,
        )
        .map_err(|e| internal("attributes", &e))?;
        Ok(Self {
            engine,
            root,
            collection,
            outcome: gix_attributes::search::Outcome::default(),
            base,
            dirs: HashMap::new(),
            any: false,
        })
    }

    async fn read_attr_file(&self, dir: &[u8], name: &str) -> Result<Option<bytes::Bytes>, Status> {
        let path = if dir.is_empty() {
            name.as_bytes().to_vec()
        } else {
            [dir, b"/", name.as_bytes()].concat()
        };
        let Ok(path) = RepoPath::new(path) else {
            return Ok(None);
        };
        let Some(e) = self
            .engine
            .lookup(self.root, &path)
            .await
            .map_err(tree_status)?
        else {
            return Ok(None);
        };
        if !matches!(e.kind, Kind::File | Kind::Exec) || e.size > MAX_ATTR_FILE {
            return Ok(None);
        }
        let data = self
            .engine
            .read_file(FileId::from_bytes(e.id), e.size)
            .await
            .map_err(|e| internal("attributes", &e))?;
        Ok((data.len() as u64 <= MAX_ATTR_FILE).then_some(data))
    }

    /// The search that applies to paths directly inside `dir` (`""` is the root).
    async fn search(&mut self, dir: &[u8]) -> Result<Arc<gix_attributes::Search>, Status> {
        if let Some(s) = self.dirs.get(dir) {
            return Ok(s.clone());
        }
        let mut prefixes: Vec<&[u8]> = vec![&dir[..0]];
        prefixes.extend(
            dir.iter()
                .enumerate()
                .filter(|(_, c)| **c == b'/')
                .map(|(i, _)| &dir[..i]),
        );
        if !dir.is_empty() {
            prefixes.push(dir);
        }
        let mut cur: Option<Arc<gix_attributes::Search>> = None;
        for d in prefixes {
            if let Some(s) = self.dirs.get(d) {
                cur = Some(s.clone());
                continue;
            }
            let mut search = match &cur {
                Some(p) => (**p).clone(),
                None => self.base.clone(),
            };
            let file = match self.read_attr_file(d, ".ascattributes").await? {
                Some(f) => Some(f),
                None => self.read_attr_file(d, ".gitattributes").await?,
            };
            if let Some(f) = file {
                self.any = true;
                let source = if d.is_empty() {
                    std::path::PathBuf::from(".gitattributes")
                } else {
                    std::path::PathBuf::from(format!(
                        "{}/.gitattributes",
                        String::from_utf8_lossy(d)
                    ))
                };
                search
                    .add_patterns_buffer(
                        &f,
                        source,
                        Some(std::path::Path::new("")),
                        &mut self.collection,
                        true,
                    )
                    .map_err(|e| internal("attributes", &e))?;
            }
            let search = Arc::new(search);
            if self.dirs.len() >= MAX_ATTR_DIRS {
                self.dirs.clear();
            }
            self.dirs.insert(d.to_vec(), search.clone());
            cur = Some(search);
        }
        cur.ok_or_else(|| internal("attributes", &"no search built"))
    }

    /// Whether `linguist-generated` is set (or `=true`) for `path`.
    async fn generated(&mut self, path: &[u8]) -> Result<bool, Status> {
        use bstr::ByteSlice;
        use gix_attributes::StateRef;
        let dir = path
            .iter()
            .rposition(|&c| c == b'/')
            .map_or(&[][..], |i| &path[..i]);
        let search = self.search(dir).await?;
        if !self.any {
            return Ok(false);
        }
        self.outcome
            .initialize_with_selection(&self.collection, ["linguist-generated"]);
        search.pattern_matching_relative_path(
            path.as_bstr(),
            gix_attributes::glob::pattern::Case::Sensitive,
            Some(false),
            &mut self.outcome,
        );
        Ok(self.outcome.iter_selected().any(|m| {
            m.assignment.name.as_str() == "linguist-generated"
                && match m.assignment.state {
                    StateRef::Set => true,
                    StateRef::Value(v) => v.as_bstr() == b"true".as_bstr(),
                    _ => false,
                }
        }))
    }
}

// ------------------------------------------------------------------------------ the RPC

/// What the engine asks of the server: attributes of the base tree and the text budget.
struct ServerGate {
    attrs: Mutex<Attrs>,
    budget: Arc<ReadHooks>,
}

#[async_trait]
impl Gate for ServerGate {
    async fn generated(&self, path: &RepoPath) -> Result<bool, GateError> {
        self.attrs
            .lock()
            .await
            .generated(path.as_bytes())
            .await
            .map_err(|s| GateError::Failed(s.message().to_owned()))
    }

    async fn admit(&self, bytes: u64) -> Result<Admission, GateError> {
        let slot: &TextBudgetSlot = &self.budget.text_budget;
        match slot.acquire(bytes).await {
            Ok(g) => Ok(Admission::holding(g)),
            Err(TextBudgetExhausted) => Err(GateError::Exhausted),
        }
    }
}

/// The `Classify` handler group.
pub struct ServerClassify {
    registry: Arc<Registry>,
    hooks: Arc<ReadHooks>,
}

impl ServerClassify {
    pub fn new(registry: Arc<Registry>, hooks: Arc<ReadHooks>) -> Self {
        Self { registry, hooks }
    }

    async fn rev(
        engine: &Eng,
        ids: &IdCodec,
        entry: &RepoEntry,
        spec: &str,
    ) -> Result<ChangesetId, Status> {
        if spec.len() > MAX_STRING {
            return Err(Status::invalid_argument("revision is too long"));
        }
        let spec = if spec.is_empty() { "HEAD" } else { spec };
        let spec = ids.rev_in(spec, &entry.default_branch).await?;
        engine
            .resolve(std::slice::from_ref(&spec))
            .await
            .map_err(graph_status)?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))
    }

    /// The rules in force at the base tree.
    async fn rules(&self, engine: &Eng, base_root: TreeId) -> Result<Rules, Status> {
        let path = RepoPath::new(REVIEW_TOML).map_err(|e| internal("review.toml path", &e))?;
        let Some(e) = engine.lookup(base_root, &path).await.map_err(tree_status)? else {
            return Ok(Rules::defaults());
        };
        if !matches!(e.kind, Kind::File | Kind::Exec) || e.size > MAX_CONFIG_FILE {
            return Ok(Rules::none());
        }
        let _guard = self
            .hooks
            .text_budget
            .acquire(e.size)
            .await
            .map_err(|_| gate_status(GateError::Exhausted))?;
        let data = engine
            .read_file(FileId::from_bytes(e.id), e.size)
            .await
            .map_err(|e| internal("review.toml", &e))?;
        if data.len() as u64 > MAX_CONFIG_FILE {
            return Ok(Rules::none());
        }
        Ok(parse_rules(&data))
    }
}

#[async_trait]
impl ClassifyHandler for ServerClassify {
    async fn classify(
        &self,
        ctx: &CallCtx,
        req: wire::ClassifyRequest,
    ) -> Result<wire::ClassifyResponse, Status> {
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry);
        let engine = entry.backend.engine().clone();
        let mut base = Self::rev(&engine, &ids, &entry, &req.base).await?;
        let head = Self::rev(&engine, &ids, &entry, &req.head).await?;
        if req.merge_base {
            base = *engine
                .merge_base(base, &[head], false)
                .await
                .map_err(graph_status)?
                .first()
                .ok_or_else(|| Status::failed_precondition("no merge base"))?;
        }
        let base_root = engine
            .get_changeset(base)
            .await
            .map_err(commit_status)?
            .root;
        let head_root = engine
            .get_changeset(head)
            .await
            .map_err(commit_status)?
            .root;
        let rules = self.rules(&engine, base_root).await?;
        let gate = ServerGate {
            attrs: Mutex::new(Attrs::new(engine.clone(), base_root)?),
            budget: self.hooks.clone(),
        };
        let got = engine
            .classify_trees(base_root, head_root, &rules, &gate)
            .await
            .map_err(classify_status)?;
        Ok(wire::ClassifyResponse {
            files: got
                .files
                .iter()
                .map(|f| wire::FileClass {
                    path: bytes::Bytes::copy_from_slice(f.path.as_bytes()),
                    class: f.class as i32,
                })
                .collect(),
            overall: got.overall as i32,
        })
    }
}

/// Registers the `Classify` group (one line in `register_all`).
pub fn register(ext: &Extensions, registry: &Arc<Registry>, hooks: &Arc<ReadHooks>) {
    ext.handlers
        .set_classify(Arc::new(ServerClassify::new(
            registry.clone(),
            hooks.clone(),
        )))
        .expect("the classify handler group is registered once");
}

#[cfg(test)]
mod tests {
    use asc_engine::classify::ChangeClass;
    use asc_rpc::IdMode;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::RepoName;

    use super::*;
    use crate::TextBudget;
    use crate::ingest::testutil::{commit, registry, repo};

    struct Fx {
        handler: ServerClassify,
        hooks: Arc<ReadHooks>,
        entry: Arc<RepoEntry>,
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let entry = repo(&reg).await;
        let hooks = Arc::new(ReadHooks::default());
        Fx {
            handler: ServerClassify::new(reg, hooks.clone()),
            hooks,
            entry,
        }
    }

    fn ctx() -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: "alice".into() },
            op_id: None,
            client: None,
        }
    }

    async fn classify(
        f: &Fx,
        base: ChangesetId,
        head: ChangesetId,
        merge_base: bool,
    ) -> Result<Vec<(String, i32)>, Status> {
        let r = f
            .handler
            .classify(
                &ctx(),
                wire::ClassifyRequest {
                    repo: Some(wire::RepoRef {
                        name: "o/r".into(),
                        id_mode: IdMode::Asc.to_wire(),
                    }),
                    base: base.to_string(),
                    head: head.to_string(),
                    merge_base,
                },
            )
            .await?;
        let mut v: Vec<(String, i32)> = r
            .files
            .iter()
            .map(|c| (String::from_utf8(c.path.to_vec()).unwrap(), c.class))
            .collect();
        v.push(("<overall>".into(), r.overall));
        Ok(v)
    }

    fn class_of(v: &[(String, i32)], path: &str) -> i32 {
        v.iter()
            .find(|(p, _)| p == path)
            .unwrap_or_else(|| panic!("{path} not classified"))
            .1
    }

    const WS: i32 = ChangeClass::WhitespaceOnly as i32;
    const CM: i32 = ChangeClass::CommentOnly as i32;
    const DOCS: i32 = ChangeClass::Docs as i32;
    const GEN: i32 = ChangeClass::Generated as i32;
    const LOCK: i32 = ChangeClass::Lockfile as i32;
    const TEST: i32 = ChangeClass::Test as i32;
    const LOGIC: i32 = ChangeClass::Logic as i32;

    #[tokio::test]
    async fn classifies_each_file_and_reports_the_maximum() {
        let f = fx().await;
        let a = commit(
            &f.entry,
            &[],
            &[
                ("src/ws.rs", b"fn a(){}"),
                ("src/cm.rs", b"fn a() {}"),
                ("src/logic.rs", b"fn a() {}"),
                ("README.md", b"one"),
                ("tests/t.rs", b"fn t() {}"),
                ("Cargo.lock", b"1"),
            ],
        )
        .await;
        let b = commit(
            &f.entry,
            &[a],
            &[
                ("src/ws.rs", b"fn a() {\n}"),
                ("src/cm.rs", b"fn a() {} // c"),
                ("src/logic.rs", b"fn b() {}"),
                ("README.md", b"two"),
                ("tests/t.rs", b"fn t() { 1; }"),
                ("Cargo.lock", b"2"),
            ],
        )
        .await;
        let v = classify(&f, a, b, false).await.unwrap();
        assert_eq!(class_of(&v, "src/ws.rs"), WS);
        assert_eq!(class_of(&v, "src/cm.rs"), CM);
        assert_eq!(class_of(&v, "src/logic.rs"), LOGIC);
        assert_eq!(class_of(&v, "README.md"), DOCS);
        assert_eq!(class_of(&v, "tests/t.rs"), TEST);
        assert_eq!(class_of(&v, "Cargo.lock"), LOCK);
        assert_eq!(class_of(&v, "<overall>"), LOGIC);
        // Only trivial files: the overall class is the highest of them.
        let c = commit(
            &f.entry,
            &[b],
            &[("src/ws.rs", b"fn a() {\n  }"), ("README.md", b"three")],
        )
        .await;
        let v = classify(&f, b, c, false).await.unwrap();
        assert_eq!(class_of(&v, "<overall>"), DOCS);
        // No change at all.
        let v = classify(&f, c, c, false).await.unwrap();
        assert_eq!(
            v,
            vec![("<overall>".to_owned(), ChangeClass::RenameOnly as i32)]
        );
    }

    #[tokio::test]
    async fn merge_base_compares_from_the_fork_point() {
        let f = fx().await;
        let base = commit(
            &f.entry,
            &[],
            &[("a.rs", b"fn a() {}"), ("b.rs", b"fn b() {}")],
        )
        .await;
        let left = commit(&f.entry, &[base], &[("a.rs", b"fn a() { 1 }")]).await;
        let right = commit(&f.entry, &[base], &[("b.rs", b"fn b() {} // c")]).await;
        let v = classify(&f, left, right, true).await.unwrap();
        assert_eq!(class_of(&v, "b.rs"), CM);
        assert!(v.iter().all(|(p, _)| p != "a.rs"));
        let v = classify(&f, left, right, false).await.unwrap();
        assert_eq!(class_of(&v, "a.rs"), LOGIC);
    }

    #[tokio::test]
    async fn the_head_cannot_relax_its_own_review() {
        let f = fx().await;
        let a = commit(&f.entry, &[], &[("src/x.rs", b"fn a() {}")]).await;
        // The head claims everything is documentation and generated.
        let b = commit(
            &f.entry,
            &[a],
            &[
                ("src/x.rs", b"fn b() {}"),
                (".asc/review.toml", b"docs = [\"**\"]\n"),
                (".gitattributes", b"src/** linguist-generated\n"),
            ],
        )
        .await;
        let v = classify(&f, a, b, false).await.unwrap();
        assert_eq!(class_of(&v, "src/x.rs"), LOGIC);
        assert_eq!(class_of(&v, ".asc/review.toml"), LOGIC);
        assert_eq!(class_of(&v, ".gitattributes"), LOGIC);
    }

    #[tokio::test]
    async fn base_rules_and_attributes_apply() {
        let f = fx().await;
        let a = commit(
            &f.entry,
            &[],
            &[
                (".asc/review.toml", b"docs = [\"manual/**\"]\ntest = []\n"),
                (
                    ".gitattributes",
                    b"gen/** linguist-generated\nbuilt/** linguist-generated=false\n",
                ),
                ("manual/a.txt", b"1"),
                ("README.md", b"1"),
                ("tests/t.rs", b"fn t() {}"),
                ("gen/out.txt", b"1"),
                ("built/out.txt", b"1"),
            ],
        )
        .await;
        let b = commit(
            &f.entry,
            &[a],
            &[
                ("manual/a.txt", b"2"),
                ("README.md", b"2"),
                ("tests/t.rs", b"fn t() { 1; }"),
                ("gen/out.txt", b"2"),
                ("built/out.txt", b"2"),
                ("gen/new.txt", b"n"),
            ],
        )
        .await;
        let v = classify(&f, a, b, false).await.unwrap();
        assert_eq!(class_of(&v, "manual/a.txt"), DOCS);
        // The configured globs replace the defaults for the classes they name.
        assert_eq!(class_of(&v, "README.md"), LOGIC);
        assert_eq!(class_of(&v, "tests/t.rs"), LOGIC);
        assert_eq!(class_of(&v, "gen/out.txt"), GEN);
        assert_eq!(class_of(&v, "gen/new.txt"), GEN);
        assert_eq!(class_of(&v, "built/out.txt"), LOGIC);
    }

    #[tokio::test]
    async fn an_unusable_review_toml_grants_no_path_classes() {
        let f = fx().await;
        for bad in [
            &b"docs = ["[..],
            &b"docs = [\"[\"]\n"[..],
            &[0xff, 0xfe][..],
        ] {
            let a = commit(
                &f.entry,
                &[],
                &[(".asc/review.toml", bad), ("README.md", b"1")],
            )
            .await;
            let b = commit(&f.entry, &[a], &[("README.md", b"2")]).await;
            let v = classify(&f, a, b, false).await.unwrap();
            assert_eq!(class_of(&v, "README.md"), LOGIC);
        }
        // An oversized file is refused too.
        let big = vec![b'#'; (MAX_CONFIG_FILE + 1) as usize];
        let a = commit(
            &f.entry,
            &[],
            &[(".asc/review.toml", &big[..]), ("README.md", b"1")],
        )
        .await;
        let b = commit(&f.entry, &[a], &[("README.md", b"2")]).await;
        let v = classify(&f, a, b, false).await.unwrap();
        assert_eq!(class_of(&v, "README.md"), LOGIC);
    }

    #[tokio::test]
    async fn content_goes_through_the_text_budget() {
        struct Count(std::sync::atomic::AtomicU64);
        #[async_trait]
        impl TextBudget for Count {
            async fn acquire(&self, bytes: u64) -> Result<crate::TextGuard, TextBudgetExhausted> {
                self.0.fetch_add(bytes, std::sync::atomic::Ordering::SeqCst);
                Ok(crate::TextGuard::unlimited())
            }
        }
        struct Never;
        #[async_trait]
        impl TextBudget for Never {
            async fn acquire(&self, _: u64) -> Result<crate::TextGuard, TextBudgetExhausted> {
                Err(TextBudgetExhausted)
            }
        }
        let f = fx().await;
        let a = commit(&f.entry, &[], &[("a.rs", b"fn a() {}")]).await;
        let b = commit(&f.entry, &[a], &[("a.rs", b"fn a() {} // c")]).await;
        let count = Arc::new(Count(Default::default()));
        f.hooks.text_budget.set(count.clone()).ok().unwrap();
        let v = classify(&f, a, b, false).await.unwrap();
        assert_eq!(class_of(&v, "a.rs"), CM);
        assert_eq!(count.0.load(std::sync::atomic::Ordering::SeqCst), 9 + 14);

        let g = fx().await;
        g.hooks.text_budget.set(Arc::new(Never)).ok().unwrap();
        let a = commit(&g.entry, &[], &[("a.rs", b"fn a() {}")]).await;
        let b = commit(&g.entry, &[a], &[("a.rs", b"fn a() {} // c")]).await;
        let e = classify(&g, a, b, false).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
    }

    #[tokio::test]
    async fn bad_revisions_are_client_errors() {
        let f = fx().await;
        let a = commit(&f.entry, &[], &[("a.rs", b"x")]).await;
        for (base, head) in [
            ("nope".to_owned(), a.to_string()),
            (a.to_string(), "x".repeat(MAX_STRING + 1)),
        ] {
            let e = f
                .handler
                .classify(
                    &ctx(),
                    wire::ClassifyRequest {
                        repo: Some(wire::RepoRef {
                            name: "o/r".into(),
                            id_mode: IdMode::Asc.to_wire(),
                        }),
                        base,
                        head,
                        merge_base: false,
                    },
                )
                .await
                .unwrap_err();
            assert!(
                matches!(
                    e.code(),
                    tonic::Code::NotFound | tonic::Code::InvalidArgument
                ),
                "{e:?}"
            );
        }
    }
}
