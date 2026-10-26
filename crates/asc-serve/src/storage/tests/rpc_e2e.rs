//! The `Storage` and `Vault` services over a real gRPC connection: authentication, the
//! `x-asc-op-id` replay, error details, scoping and the masking of credentials on the wire.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use asc_rpc::auth::{AuthError, Access, Authorizer, BearerToken, Credential, Principal};
use asc_rpc::server::{status_rpc_error, wire};
use asc_rpc::services::storage_client::StorageClient;
use asc_rpc::services::vault_client::VaultClient;
use async_trait::async_trait;
use tonic::transport::{Channel, Server};
use tonic::{Code, Request};

use super::*;

/// Tokens to principals; everything else is anonymous.
struct Tokens(HashMap<&'static str, &'static str>);

#[async_trait]
impl Authorizer for Tokens {
    async fn authorize(
        &self,
        _t: Option<&BearerToken>,
        _repo: &str,
        _a: Access,
    ) -> Result<Principal, AuthError> {
        Err(AuthError::Forbidden)
    }

    async fn authenticate(&self, c: Option<&Credential>) -> Result<Principal, AuthError> {
        let c = c.ok_or(AuthError::Unauthenticated)?;
        self.0
            .get(c.secret().expose())
            .map(|id| Principal { id: (*id).into() })
            .ok_or(AuthError::Unauthenticated)
    }
}

struct Wire {
    fx: Fx,
    storage: StorageClient<Channel>,
    vault: VaultClient<Channel>,
}

