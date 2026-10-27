//! The `Signing` service: key registration and revocation, the verdicts a signature can get, the
//! server key, and the branch rule that needs signed changes. The fixture is the one of the policy
//! tests (a repository behind the gate).
//!
//! The throw-away keys below were made for these tests with `ssh-keygen -t ed25519`; they sign
//! nothing else. The armour lines are put together here so no source line holds a whole key block.

use std::sync::Arc;

use asc_core::id::ChangesetId;
use asc_core::sshsig::{self, KeyFileSigner};
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, BearerToken, Credential, StaticTokens};
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::signing_server::Signing as SigningApi;
use tonic::{Code, Request};
use uuid::Uuid;
use wire::VerificationStatus as V;

use super::rpc::{Signing, SigningService};
use super::{Config, ServerSigner};
use crate::authz::identity::HOST_SERVICE_PRINCIPAL;
use crate::ingest::testutil::commit;
use crate::policy::tests::{ALICE, BOB, Fx, code, fx_with, paths, policy_text};

const ALICE_BODY: &str = "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACCRkhzaJp495y3mienZIIjlMtZ+HDFEaMv5asll5INH8QAAAJgydzP/Mncz
/wAAAAtzc2gtZWQyNTUxOQAAACCRkhzaJp495y3mienZIIjlMtZ+HDFEaMv5asll5INH8Q
AAAECpoW5XdhH47VXvdSeyliHgbAaC7LpHrnBPd3PuxHjogZGSHNomnj3nLeaJ6dkgiOUy
1n4cMURoy/lqyWXkg0fxAAAADmFzYy10ZXN0LWFsaWNlAQIDBAUGBw==";
const ALICE_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJGSHNomnj3nLeaJ6dkgiOUy1n4cMURoy/lqyWXkg0fx asc-test-alice";
const MALLORY_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH63dZRlOJN5uHTCaUhCBYfg/yLFsDYL2VaxBhiVbAKB asc-test-mallory";

fn alice_key() -> KeyFileSigner {
    let text = [
        "-----BEGIN OPENSSH ",
        "PRIVATE KEY-----\n",
        ALICE_BODY,
        "\n-----END OPENSSH ",
        "PRIVATE KEY-----\n",
    ]
    .concat();
    KeyFileSigner::from_openssh(&text).unwrap()
}

fn sign(key: &KeyFileSigner, cs: &ChangesetId) -> String {
    key.sign(&sshsig::message(&cs.to_string()))
        .unwrap()
        .armored()
        .unwrap()
}

fn tokens() -> Arc<dyn asc_rpc::auth::Authorizer> {
    Arc::new(
        StaticTokens::new()
            .with_token("alice", ALICE, Access::Write)
            .with_token("bob", BOB, Access::Write)
            .with_token("reader", "44:carol", Access::Read)
            .with_token("host", HOST_SERVICE_PRINCIPAL, Access::Admin),
    )
}

fn call<T>(m: T, token: Option<&str>, op: bool) -> Request<T> {
    let mut r = Request::new(m);
    if let Some(t) = token {
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new(t).unwrap()));
    }
    if op {
        r.metadata_mut()
            .insert("x-asc-op-id", Uuid::new_v4().to_string().parse().unwrap());
    }
    r
}

fn repo_ref() -> Option<wire::RepoRef> {
    Some(wire::RepoRef {
        name: "o/r".into(),
        id_mode: IdMode::Asc.to_wire(),
    })
}

struct Svc {
    f: Fx,
    hub: Arc<Signing>,
    svc: SigningService,
}

async fn svc_with(cfg: Config, server: Option<Arc<ServerSigner>>) -> Svc {
    let f = fx_with(server.clone()).await;
    let hub = Arc::new(Signing::new(f.reg.clone(), cfg, server));
    let svc = SigningService::new(hub.clone(), tokens(), ServerInfo::default());
    Svc { f, hub, svc }
}

async fn svc() -> Svc {
    svc_with(Config::default(), None).await
}

