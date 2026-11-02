//! The objects group of the primary server: `HasObjects`, `GetObjects` and `PutObjects` for
//! every repository of the registry (SPEC §3.3).
//!
//! Each call opens the repository named by the request (`Registry::open`), so an object is read
//! and written in that repository's namespace only, and a missing repository answers
//! `NOT_FOUND`. The service layer has already authorized the call (`HasObjects` and
//! `GetObjects` need read, `PutObjects` write) and admitted it by class (`limits::class_of`:
//! `GetObjects` and `PutObjects` are charged to the caller's `objects` byte bucket as they
//! stream). The repository's engine backend validates the uploaded objects (stored encoding,
//! content id, no git-only extras). An upload adds objects no ref points to; the user-ref quota
//! (`UserQuota::claim`) is charged by the `UpdateRefs` that makes them reachable.

use std::sync::Arc;

use asc_core::change::MetaId;
use asc_rpc::server::{CallCtx, ObjectsHandler, RpcStream, wire};
use asc_store::key::ObjKey;
use async_trait::async_trait;
use futures::StreamExt as _;
use tonic::Status;

use crate::pathrules::scope::Scope;
use crate::registry::{Extensions, Registry, RegistryError, RepoEntry};

/// The objects handler over a registry.
pub struct ServerObjects {
    registry: Arc<Registry>,
    usage: Arc<crate::usage::UsageSlot>,
    read_only: Arc<std::sync::atomic::AtomicBool>,
    pathrules: Arc<crate::pathrules::PathRulesSlot>,
    objpaths: Arc<crate::pathrules::objpaths::ObjPaths>,
}

impl ServerObjects {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            usage: Arc::default(),
            read_only: Arc::default(),
            pathrules: Arc::default(),
            objpaths: Arc::default(),
        }
    }

    /// Applies the repository's path rules: a restricted caller fetches a tree or file by id
    /// only when it occurs at a path it can read, and no changeset (SPEC §15.5.1).
    #[must_use]
    pub fn with_pathrules(mut self, slot: Arc<crate::pathrules::PathRulesSlot>) -> Self {
        self.pathrules = slot;
        self
    }

    /// Refuses `keys` the caller's path scope does not allow by id.
    async fn gate<'k>(
        &self,
        ctx: &CallCtx,
        entry: &RepoEntry,
        keys: impl Iterator<Item = &'k wire::ObjKey>,
    ) -> Result<Option<Arc<Scope>>, Status> {
        let Some(scope) = Scope::read(&self.pathrules, ctx).await? else {
            return Ok(None);
        };
        let keys = keys
            .map(|k| ObjKey::try_from(k).map_err(|e| Status::invalid_argument(e.to_string())))
            .collect::<Result<Vec<_>, _>>()?;
        self.objpaths
            .require(self.registry.pool(), entry, &scope, &keys)
            .await?;
        Ok(Some(scope))
    }

    /// Refuses uploads while the flag is raised (a restore is staged).
    #[must_use]
    pub fn with_read_only(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.read_only = flag;
        self
    }

    /// Counts the bytes of every fetch against the repository's bandwidth.
    #[must_use]
    pub fn with_usage(mut self, slot: Arc<crate::usage::UsageSlot>) -> Self {
        self.usage = slot;
        self
    }

    async fn entry(&self, ctx: &CallCtx) -> Result<Arc<RepoEntry>, Status> {
        self.registry.open(&ctx.repo).await.map_err(|e| match e {
            RegistryError::NotFound(_) => Status::not_found("no such repository"),
            e => {
                tracing::error!(error = %e, "objects: opening the repository failed");
                Status::internal("internal error")
            }
        })
    }
}

#[async_trait]
impl ObjectsHandler for ServerObjects {
    async fn has_objects(
        &self,
        ctx: &CallCtx,
        req: wire::HasObjectsRequest,
    ) -> Result<wire::HasObjectsResponse, Status> {
        let entry = self.entry(ctx).await?;
        self.gate(ctx, &entry, req.keys.iter()).await?;
        entry.backend.has_objects(ctx, req).await
    }

