//! The `RepoConfig` RPC (PLAN C30d, SPEC §20.6): the autolinks of `.asc/review.toml` at a
//! revision.
//!
//! The file is untrusted repository content. It is read only when it is a regular file of at
//! most [`MAX_CONFIG_FILE`] bytes, through the text budget, and parsed strictly; an unusable
//! file, or an entry that is not valid, yields no autolink rather than an error. An autolink
//! is `{prefix, url}` from `[[autolinks]]`; the url must be `http` or `https`, so a client that
//! renders it never gets a `javascript:` or `file:` link. Counts and lengths are capped. The
//! server-side fields (`branchless`, `cloud`, `shared_refs`, `visibility`) belong to the
//! branchless follow-up (B07) and stay at their defaults here. Access is checked by the service.

use std::sync::Arc;

use asc_core::id::FileId;
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_rpc::server::{CallCtx, MAX_STRING, wire};
use tonic::Status;

use crate::commit_graph_rpc::{commit_status, data_loss, internal, registry_status};
use crate::idmode::IdCodec;
use crate::read::ReadHooks;
use crate::registry::Registry;

/// Largest `.asc/review.toml` read.
const MAX_CONFIG_FILE: u64 = 64 * 1024;
/// Most autolinks returned, and the longest prefix and url.
const MAX_AUTOLINKS: usize = 64;
const MAX_PREFIX: usize = 64;
const MAX_URL: usize = 2048;
const REVIEW_TOML: &str = ".asc/review.toml";

#[derive(serde::Deserialize, Default)]
struct ReviewToml {
    #[serde(default)]
    autolinks: Vec<Autolink>,
}

#[derive(serde::Deserialize)]
struct Autolink {
    prefix: String,
    url: String,
}

/// The valid autolinks of `text`, at most [`MAX_AUTOLINKS`].
fn parse_autolinks(text: &[u8]) -> Vec<wire::Autolink> {
    let Some(parsed) = std::str::from_utf8(text)
        .ok()
        .and_then(|t| toml::from_str::<ReviewToml>(t).ok())
    else {
        return Vec::new();
    };
    parsed
        .autolinks
        .into_iter()
        .filter(|a| {
            !a.prefix.is_empty()
                && a.prefix.len() <= MAX_PREFIX
                && !a
                    .prefix
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace())
                && a.url.len() <= MAX_URL
                && (a.url.starts_with("https://") || a.url.starts_with("http://"))
                && !a.url.chars().any(|c| c.is_control() || c.is_whitespace())
        })
        .take(MAX_AUTOLINKS)
        .map(|a| wire::Autolink {
            prefix: a.prefix,
            url: a.url,
        })
        .collect()
}

/// The `repo_config` method of the `insights` handler group.
pub struct ServerRepoConfig {
    registry: Arc<Registry>,
    hooks: Arc<ReadHooks>,
}

impl ServerRepoConfig {
    pub fn new(registry: Arc<Registry>, hooks: Arc<ReadHooks>) -> Self {
        Self { registry, hooks }
    }

    /// The configuration at `req.rev` (empty means `HEAD`).
    pub async fn repo_config(
        &self,
        ctx: &CallCtx,
        req: wire::RepoConfigRequest,
    ) -> Result<wire::RepoConfigResponse, Status> {
        if req.rev.len() > MAX_STRING {
            return Err(Status::invalid_argument("revision is too long"));
        }
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let engine = entry.backend.engine().clone();
        let shown = if req.rev.is_empty() { "HEAD" } else { &req.rev };
        let cs = ids
            .resolve(shown, &entry.default_branch, |e| {
                Status::invalid_argument(e.to_string())
            })
            .await?
            .ok_or_else(|| Status::not_found(format!("revision {shown} does not exist")))?;
        let root = engine.get_changeset(cs).await.map_err(commit_status)?.root;
        let path = RepoPath::new(REVIEW_TOML).map_err(|e| internal("review.toml path", &e))?;
        let found = engine.lookup(root, &path).await.map_err(|e| match e {
            asc_engine::tree::Error::MissingTree(_) | asc_engine::tree::Error::Codec(_) => {
                data_loss("tree", &e)
            }
            e => internal("tree", &e),
        })?;
        let mut out = wire::RepoConfigResponse::default();
        let Some(e) = found else {
            return Ok(out);
        };
        if !matches!(e.kind, Kind::File | Kind::Exec) || e.size > MAX_CONFIG_FILE {
            return Ok(out);
        }
        let _guard = self.hooks.text_budget.acquire(e.size).await.map_err(|_| {
            Status::resource_exhausted("text memory budget exhausted; retry shortly")
        })?;
        let data = engine
            .read_file(FileId::from_bytes(e.id), e.size)
            .await
            .map_err(|e| internal("review.toml", &e))?;
        if data.len() as u64 <= MAX_CONFIG_FILE {
            out.autolinks = parse_autolinks(&data);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use asc_core::id::{ChangesetId, RefTarget};
    use asc_rpc::IdMode;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::RepoName;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::registry::RepoEntry;

    struct Fx {
        handler: ServerRepoConfig,
        reg: Arc<Registry>,
        entry: Arc<RepoEntry>,
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let entry = repo(&reg).await;
        Fx {
            handler: ServerRepoConfig::new(reg.clone(), Arc::new(ReadHooks::default())),
            reg,
            entry,
        }
    }

    fn ctx(mode: IdMode) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: mode,
            principal: Principal { id: "alice".into() },
            op_id: None,
            client: None,
        }
    }