impl Svc {
    async fn add(&self, token: &str, line: &str) -> Result<wire::SigningKey, tonic::Status> {
        self.svc
            .add_key(call(
                wire::AddSigningKeyRequest {
                    title: "laptop".into(),
                    public_key: line.into(),
                    expires_at: 0,
                },
                Some(token),
                false,
            ))
            .await
            .map(|r| r.into_inner().key.unwrap())
    }

    async fn submit(
        &self,
        token: &str,
        cs: &ChangesetId,
        armored: &str,
    ) -> Result<wire::SignatureVerification, tonic::Status> {
        self.svc
            .submit_signature(call(
                wire::SubmitSignatureRequest {
                    repo: repo_ref(),
                    changeset: bytes::Bytes::copy_from_slice(cs.as_bytes()),
                    signature: armored.into(),
                },
                Some(token),
                true,
            ))
            .await
            .map(|r| r.into_inner().verification.unwrap())
    }

    async fn verify(&self, cs: &ChangesetId) -> wire::SignatureVerification {
        self.svc
            .verify(call(
                wire::VerifySignaturesRequest {
                    repo: repo_ref(),
                    changesets: vec![bytes::Bytes::copy_from_slice(cs.as_bytes())],
                    changes: vec![],
                },
                Some("reader"),
                false,
            ))
            .await
            .unwrap()
            .into_inner()
            .results
            .remove(0)
    }

    async fn revoke(&self, token: &str, id: &str) -> Result<wire::SigningKey, tonic::Status> {
        self.svc
            .revoke_key(call(
                wire::RevokeSigningKeyRequest {
                    id: id.into(),
                    reason: "lost laptop".into(),
                },
                Some(token),
                false,
            ))
            .await
            .map(|r| r.into_inner().key.unwrap())
    }

    /// A second changeset of the repository, for a signature that names the wrong one.
    async fn another_changeset(&self) -> ChangesetId {
        let tip = self.f.tip().await;
        commit(&self.f.e, &[tip], &[("other", b"x")]).await
    }

    /// A changeset on a side ref, registered as version 1 of change `n` pushed by `actor`.
    async fn authored(&self, n: u8, actor: &str) -> ChangesetId {
        let base = commit(&self.f.e, &[], &[("base", b"0")]).await;
        self.f.set_master(base).await;
        let cs = commit(&self.f.e, &[base], &[("file", &[n])]).await;
        self.f.ingest(cs).await;
        self.f.register(cs, n, 1, actor).await;
        cs
    }
}

fn server_signer(dir: &tempfile::TempDir, name: &str, key_body: &str) -> Arc<ServerSigner> {
    use std::io::Write as _;
    let path = dir.path().join(name);
    let mut file = crate::authz::key_file::create_private(&path).unwrap();
    write!(
        file,
        "{}",
        [
            "-----BEGIN OPENSSH ",
            "PRIVATE KEY-----\n",
            key_body,
            "\n-----END OPENSSH ",
            "PRIVATE KEY-----\n",
        ]
        .concat()
    )
    .unwrap();
    drop(file);
    Arc::new(ServerSigner::load(&path).unwrap())
}

// ------------------------------------------------------------------------------ keys