    async fn put_objects(
        &self,
        ctx: &CallCtx,
        req: wire::PutObjectsRequest,
    ) -> Result<wire::PutObjectsResponse, Status> {
        if self.read_only.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Status::unavailable(
                "the server is read-only: a restore is staged and applies when the server restarts",
            ));
        }
        let entry = self.entry(ctx).await?;
        // The change metas of the upload, so the push that names their changesets finds them.
        let metas: Vec<MetaId> = req
            .objects
            .iter()
            .filter_map(|o| o.key.as_ref())
            .filter_map(|k| ObjKey::try_from(k).ok())
            .filter_map(|k| match k {
                ObjKey::ChangeMeta(id) => Some(id),
                _ => None,
            })
            .collect();
        let stored = entry.backend.put_objects(ctx, req).await?;
        if !metas.is_empty()
            && let Err(e) = crate::changes::staged::stage(
                self.registry.pool(),
                &entry,
                &ctx.principal.id,
                &metas,
            )
            .await
        {
            // The upload stands; the push that needs the meta reports it missing.
            tracing::warn!(error = %e, "staging the uploaded change metas failed");
        }
        Ok(stored)
    }

    async fn get_objects(
        &self,
        ctx: &CallCtx,
        req: wire::GetObjectsRequest,
    ) -> Result<RpcStream<wire::GetObjectsResponse>, Status> {
        use prost::Message as _;
        let entry = self.entry(ctx).await?;
        let scope = self.gate(ctx, &entry, req.keys.iter()).await?;
        let mut stream = entry.backend.get_objects(ctx, req).await?;
        if let Some(scope) = scope {
            stream = Box::pin(stream.map(move |r| {
                let mut resp = r?;
                for o in &mut resp.objects {
                    redact_object(&scope, o)?;
                }
                Ok(resp)
            }));
        }
        let Some(hub) = self.usage.get().cloned() else {
            return Ok(stream);
        };
        // Bytes are counted as they are polled and flushed when the stream is dropped, so a read
        // the client cuts off still counts what was sent.
        Ok(Box::pin(crate::usage::meter::Metered::new(
            stream,
            hub,
            entry.name.as_str().to_owned(),
            |r: &Result<wire::GetObjectsResponse, Status>| {
                r.as_ref().map_or(0, |m| m.encoded_len() as u64)
            },
        )))
    }
}

/// Replaces a changeset envelope with what `scope` may see of it (SPEC §15.5.1), marking it
/// `redacted`; other objects passed the gate as they are.
fn redact_object(scope: &Scope, o: &mut wire::Object) -> Result<(), Status> {
    use asc_core::codec::{Enveloped as _, decode_envelope};
    use asc_core::object::Changeset;
    use asc_store::codec::{decode_stored_capped, encode_stored};
    let Some(k) = o.key.as_ref() else {
        return Ok(());
    };
    if !matches!(ObjKey::try_from(k), Ok(ObjKey::Changeset(_))) {
        return Ok(());
    }
    let bad = |e: &dyn std::fmt::Display| {
        tracing::error!(error = %e, "objects: a stored changeset does not decode");
        Status::internal("internal error")
    };
    let data = decode_stored_capped(
        &o.data,
        asc_store::objects::max_decoded(asc_store::key::KeyClass::Changeset),
    )
    .map_err(|e| bad(&e))?;
    let cs: Changeset = decode_envelope(&data).map_err(|e| bad(&e))?;
    if let Some(shown) = scope.redact_changeset(&cs) {
        o.data = encode_stored(&shown.to_envelope());
        o.redacted = true;
    }
    Ok(())
}