    fn req(rev: &str) -> wire::RepoConfigRequest {
        wire::RepoConfigRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: IdMode::Asc.to_wire(),
            }),
            rev: rev.into(),
        }
    }

    async fn set_master(f: &Fx, cs: ChangesetId) {
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: asc_core::path::RefName::new(b"refs/heads/master".to_vec()).unwrap(),
                expect: Expect::Any,
                new: Some(RefTarget::Changeset(cs)),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: uuid::Uuid::new_v4(),
        };
        f.reg.refs().update(f.entry.key, &txn).await.unwrap();
    }

    const TOML: &[u8] = b"[[autolinks]]\nprefix = \"JIRA-\"\nurl = \"https://j.example/browse/JIRA-<num>\"\n\n[[autolinks]]\nprefix = \"bad\"\nurl = \"javascript:alert(1)\"\n";

    #[tokio::test]
    async fn returns_valid_autolinks_at_the_revision() {
        let f = fx().await;
        let a = commit(&f.entry, &[], &[(".asc/review.toml", TOML)]).await;
        let b = commit(&f.entry, &[a], &[(".asc/review.toml", b"")]).await;
        set_master(&f, b).await;
        let at_a = f
            .handler
            .repo_config(&ctx(IdMode::Asc), req(&a.to_string()))
            .await
            .unwrap();
        assert_eq!(at_a.autolinks.len(), 1);
        assert_eq!(at_a.autolinks[0].prefix, "JIRA-");
        let head = f
            .handler
            .repo_config(&ctx(IdMode::Asc), req(""))
            .await
            .unwrap();
        assert!(head.autolinks.is_empty());
    }

    #[tokio::test]
    async fn missing_oversized_and_unusable_files_give_none() {
        let f = fx().await;
        let big = vec![b'#'; (MAX_CONFIG_FILE + 1) as usize];
        let cases: Vec<Vec<(&str, &[u8])>> = vec![
            vec![("README", b"x")],
            vec![(".asc/review.toml", b"autolinks = [")],
            vec![(".asc/review.toml", &[0xff, 0xfe])],
            vec![(".asc/review.toml", &big)],
        ];
        for files in cases {
            let c = commit(&f.entry, &[], &files).await;
            let got = f
                .handler
                .repo_config(&ctx(IdMode::Asc), req(&c.to_string()))
                .await
                .unwrap();
            assert!(got.autolinks.is_empty());
        }
    }

    #[test]
    fn autolinks_are_validated_and_capped() {
        let mut t = String::new();
        for i in 0..100 {
            t.push_str(&format!(
                "[[autolinks]]\nprefix = \"P{i}-\"\nurl = \"http://x/<num>\"\n"
            ));
        }
        assert_eq!(parse_autolinks(t.as_bytes()).len(), MAX_AUTOLINKS);
        let bad = format!(
            "[[autolinks]]\nprefix = \"\"\nurl = \"https://x\"\n[[autolinks]]\nprefix = \"a b\"\nurl = \"https://x\"\n[[autolinks]]\nprefix = \"{}\"\nurl = \"https://x\"\n[[autolinks]]\nprefix = \"a\"\nurl = \"https://x/{}\"\n[[autolinks]]\nprefix = \"c\"\nurl = \"file:///etc\"\n",
            "p".repeat(MAX_PREFIX + 1),
            "u".repeat(MAX_URL)
        );
        assert!(parse_autolinks(bad.as_bytes()).is_empty());
    }

    #[tokio::test]
    async fn rejects_bad_revisions() {
        let f = fx().await;
        let c = commit(&f.entry, &[], &[("a", b"1")]).await;
        set_master(&f, c).await;
        let code = |r: Result<wire::RepoConfigResponse, Status>| r.unwrap_err().code();
        assert_eq!(
            code(f.handler.repo_config(&ctx(IdMode::Asc), req("nope")).await),
            tonic::Code::NotFound
        );
        let long = "x".repeat(MAX_STRING + 1);
        assert_eq!(
            code(f.handler.repo_config(&ctx(IdMode::Asc), req(&long)).await),
            tonic::Code::InvalidArgument
        );
    }
}