#[tokio::test]
async fn a_user_registers_lists_and_revokes_their_own_keys() {
    let s = svc().await;
    let key = s.add("alice", ALICE_PUB).await.unwrap();
    assert!(key.id.starts_with("sk_"));
    assert_eq!(
        key.owner, "alice",
        "the owner is the caller, never a request field"
    );
    assert_eq!(key.kind, wire::SigningKeyKind::SshEd25519 as i32);
    assert!(key.fingerprint.starts_with("SHA256:"));
    assert_eq!(key.revoked_at, 0);
    // The comment on the line is not kept as the title: the title is what the person typed.
    assert_eq!(key.title, "laptop");
    // A retry by the owner returns the same key.
    assert_eq!(s.add("alice", ALICE_PUB).await.unwrap().id, key.id);
    // One fingerprint belongs to one key: nobody else registers it.
    let err = s.add("bob", ALICE_PUB).await.unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists);
    // Listing is the caller's own.
    let mine = s
        .svc
        .list_keys(call(
            wire::ListSigningKeysRequest::default(),
            Some("alice"),
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(mine.keys.len(), 1);
    let bobs = s
        .svc
        .list_keys(call(
            wire::ListSigningKeysRequest::default(),
            Some("bob"),
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(bobs.keys.is_empty());
    // Another user's keys are not there for anyone but the host service.
    let other = wire::ListSigningKeysRequest {
        owner: "alice".into(),
    };
    let err = s
        .svc
        .list_keys(call(other.clone(), Some("bob"), false))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    let host = s
        .svc
        .list_keys(call(other, Some("host"), false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(host.keys.len(), 1);
    // Bob cannot revoke it, and cannot tell it exists; Alice can.
    let err = s.revoke("bob", &key.id).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    let revoked = s.revoke("alice", &key.id).await.unwrap();
    assert!(revoked.revoked_at > 0);
    assert_eq!(revoked.revoked_reason, "lost laptop");
    // Both are audited, by fingerprint, never by key text.
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT action FROM audit_events WHERE action LIKE 'signing.key.%' ORDER BY action",
    )
    .fetch_all(s.f.reg.pool())
    .await
    .unwrap();
    assert_eq!(events, ["signing.key.added", "signing.key.revoked"]);
}

#[tokio::test]
async fn key_calls_need_a_user_and_refuse_bad_input() {
    let s = svc().await;
    let err = s
        .svc
        .add_key(call(
            wire::AddSigningKeyRequest {
                public_key: ALICE_PUB.into(),
                ..Default::default()
            },
            None,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
    // Not a key, a key with options, a key that is too long.
    for (line, why) in [
        ("not a key", "garbage"),
        ("command=\"rm -rf /\" ssh-ed25519 AAAA x", "options"),
        (&format!("ssh-ed25519 {}", "A".repeat(5000)), "too long"),
    ] {
        let err = s.add("alice", line).await.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument, "{why}");
    }
    // A title with a control character, an expiry in the past and one eleven years away.
    let now = crate::review::now();
    for (title, expires_at, why) in [
        ("bad\u{7}title".to_owned(), 0, "title"),
        ("ok".to_owned(), now - 10, "past"),
        ("ok".to_owned(), now + 11 * 365 * 24 * 3600, "far"),
        ("x".repeat(129), 0, "long title"),
    ] {
        let err = s
            .svc
            .add_key(call(
                wire::AddSigningKeyRequest {
                    title,
                    public_key: ALICE_PUB.into(),
                    expires_at,
                },
                Some("alice"),
                false,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument, "{why}");
    }
    assert_eq!(s.f.rows("SELECT COUNT(*) FROM signing_keys").await, 0);
    // Revoke: an empty or unknown id, a long reason.
    for id in ["", "sk_nothing"] {
        let err = s.revoke("alice", id).await.unwrap_err();
        assert!(
            matches!(err.code(), Code::InvalidArgument | Code::NotFound),
            "{id:?}"
        );
    }
    let key = s.add("alice", ALICE_PUB).await.unwrap();
    let err = s
        .svc
        .revoke_key(call(
            wire::RevokeSigningKeyRequest {
                id: key.id,
                reason: "r".repeat(257),
            },
            Some("alice"),
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn a_user_holds_at_most_the_configured_number_of_live_keys() {
    let s = svc_with(
        Config {
            max_keys_per_user: 1,
            ..Config::default()
        },
        None,
    )
    .await;
    let first = s.add("alice", ALICE_PUB).await.unwrap();
    let err = s.add("alice", MALLORY_PUB).await.unwrap_err();
    assert_eq!(code(&err), "SIGNING_KEY_LIMIT");
    // A revoked key does not count.
    s.revoke("alice", &first.id).await.unwrap();
    s.add("alice", MALLORY_PUB).await.unwrap();
}

// ------------------------------------------------------------------------------ verdicts

#[tokio::test]
async fn a_signature_by_a_key_of_the_author_is_valid_and_replaces_nothing_better() {
    let s = svc().await;
    let cs = s.authored(1, ALICE).await;
    assert_eq!(s.verify(&cs).await.status, V::Unsigned as i32);
    let key = s.add("alice", ALICE_PUB).await.unwrap();
    let sig = sign(&alice_key(), &cs);
    let v = s.submit("alice", &cs, &sig).await.unwrap();
    assert_eq!(v.status, V::Valid as i32, "{}", v.reason);
    assert_eq!(v.signer, "alice");
    assert_eq!(v.key_id, key.id);
    assert_eq!(v.fingerprint, key.fingerprint);
    assert!(v.signed_at > 0);
    assert_eq!(s.verify(&cs).await.status, V::Valid as i32);
    // Verify by change id gives the same answer.
    let by_change = s
        .svc
        .verify(call(
            wire::VerifySignaturesRequest {
                repo: repo_ref(),
                changesets: vec![],
                changes: vec![bytes::Bytes::copy_from_slice(
                    crate::policy::tests::cid(1).as_bytes(),
                )],
            },
            Some("reader"),
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(by_change.results[0].status, V::Valid as i32);
    // Someone with write access cannot unsign it by sending a signature that does not count.
    let wrong = sign(&alice_key(), &s.another_changeset().await);
    let err = s.submit("bob", &cs, &wrong).await.unwrap_err();
    assert_eq!(
        code(&err),
        "SIGNATURE_INVALID",
        "a signature of another changeset is refused"
    );
    assert_eq!(s.verify(&cs).await.status, V::Valid as i32);
    // The signature and the key row hold no secret, and the submission is audited.
    assert_eq!(
        s.f.rows(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'signing.signature.submitted'              AND scope_kind = 'repo'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn a_key_nobody_registered_and_a_key_of_someone_else_are_told_apart() {
    let s = svc().await;
    let cs = s.authored(2, ALICE).await;
    let sig = sign(&alice_key(), &cs);
    // The key is not registered by anyone.
    let v = s.submit("alice", &cs, &sig).await.unwrap();
    assert_eq!(v.status, V::UnknownKey as i32);
    assert!(v.signer.is_empty());
    assert_eq!(s.verify(&cs).await.status, V::UnknownKey as i32);
    // The key belongs to someone else than the author.
    s.add("bob", ALICE_PUB).await.unwrap();
    let v = s.submit("alice", &cs, &sig).await.unwrap();
    assert_eq!(v.status, V::KeyNotAuthor as i32);
    assert_eq!(v.signer, "bob");
    assert!(v.reason.contains("not of the author"), "{}", v.reason);
}

#[tokio::test]
async fn a_signature_after_the_key_was_revoked_does_not_count() {
    let s = svc().await;
    let cs = s.authored(9, ALICE).await;
    let key = s.add("alice", ALICE_PUB).await.unwrap();
    s.revoke("alice", &key.id).await.unwrap();
    // A revoked fingerprint stays taken: it cannot be registered again by anyone.
    assert_eq!(
        s.add("bob", ALICE_PUB).await.unwrap_err().code(),
        Code::AlreadyExists
    );
    let v = s
        .submit("alice", &cs, &sign(&alice_key(), &cs))
        .await
        .unwrap();
    assert_eq!(v.status, V::KeyRevoked as i32, "{}", v.reason);
}

#[tokio::test]
async fn a_key_revoked_after_the_signature_does_not_unsign_it() {
    let s = svc().await;
    let cs = s.authored(3, ALICE).await;
    let key = s.add("alice", ALICE_PUB).await.unwrap();
    let v = s
        .submit("alice", &cs, &sign(&alice_key(), &cs))
        .await
        .unwrap();
    assert_eq!(v.status, V::Valid as i32);
    // Time moves on: the revocation is later than the signature.
    sqlx::query("UPDATE change_signatures SET signed_at = signed_at - 100")
        .execute(s.f.reg.pool())
        .await
        .unwrap();
    s.revoke("alice", &key.id).await.unwrap();
    let after = s.verify(&cs).await;
    assert_eq!(after.status, V::Valid as i32, "{}", after.reason);
    assert!(
        after.reason.contains("before the key was revoked"),
        "{}",
        after.reason
    );
}

#[tokio::test]
async fn an_expired_key_does_not_vouch_for_a_new_signature() {
    let s = svc().await;
    let cs = s.authored(4, ALICE).await;
    let key = s.add("alice", ALICE_PUB).await.unwrap();
    sqlx::query("UPDATE signing_keys SET expires_at = 5 WHERE id = $1")
        .bind(key.id.clone())
        .execute(s.f.reg.pool())
        .await
        .unwrap();
    let v = s
        .submit("alice", &cs, &sign(&alice_key(), &cs))
        .await
        .unwrap();
    assert_eq!(v.status, V::KeyExpired as i32, "{}", v.reason);
}

#[tokio::test]
async fn a_changeset_whose_author_is_unknown_is_never_valid() {
    let s = svc().await;
    let base = commit(&s.f.e, &[], &[("base", b"0")]).await;
    s.f.set_master(base).await;
    let cs = commit(&s.f.e, &[base], &[("file", b"1")]).await;
    s.f.ingest(cs).await; // pushed, but in no change version
    s.add("alice", ALICE_PUB).await.unwrap();
    let v = s
        .submit("alice", &cs, &sign(&alice_key(), &cs))
        .await
        .unwrap();
    assert_eq!(v.status, V::KeyNotAuthor as i32);
    assert!(v.reason.contains("does not know which user authored"));
}

#[tokio::test]
async fn submit_and_verify_check_access_ids_and_sizes() {
    let s = svc().await;
    let cs = s.authored(5, ALICE).await;
    s.add("alice", ALICE_PUB).await.unwrap();
    let sig = sign(&alice_key(), &cs);
    // A reader may verify but not submit; no credential is refused; submit needs an operation id.
    let err = s.submit("reader", &cs, &sig).await.unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
    let err = s
        .svc
        .verify(call(
            wire::VerifySignaturesRequest {
                repo: repo_ref(),
                changesets: vec![],
                changes: vec![],
            },
            None,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
    let err = s
        .svc
        .submit_signature(call(
            wire::SubmitSignatureRequest {
                repo: repo_ref(),
                changeset: bytes::Bytes::copy_from_slice(cs.as_bytes()),
                signature: sig.clone(),
            },
            Some("alice"),
            false,
        ))
        .await
        .unwrap_err();
    assert_ne!(err.code(), Code::Ok);
    assert_eq!(
        s.verify(&cs).await.status,
        V::Unsigned as i32,
        "nothing was stored"
    );
    // Garbage, an oversized block, an unknown changeset.
    let err = s.submit("alice", &cs, "not a signature").await.unwrap_err();
    assert_eq!(code(&err), "SIGNATURE_INVALID");
    let err = s
        .submit("alice", &cs, &"x".repeat(sshsig::MAX_SIGNATURE_BYTES + 1))
        .await
        .unwrap_err();
    assert_eq!(code(&err), "SIGNATURE_INVALID");
    let unknown = ChangesetId::from_bytes([9; 32]);
    let err = s.submit("alice", &unknown, &sig).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    // Too many to verify at once; an unknown change.
    let many = wire::VerifySignaturesRequest {
        repo: repo_ref(),
        changesets: (0..=super::MAX_VERIFY)
            .map(|_| bytes::Bytes::copy_from_slice(cs.as_bytes()))
            .collect(),
        changes: vec![],
    };
    let err = s
        .svc
        .verify(call(many, Some("reader"), false))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let err = s
        .svc
        .verify(call(
            wire::VerifySignaturesRequest {
                repo: repo_ref(),
                changesets: vec![],
                changes: vec![bytes::Bytes::from_static(&[1; 16])],
            },
            Some("reader"),
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}

// ------------------------------------------------------------------------------ server key

#[tokio::test]
async fn the_server_key_is_public_and_signs_what_the_server_attests_to() {
    let s = svc().await;
    let none = s
        .svc
        .get_server_key(call(wire::GetServerKeyRequest {}, Some("reader"), false))
        .await
        .unwrap()
        .into_inner();
    assert!(!none.configured && none.public_key.is_empty());
    let err = s
        .svc
        .get_server_key(call(wire::GetServerKeyRequest {}, None, false))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);

    let dir = tempfile::tempdir().unwrap();
    let server = server_signer(&dir, "server_key", ALICE_BODY);
    let s = svc_with(Config::default(), Some(server)).await;
    let got = s
        .svc
        .get_server_key(call(wire::GetServerKeyRequest {}, Some("reader"), false))
        .await
        .unwrap()
        .into_inner();
    assert!(got.configured);
    assert_eq!(got.kind, wire::SigningKeyKind::SshEd25519 as i32);
    assert!(got.public_key.starts_with("ssh-ed25519 "));
    assert!(!got.public_key.contains("PRIVATE"));
    // The server attests to a changeset: valid, by the server, whatever its author.
    let cs = s.authored(6, BOB).await;
    assert_eq!(s.verify(&cs).await.status, V::Unsigned as i32);
    let repo_id = s.f.e.key.0;
    assert!(
        s.hub
            .verifier()
            .attest(repo_id, &cs, 1)
            .await
            .unwrap()
            .is_some()
    );
    let v = s.verify(&cs).await;
    assert_eq!(v.status, V::Valid as i32, "{}", v.reason);
    assert_eq!(v.signer, super::SERVER_SIGNER);
    assert_eq!(v.key_id, super::SERVER_KEY_ID);
    // Nobody replaces a valid signature with another one: a submission whose key is registered to
    // no one is answered with the verdict that stands, the server's.
    let v = s
        .submit("bob", &cs, &sign(&alice_key(), &cs))
        .await
        .unwrap();
    assert_eq!(v.status, V::Valid as i32, "{}", v.reason);
    assert_eq!(v.signer, super::SERVER_SIGNER);
}

#[test]
fn the_server_key_loads_only_from_a_private_unencrypted_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = ServerSigner::load(&dir.path().join("nope")).unwrap_err();
    assert!(missing.contains("signing.server_key_file"), "{missing}");
    // Text that is not a key: the error names the setting, not the content.
    let path = dir.path().join("junk");
    {
        use std::io::Write as _;
        let mut f = crate::authz::key_file::create_private(&path).unwrap();
        write!(f, "hunter2-not-a-key").unwrap();
    }
    let err = ServerSigner::load(&path).unwrap_err();
    assert!(
        err.contains("signing.server_key_file") && !err.contains("hunter2"),
        "{err}"
    );
    // The Debug form shows the fingerprint only.
    let ok = server_signer(&dir, "ok", ALICE_BODY);
    let shown = format!("{ok:?}");
    assert!(
        shown.contains("SHA256:") && !shown.contains(ALICE_BODY),
        "{shown}"
    );
}

#[test]
fn the_signing_settings_are_bounded() {
    assert!(Config::default().validate().is_ok());
    for n in [0usize, 1001] {
        let c = Config {
            max_keys_per_user: n,
            ..Config::default()
        };
        assert!(c.validate().unwrap_err().contains("max_keys_per_user"));
    }
}

// ------------------------------------------------------------------------------ the branch rule

#[tokio::test]
async fn a_branch_that_requires_signed_changes_lands_only_signed_ones() {
    let s = svc().await;
    let f = &s.f;
    let a = commit(
        &f.e,
        &[],
        &[
            (
                asc_core::policy::POLICY_PATH,
                policy_text("[signing]\nrequire_signed = true\n").as_bytes(),
            ),
            ("x", b"1"),
        ],
    )
    .await;
    f.set_master(a).await;
    let b = commit(&f.e, &[a], &[("y", b"2")]).await;
    f.register(b, 7, 1, ALICE).await;
    // Unsigned: refused, and the answer names the changeset and the reason.
    let err = f.land(ALICE, b).await.unwrap_err();
    assert_eq!(code(&err), "UNSIGNED_CHANGE");
    assert_eq!(paths(&err), [format!("{b} unsigned")]);
    assert_eq!(f.tip().await, a);
    // Signed by a key that is not the author's: still refused.
    s.add("bob", MALLORY_PUB).await.unwrap();
    s.add("alice", ALICE_PUB).await.unwrap();
    s.submit("alice", &b, &sign(&alice_key(), &b))
        .await
        .unwrap();
    f.land(ALICE, b).await.unwrap();
    assert_eq!(f.tip().await, b);
    // The next one is unsigned again.
    let c = commit(&f.e, &[b], &[("z", b"3")]).await;
    f.register(c, 8, 1, ALICE).await;
    let err = f.land(ALICE, c).await.unwrap_err();
    assert_eq!(code(&err), "UNSIGNED_CHANGE");
}

fn claim_call<T>(
    m: T,
    subject: &str,
    user: &str,
    role: asc_rpc::forwarded::Role,
    at: i64,
) -> Request<T> {
    let v = asc_rpc::forwarded::ForwardedVerifier::new(vec![5u8; 32]).unwrap();
    let mut r = call(m, Some("host"), false);
    for (k, val) in v.sign(user, role, subject, at) {
        r.metadata_mut().append(k, val.parse().unwrap());
    }
    r
}

#[tokio::test]
async fn the_hub_signing_keys_page_acts_as_the_person_of_an_account_claim_and_no_more() {
    use asc_rpc::forwarded::{ForwardedVerifier, Role};
    let f = fx_with(None).await;
    let hub = Arc::new(Signing::new(f.reg.clone(), Config::default(), None));
    let authz: Arc<dyn asc_rpc::auth::Authorizer> =
        Arc::new(crate::authz::forwarded::ForwardedAuthorizer::new(
            tokens(),
            ForwardedVerifier::new(vec![5u8; 32]).unwrap(),
        ));
    let svc = SigningService::new(hub, authz, ServerInfo::default());
    let now = crate::ai::clock::now();
    let add = || wire::AddSigningKeyRequest {
        title: "laptop".into(),
        public_key: ALICE_PUB.into(),
        expires_at: 0,
    };
    // Without a claim the host service has no user identity, as before.
    let e = svc
        .add_key(call(add(), Some("host"), false))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    // A read claim lists and does not write.
    let read = |at| {
        claim_call(
            wire::AddSigningKeyRequest::default(),
            "user:alice",
            "7:alice",
            Role::Read,
            at,
        )
    };
    let e = svc.add_key(read(now)).await.unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    // A write claim for the account adds the key as that person.
    let key = svc
        .add_key(claim_call(add(), "user:alice", "7:alice", Role::Write, now))
        .await
        .unwrap()
        .into_inner()
        .key
        .unwrap();
    let list = |user: &str, subject: &str, role, at| {
        claim_call(
            wire::ListSigningKeysRequest::default(),
            subject,
            user,
            role,
            at,
        )
    };
    let mine = svc
        .list_keys(list("7:alice", "user:alice", Role::Read, now))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(mine.keys.len(), 1);
    // Another person's page shows another person's keys: none.
    let theirs = svc
        .list_keys(list("8:bob", "user:bob", Role::Write, now))
        .await
        .unwrap()
        .into_inner();
    assert!(theirs.keys.is_empty());
    // A claim for another account, a stale claim and a forged one change nothing.
    let e = svc
        .list_keys(list("8:bob", "user:alice", Role::Write, now))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let e = svc
        .list_keys(list("7:alice", "user:alice", Role::Write, now - 600))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    // Bob's write claim cannot revoke Alice's key and cannot tell it exists.
    let revoke = |user: &str, subject: &str| {
        claim_call(
            wire::RevokeSigningKeyRequest {
                id: key.id.clone(),
                reason: String::new(),
            },
            subject,
            user,
            Role::Write,
            now,
        )
    };
    let e = svc
        .revoke_key(revoke("8:bob", "user:bob"))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    svc.revoke_key(revoke("7:alice", "user:alice"))
        .await
        .unwrap();
}