/// Registers the objects group.
pub fn register(ext: &Extensions, registry: &Arc<Registry>) {
    ext.handlers
        .set_objects(Arc::new(
            ServerObjects::new(registry.clone())
                .with_usage(ext.usage.clone())
                .with_read_only(ext.read_only.clone())
                .with_pathrules(ext.pathrules.clone()),
        ))
        .expect("the objects handler group is registered once");
}

#[cfg(test)]
mod tests {
    use asc_core::codec::decode_envelope;
    use asc_core::id::ChangesetId;
    use asc_core::object::Changeset;
    use asc_rpc::IdMode;
    use asc_rpc::auth::Principal;
    use asc_store::codec::decode_stored_capped;
    use futures::TryStreamExt as _;

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::pathrules::scope::RESTRICTED_MESSAGE;

    fn ctx() -> CallCtx {
        CallCtx {
            repo: asc_rpc::server::RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: "alice".into() },
            op_id: None,
            client: None,
            admin: Default::default(),
        }
    }

    async fn fetch(h: &ServerObjects, ids: &[ChangesetId]) -> Vec<wire::Object> {
        let req = wire::GetObjectsRequest {
            repo: None,
            keys: ids
                .iter()
                .map(|c| wire::ObjKey::try_from(&ObjKey::Changeset(*c)).unwrap())
                .collect(),
        };
        let s = h.get_objects(&ctx(), req).await.unwrap();
        let rs: Vec<wire::GetObjectsResponse> = s.try_collect().await.unwrap();
        rs.into_iter().flat_map(|r| r.objects).collect()
    }

    fn changeset(o: &wire::Object) -> Changeset {
        decode_envelope(&decode_stored_capped(&o.data, 1 << 20).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_restricted_caller_gets_changesets_redacted_and_marked() {
        let reg = registry().await;
        let e = repo(&reg).await;
        let c1 = commit(&e, &[], &[("a", b"1\n"), ("secret/k", b"s\n")]).await;
        let c2 = commit(&e, &[c1], &[("secret/k", b"t\n")]).await;
        let c3 = commit(&e, &[c2], &[("a", b"2\n"), ("secret/k", b"u\n")]).await;
        let h = ServerObjects::new(reg.clone());
        // Nothing restricted: the stored envelopes as they are.
        for o in fetch(&h, &[c1, c2, c3]).await {
            assert!(!o.redacted);
        }
        let hub = crate::pathrules::PathRulesHub::new(
            reg.pool().clone(),
            Arc::new(crate::settings::testkit::Recorder::default()),
        );
        let rules = [crate::pathrules::parse_rule("secret", "*", "none").unwrap()];
        hub.set("o/r", &rules, 0, "pat:admin").await.unwrap();
        let slot = Arc::new(crate::pathrules::PathRulesSlot::default());
        assert!(slot.set(hub));
        let h = ServerObjects::new(reg.clone()).with_pathrules(slot);
        let got = fetch(&h, &[c1, c2, c3]).await;
        assert_eq!(got.len(), 3);
        for (o, id) in got.iter().zip([c1, c2, c3]) {
            assert!(o.redacted);
            let cs = changeset(o);
            assert_ne!(
                cs.id(),
                id,
                "a redacted changeset no longer hashes to its id"
            );
            assert!(
                cs.changes
                    .as_slice()
                    .iter()
                    .all(|(p, _)| !p.as_bytes().starts_with(b"secret"))
            );
            let original = e.backend.engine().get_changeset(id).await.unwrap();
            assert_eq!(cs.parents, original.parents);
            assert_eq!(cs.root, original.root);
        }
        // c2 changed only the restricted directory; c1 and c3 keep their messages.
        assert_eq!(changeset(&got[1]).message, RESTRICTED_MESSAGE);
        assert!(changeset(&got[1]).changes.as_slice().is_empty());
        assert_ne!(changeset(&got[0]).message, RESTRICTED_MESSAGE);
        assert_eq!(changeset(&got[2]).changes.as_slice().len(), 1);
    }
}
