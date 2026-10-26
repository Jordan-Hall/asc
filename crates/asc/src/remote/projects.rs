//! The project graph from a server (SPEC §12A.9 "Where it runs"; G10): `asc affected` asks the
//! server when the repository has a server remote and both revisions are pushed, and always
//! with `--repo`. [`ServerProjects`] is the `RpcClient` as the engine's `ProjectsApi`, connected
//! on first use; [`register`] installs it as U20's `ProjectsSelector`.

use std::sync::Arc;

use asc_core::id::ChangesetId;
use asc_engine::history::HistoryApi;
use asc_engine::projects::{AffectedQuery, AffectedResult, GraphView, ProjectsApi, Result};
use asc_graph::model::InputHashes;
use asc_rpc::client::RpcClient;
use asc_state::state::repos::RemoteKind;
use async_trait::async_trait;

use super::server::{Lazy, bound};
use crate::ext::{self, ProjectsSelector, RemoteSpec};

enum Source {
    /// Connects on first use, with the config and `state.db` of this invocation.
    Lazy(Lazy),
    Ready(RpcClient),
}

/// An asc server answering project queries.
pub struct ServerProjects(Source);

impl ServerProjects {
    /// For the server at `url`, connected on first use; `None` outside a workspace invocation.
    pub fn lazy(url: &str) -> Option<Self> {
        Some(Self(Source::Lazy(Lazy::new(bound()?, url.to_owned()))))
    }

    /// Over a client that is already connected (`--repo`).
    pub fn ready(client: RpcClient) -> Self {
        Self(Source::Ready(client))
    }

    /// The connected client; the error says why the server could not be reached.
    pub async fn client(&self) -> std::result::Result<&RpcClient, String> {
        match &self.0 {
            Source::Lazy(l) => l.client().await,
            Source::Ready(c) => Ok(c),
        }
    }

    /// Whether the server knows every one of `ids` (both revisions pushed). An unreachable
    /// server answers `false`, so the caller falls back to the local engine.
    pub async fn pushed(&self, ids: &[ChangesetId]) -> bool {
        let Ok(client) = self.client().await else {
            return false;
        };
        let specs: Vec<String> = ids.iter().map(ToString::to_string).collect();
        HistoryApi::resolve(client, &specs)
            .await
            .is_ok_and(|r| r.len() == ids.len() && r.iter().all(Option::is_some))
    }

    async fn api(&self) -> Result<&RpcClient> {
        self.client()
            .await
            .map_err(|m| asc_engine::projects::Error::History(asc_engine::graph::Error::Remote(m)))
    }
}

#[async_trait]
impl ProjectsApi for ServerProjects {
    async fn project_graph(&self, cs: ChangesetId) -> Result<GraphView> {
        ProjectsApi::project_graph(self.api().await?, cs).await
    }
    async fn affected(&self, q: &AffectedQuery) -> Result<AffectedResult> {
        ProjectsApi::affected(self.api().await?, q).await
    }
    async fn project_hashes(&self, cs: ChangesetId, target: &str) -> Result<InputHashes> {
        ProjectsApi::project_hashes(self.api().await?, cs, target).await
    }
}

struct Selector;

impl ProjectsSelector for Selector {
    fn select(&self, remote: &RemoteSpec) -> Option<Arc<dyn ProjectsApi>> {
        if remote.kind != RemoteKind::Server {
            return None;
        }
        let api = ServerProjects::lazy(remote.url.as_deref()?)?;
        Some(Arc::new(api))
    }
}

/// Registers the selector (called once from `main`).
pub fn register() {
    let _ = ext::set_projects_selector(Box::new(Selector));
}