async fn wire() -> Wire {
    let fx = fx().await;
    fx.table.grant_admin("host-service");
    let tokens: Arc<dyn Authorizer> = Arc::new(Tokens(HashMap::from([
        ("alice-token", "7:alice"),
        ("bob-token", "8:bob"),
        ("host-token", "host-service"),
        ("anon-token", "0:anonymous"),
    ])));
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr: SocketAddr = probe.local_addr().unwrap();
    drop(probe);
    let storage_svc = fx.byos.storage_service(tokens.clone());
    let vault_svc = fx.byos.vault_service(tokens);
    tokio::spawn(async move {
        Server::builder()
            .add_service(storage_svc)
            .add_service(vault_svc)
            .serve(addr)
            .await
            .ok();
    });
    let url = format!("http://{addr}");
    let channel = loop {
        match Channel::from_shared(url.clone()).unwrap().connect().await {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    Wire {
        fx,
        storage: StorageClient::new(channel.clone()),
        vault: VaultClient::new(channel),
    }
}

fn req<T>(msg: T, token: Option<&str>, op: Option<&str>) -> Request<T> {
    let mut r = Request::new(msg);
    if let Some(t) = token {
        r.metadata_mut()
            .insert("authorization", format!("Bearer {t}").parse().unwrap());
    }
    if let Some(o) = op {
        r.metadata_mut().insert("x-asc-op-id", o.parse().unwrap());
    }
    r
}

const OP1: &str = "6f1d2a40-5c1e-4b57-9a58-0d4d6f3c2e01";
const OP2: &str = "6f1d2a40-5c1e-4b57-9a58-0d4d6f3c2e02";
const OP3: &str = "6f1d2a40-5c1e-4b57-9a58-0d4d6f3c2e03";

fn create_msg(w: &Wire, name: &str) -> wire::CreateStorageTargetRequest {
    wire::CreateStorageTargetRequest {
        owner: Some(wire::OwnerRef {
            kind: wire::OwnerKind::User as i32,
            name: "alice".into(),
        }),
        name: name.into(),
        endpoint: w.fx.s3.endpoint(),
        region: "us-east-1".into(),
        bucket: BUCKET.into(),
        prefix: "pre/".into(),
        addressing: wire::StorageAddressing::Path as i32,
        credentials: Some(wire::StorageCredentials {
            access_key_id: AK.into(),
            secret_access_key: SK.into(),
            session_token: String::new(),
        }),
    }
}

#[tokio::test]
async fn create_over_the_wire_masks_the_secret_and_replays_a_retried_op_id() {
    let mut w = wire().await;
    let msg = create_msg(&w, "main");
    let first = w
        .storage
        .create_target(req(msg.clone(), Some("alice-token"), Some(OP1)))
        .await
        .unwrap()
        .into_inner()
        .target
        .unwrap();
    assert_eq!(first.state, wire::StorageTargetState::Pending as i32);
    assert_eq!((first.access_key_hint.as_str(), first.has_secret), ("AKIA...6789", true));
    assert_eq!(first.prefix, "pre/");
    let encoded = format!("{first:?}");
    assert!(!encoded.contains(SK));
    // The same op id returns the first answer, not a name conflict.
    let again = w
        .storage
        .create_target(req(msg.clone(), Some("alice-token"), Some(OP1)))
        .await
        .unwrap()
        .into_inner()
        .target
        .unwrap();
    assert_eq!(again.id, first.id);
    // A new op id with the same name conflicts.
    let dup = w
        .storage
        .create_target(req(msg.clone(), Some("alice-token"), Some(OP2)))
        .await
        .unwrap_err();
    assert_eq!(dup.code(), Code::AlreadyExists);
    // A mutating call needs the op id.
    let no_op = w
        .storage
        .create_target(req(create_msg(&w, "other"), Some("alice-token"), None))
        .await
        .unwrap_err();
    assert_eq!(no_op.code(), Code::InvalidArgument);
    // Reads return the same masked form.
    let got = w
        .storage
        .get_target(req(wire::GetStorageTargetRequest { id: first.id.clone() }, Some("alice-token"), None))
        .await
        .unwrap()
        .into_inner()
        .target
        .unwrap();
    assert_eq!(got.id, first.id);
    let list = w
        .storage
        .list_targets(req(
            wire::ListStorageTargetsRequest { owner: msg.owner.clone() },
            Some("alice-token"),
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.targets.len(), 1);
    assert!(!format!("{list:?}").contains(SK));
}

#[tokio::test]
async fn callers_are_authenticated_and_scoped_on_the_wire() {
    let mut w = wire().await;
    let created = w
        .storage
        .create_target(req(create_msg(&w, "main"), Some("alice-token"), Some(OP1)))
        .await
        .unwrap()
        .into_inner()
        .target
        .unwrap();
    let get = |id: &str| wire::GetStorageTargetRequest { id: id.into() };
    // No credential, an unknown one, and the anonymous user.
    for t in [None, Some("nobody"), Some("anon-token")] {
        let e = w.storage.get_target(req(get(&created.id), t, None)).await.unwrap_err();
        assert_eq!(e.code(), Code::Unauthenticated, "{t:?}");
    }
    // Another user: the same NOT_FOUND as for an id that does not exist.
    let a = w.storage.get_target(req(get(&created.id), Some("bob-token"), None)).await.unwrap_err();
    let b = w
        .storage
        .get_target(req(get("st_aaaaaaaaaaaaaaaaaaaa"), Some("bob-token"), None))
        .await
        .unwrap_err();
    assert_eq!((a.code(), b.code()), (Code::NotFound, Code::NotFound));
    assert_eq!(a.message(), b.message());
    // Naming someone else as the owner.
    let e = w
        .storage
        .create_target(req(create_msg(&w, "sneaky"), Some("bob-token"), Some(OP2)))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let e = w
        .storage
        .list_targets(req(
            wire::ListStorageTargetsRequest { owner: create_msg(&w, "x").owner },
            Some("bob-token"),
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    // Bad owner kinds and bad fields are invalid arguments.
    let mut bad = create_msg(&w, "main2");
    bad.owner = Some(wire::OwnerRef { kind: wire::OwnerKind::Instance as i32, name: String::new() });
    let e = w.storage.create_target(req(bad, Some("alice-token"), Some(OP3))).await.unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    let mut bad = create_msg(&w, "main2");
    bad.credentials = None;
    let e = w.storage.create_target(req(bad, Some("alice-token"), Some(OP3))).await.unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn a_blocked_endpoint_carries_the_reason_in_the_error_detail() {
    let mut w = wire().await;
    let mut msg = create_msg(&w, "main");
    msg.endpoint = "https://169.254.169.254".into();
    let e = w
        .storage
        .create_target(req(msg, Some("alice-token"), Some(OP1)))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    let detail = status_rpc_error(&e).expect("an ErrorDetail");
    assert_eq!(detail.code(), "STORAGE_ENDPOINT_BLOCKED");
    assert_eq!(detail.info().paths, vec!["metadata".to_owned()]);
    assert!(!e.message().contains("169.254"), "the message does not repeat the address: {}", e.message());
    // The failed call stored nothing and left no replayable answer.
    assert_eq!(w.fx.svc.db.count_targets(&owner_of("alice")).await.unwrap(), 0);
}

#[tokio::test]
async fn verify_enable_assign_and_migrate_over_the_wire() {
    let mut w = wire().await;
    w.fx.byos.engine.spawn();
    let id = w
        .storage
        .create_target(req(create_msg(&w, "main"), Some("alice-token"), Some(OP1)))
        .await
        .unwrap()
        .into_inner()
        .target
        .unwrap()
        .id;
    let v = w
        .storage
        .verify_target(req(wire::VerifyStorageTargetRequest { id: id.clone() }, Some("alice-token"), None))
        .await
        .unwrap()
        .into_inner();
    assert!(v.ok);
    let steps: Vec<i32> = v.checks.iter().map(|c| c.step).collect();
    assert_eq!(steps, vec![1, 2, 3, 4, 5]);
    assert_eq!(v.target.unwrap().state, wire::StorageTargetState::Verified as i32);
    let on = w
        .storage
        .set_enabled(req(
            wire::SetStorageTargetEnabledRequest { id: id.clone(), enabled: true },
            Some("alice-token"),
            None,
        ))
        .await
        .unwrap()
        .into_inner()
        .target
        .unwrap();
    assert_eq!(on.state, wire::StorageTargetState::Enabled as i32);
    // A failing verification is an answer, not an error; enabling a failed one is refused.
    let mut bad = create_msg(&w, "badkey");
    bad.credentials.as_mut().unwrap().secret_access_key = "wrong-wrong-wrong".into();
    let bad_id = w
        .storage
        .create_target(req(bad, Some("alice-token"), Some(OP2)))
        .await
        .unwrap()
        .into_inner()
        .target
        .unwrap()
        .id;
    let v = w
        .storage
        .verify_target(req(wire::VerifyStorageTargetRequest { id: bad_id.clone() }, Some("alice-token"), None))
        .await
        .unwrap()
        .into_inner();
    assert!(!v.ok);
    let failed = v.checks.iter().find(|c| !c.ok).unwrap();
    assert_eq!(failed.message, "the provider rejected the credentials");
    let e = w
        .storage
        .set_enabled(req(
            wire::SetStorageTargetEnabledRequest { id: bad_id, enabled: true },
            Some("alice-token"),
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::FailedPrecondition);
    assert_eq!(status_rpc_error(&e).unwrap().code(), "STORAGE_VERIFY_FAILED");

    // A repository with data, migrated through the API.
    let entry = w.fx.repo("acme/app").await;
    let objs = seed(&w.fx, &entry, 70).await;
    let repo = Some(wire::RepoRef { name: "acme/app".into(), ..Default::default() });
    let started = w
        .storage
        .start_migration(req(
            wire::StartStorageMigrationRequest { repo: repo.clone(), to_target_id: id.clone(), delete_source: false },
            Some("alice-token"),
            Some(OP3),
        ))
        .await
        .unwrap()
        .into_inner()
        .migration
        .unwrap();
    // The same op id replays the same migration.
    let replay = w
        .storage
        .start_migration(req(
            wire::StartStorageMigrationRequest { repo: repo.clone(), to_target_id: id.clone(), delete_source: false },
            Some("alice-token"),
            Some(OP3),
        ))
        .await
        .unwrap()
        .into_inner()
        .migration
        .unwrap();
    assert_eq!(replay.id, started.id);
    // bob cannot see it.
    let e = w
        .storage
        .get_migration(req(wire::GetStorageMigrationRequest { id: started.id.clone() }, Some("bob-token"), None))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    let mut last = started.clone();
    for _ in 0..500 {
        last = w
            .storage
            .get_migration(req(wire::GetStorageMigrationRequest { id: started.id.clone() }, Some("alice-token"), None))
            .await
            .unwrap()
            .into_inner()
            .migration
            .unwrap();
        if last.state == wire::StorageMigrationState::Done as i32 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(last.state, wire::StorageMigrationState::Done as i32, "{last:?}");
    assert_eq!(last.objects_total, 70);
    assert_all_in_target(&w.fx, &entry, &objs).await;
    let listed = w
        .storage
        .list_migrations(req(
            wire::ListStorageMigrationsRequest { repo, limit: 0, page_token: String::new() },
            Some("alice-token"),
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((listed.migrations.len(), listed.next_page_token.as_str()), (1, ""));
    // Deleting the target now is refused with the dedicated code.
    let e = w
        .storage
        .delete_target(req(wire::DeleteStorageTargetRequest { id }, Some("alice-token"), None))
        .await
        .unwrap_err();
    assert_eq!(status_rpc_error(&e).unwrap().code(), "STORAGE_IN_USE");
}

#[tokio::test]
async fn the_vault_is_for_site_administrators() {
    let mut w = wire().await;
    for t in ["alice-token", "bob-token"] {
        let e = w
            .vault
            .status(req(wire::VaultStatusRequest {}, Some(t), None))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::PermissionDenied);
    }
    let e = w.vault.status(req(wire::VaultStatusRequest {}, None, None)).await.unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);
    let s = w
        .vault
        .status(req(wire::VaultStatusRequest {}, Some("host-token"), None))
        .await
        .unwrap()
        .into_inner();
    assert!(s.ready, "{s:?}");
    assert_eq!(s.key_file_mode, "0600");
    assert_eq!(s.generations.len(), 1);
    assert!(s.generations[0].active);
    let t = w
        .storage
        .create_target(req(create_msg(&w, "main"), Some("alice-token"), Some(OP1)))
        .await
        .unwrap();
    drop(t);
    let r = w
        .vault
        .rotate(req(wire::RotateVaultRequest { retire_old: false }, Some("host-token"), Some(OP2)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.generation, 2);
    let replay = w
        .vault
        .rotate(req(wire::RotateVaultRequest { retire_old: false }, Some("host-token"), Some(OP2)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(replay.rotation_id, r.rotation_id, "a retried op id does not rotate twice");
    w.fx.vault.wait_idle().await;
    let g = w
        .vault
        .get_rotation(req(
            wire::GetVaultRotationRequest { rotation_id: r.rotation_id.clone() },
            Some("host-token"),
            None,
        ))
        .await
        .unwrap()
        .into_inner()
        .rotation
        .unwrap();
    assert!(g.finished && g.error.is_empty());
    assert_eq!((g.total, g.done), (1, 1));
    let e = w
        .vault
        .get_rotation(req(wire::GetVaultRotationRequest { rotation_id: "nope".into() }, Some("host-token"), None))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    // With no id: the newest.
    let latest = w
        .vault
        .get_rotation(req(wire::GetVaultRotationRequest::default(), Some("host-token"), None))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(latest.rotation.unwrap().id, r.rotation_id);
    assert!(w.fx.audit.actions().contains(&"vault.rotated".to_owned()));
}

#[tokio::test]
async fn the_features_are_advertised_exactly_while_registered() {
    assert!(super::super::features(&super::super::ByosSlot::default()).is_empty());
    let f = fx().await;
    let ext = Extensions::new();
    let _ = register(
        &ext,
        &f.registry,
        Parts {
            vault: f.vault.clone(),
            guard: f.svc.guard.clone(),
            cfg: StorageConfig::default(),
            authorizer: Arc::new(crate::DenyAll),
            audit: f.audit.clone(),
            authority: None,
        },
    );
    assert_eq!(super::super::features(&ext.byos), vec!["storage.v1", "vault.v1"]);
}
