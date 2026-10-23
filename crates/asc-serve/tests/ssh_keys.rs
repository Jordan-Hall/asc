//! U84: SSH public keys in the `Authorizer` and the token store. Add, identify, authorise, expire
//! and revoke; user keys never exceed their user, deploy keys stay on one repository; one
//! fingerprint, one owner; option lines and weak keys are refused; only public material is
//! stored; untyped rows are refused; the tables are bounded; the cache bound; the CLI functions.
#![cfg(feature = "git-ssh")]

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use base64::Engine as _;
use asc_rpc::auth::{
    Access, AuthError, Authorizer, Decision, Principal, SshFingerprint, SshIdentity, SshKey,
    SshKeyKind,
};
use asc_serve::authz::ssh_keys::{
    self, DEFAULT_MIN_RSA_BITS, MAX_KEYS_PER_OWNER, MAX_LIVE_SSH_KEYS, NewSshKey,
};
use asc_serve::authz::token_cli::{self, SshKeyAddArgs};
use asc_serve::authz::token_store::{
    CACHE_TTL_SECS, DEAD_RETENTION_SECS, NewToken, TokenError, TokenKey, TokenKind, TokenStore,
    TokenStoreAuthz,
};
use asc_serve::config::ServeConfig;
use asc_serve::migrate;
use sqlx::{AnyPool, Row};

const T0: i64 = 1_800_000_000;

// Keys made with `ssh-keygen -t <type> -N ""`; the private halves are not kept.
const ED_A: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAII9Us3YO+W3z3kVVPlsOaSbvDrdNg/GGwx0Lt1AU9P1N ed25519@test";
const ED_B: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIALh0iSRdAdT03gj5pvIPUJdeN08a2tDo06nIDDXH1UI e1@test";
const ED_C: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMqTOXHfClSZGrUz4IiJr3GLN3G4VwlT/13c5RfS6Vgk e2@test";
const ED_D: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBTB7/2Sv0Vl883qZDfxqlvQVjBGVWb4qbyIjcHXgFzc e3@test";
const ECDSA_256: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBLPgix1Bz+AFvFtguk2+ttPkz8ltupkokXzL+1FpHOXThfyihaapOupYhNK3oxXoWgZ3PlMNiJOgaEYdVQD4xQ8= ecdsab256@test";
const ECDSA_384: &str = "ecdsa-sha2-nistp384 AAAAE2VjZHNhLXNoYTItbmlzdHAzODQAAAAIbmlzdHAzODQAAABhBM39H6ZEBbda/wKEtryXOGlGhBBGf3X7x1DNR2Esap431DgFbiQDW6KeyaL3Z9FD/DRQsbN9UVNDTeOSJFPqGba2Sp0K9/udNjlp3hwhh+5Dx352Zd+NVgQr0Uth7AjKiA== ecdsab384@test";
const ECDSA_521: &str = "ecdsa-sha2-nistp521 AAAAE2VjZHNhLXNoYTItbmlzdHA1MjEAAAAIbmlzdHA1MjEAAACFBADxpYlfMnMw6QGrkCjou5zXNb4nBp+n9QRdG358svmxvzbQUE/d0sRXofg9Kn0SN2m3NUnXjyZ+ZQcfY+IpDMetSAHpBuKx0S8/1CbTuQDl6FrLOT+y+rkgk7bFDpatfSS2ro0yPobhNVS25iJXpCfyre4NrF3NbQ7vUtJHLQc6WnZslA== ecdsab521@test";
const RSA_2048: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQC6lmfZ+si11mRruJyFYhFWnTxBpyt9U4WAYo9r6Wf3nJtxgDt5nS+3qGO7pvO3hNJnuOBMBFD2w1Br45d5jD0iyWsSevntZWTVoJOUdzApCUe39df4ttSFaI0g2bc7QxzoQ7xI0noKArT3vsp9F/Zi8axmJ9PDIZDfmMZU8U+2XfE3yQveKOKV8BcGuiUiDLKoijDx0b7scWsAWe+fP/iWMyPRd1EnlyAgzhH4SvvTQH9Ghq4ARm8YFDyFP/1maQdnLWsRCRFZ4Onlsxxpivq5rHYrN14cxJ95Pl4qXBmsq0M3ODylAUE/NiAyzw2D4XOGhE125CyDUdbnWGT1FNQD rsab2048@test";
const RSA_3072: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCv7ntZcwkIgramIoPCq8wWmI23heINNSaU6I4WoCIpuNgMH8J7K4wHfriE433K3rhfoIZ2gmxboilLhOqO3lvCmkrh+gqk8h+Umj4SlpUnmOuEVYg5gMe/+6r2cZZt9r8ZUG0NQmcazC7AdsWrM0+7V1iWP1LHbaqxLK0oaAGgw8u1kWuwM/n5CeJ/NInwun8ZldFUB16Q/sUhu+iIusbTAA63FWiN6qmJq8A2FjVVKHVr/T1arAPBXD1keOCWSPUqPn7I5CzjmHMFqCK//zSxmWDa3q5sFbAA5HRkotrfWJDudYOKyiG9H9Y+2ZjuXWXgA+H8JCAnDfIyeQTKD8N6zLwwvOFRgNmMVSBzjuY5qOkkL5Ytl47xBkzZbpB5eaH+/VkLDkqa0qRsuEgFaxKy2KenTiwqO4rXSa39urWm3ArzUIG3WeDuADoME9kYu8g7Kp51N16q378a90jPPDl6vzyEdqzq7P+G4pjvsT9OVXGHM1tF+GdVv86cjTXseFk= rsab3072@test";
const RSA_1024: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQDAz334CV6OuLpprLRdGB/v4qg1EfRoyJuWrhWEV0XzO/E+DoXe46sptRrdA4akS4GrPx4Mh3Cs5KMFHT/vyLmgWKh1jq/9RnkFEbK/cMrzL26lAGdgMPjMjimRDEd6q4vISFsboSPa9+jZY7sKiUdUvKcnGLcDbi2Gn+Z9acn7kw== rsab1024@test";
/// An `ssh-dss` public key (1024 bits), from the `ssh-key` crate's examples.
const DSA: &str = "ssh-dss AAAAB3NzaC1kc3MAAACBANw9iSUO2UYhFMssjUgW46URqv8bBrDgHeF8HLBOWBvKuXF2Rx2J/XyhgX48SOLMuv0hcPaejlyLarabnF9F2V4dkpPpZSJ+7luHmxEjNxwhsdtg8UteXAWkeCzrQ6MvRJZHcDBjYh56KGvslbFnJsGLXlI4PQCyl6awNImwYGilAAAAFQCJGBU3hZf+QtP9Jh/nbfNlhFu7hwAAAIBHObOQioQVRm3HsVb7mOy3FVKhcLoLO3qoG9gTkd4KeuehtFAC3+rckiX7xSCnE/5BBKdL7VP9WRXac2Nlr9Pwl3e7zPut96wrCHt/TZX6vkfXKkbpUIj5zSqfvyNrWKaYJkfzwAQwrXNS1Hol676Ud/DDEn2oatdEhkS3beWHXAAAAIBgQqaz/YYTRMshzMzYcZ4lqgvgmA55y6v0h39e8HH2A5dwNS6sPUw2jyna+le0dceNRJifFld1J+WYM0vmquSr11DDavgEidOSaXwfMvPPPJqLmbzdtT16N+Gij9U9STQTHPQcQ3xnNNHgQAStzZJbhLOVbDDDo5BO7LMUALDfSA== user@example.com";

struct Fixture {
    pool: AnyPool,
    store: Arc<TokenStore>,
    authz: TokenStoreAuthz,
    clock: Arc<AtomicI64>,
}

async fn fixture() -> Fixture {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let clock = Arc::new(AtomicI64::new(T0));
    let store = Arc::new(store_on(&pool, &clock));
    Fixture {
        pool,
        authz: TokenStoreAuthz::new(store.clone()),
        store,
        clock,
    }
}

/// Another process over the same database: its own cache.
fn store_on(pool: &AnyPool, clock: &Arc<AtomicI64>) -> TokenStore {
    let c = clock.clone();
    TokenStore::new(pool.clone(), TokenKey::generate().unwrap())
        .with_clock(Arc::new(move || c.load(Ordering::SeqCst)))
}

fn user_req(user: &str, line: &str) -> NewSshKey {
    NewSshKey {
        kind: SshKeyKind::User,
        user: Some(user.into()),
        repo: None,
        write: false,
        key_file: format!("{line}\n"),
        title: String::new(),
        expires_in: None,
        min_rsa_bits: DEFAULT_MIN_RSA_BITS,
    }
}

fn deploy_req(repo: &str, write: bool, line: &str) -> NewSshKey {
    NewSshKey {
        kind: SshKeyKind::Deploy,
        user: None,
        repo: Some(repo.into()),
        write,
        key_file: format!("{line}\n"),
        title: String::new(),
        expires_in: None,
        min_rsa_bits: DEFAULT_MIN_RSA_BITS,
    }
}

/// The key as the listener hands it over.
fn key_of(line: &str) -> SshKey {
    ssh_keys::parse_key_file(line, DEFAULT_MIN_RSA_BITS)
        .unwrap()
        .ssh_key()
        .unwrap()
}

/// A syntactically valid ed25519 line with a distinct key per `i` (not a usable key pair).
fn ed_line(i: u32) -> String {
    let mut wire = Vec::new();
    wire.extend_from_slice(&11u32.to_be_bytes());
    wire.extend_from_slice(b"ssh-ed25519");
    wire.extend_from_slice(&32u32.to_be_bytes());
    let mut point = [0u8; 32];
    point[..4].copy_from_slice(&i.to_be_bytes());
    point[31] = 1;
    wire.extend_from_slice(&point);
    format!(
        "ssh-ed25519 {} gen{i}",
        base64::engine::general_purpose::STANDARD.encode(wire)
    )
}

impl Fixture {
    fn advance(&self, secs: i64) {
        self.clock.fetch_add(secs, Ordering::SeqCst);
    }

    /// A personal token for `user` with `access` on each of `repos`.
    async fn pat(
        &self,
        user: &str,
        repos: &[&str],
        access: Access,
        expires_in: Option<i64>,
    ) -> String {
        let issued = self
            .store
            .create(&NewToken {
                kind: TokenKind::Pat,
                user: Some(user.into()),
                repos: repos.iter().map(|r| (*r).to_owned()).collect(),
                access,
                expires_in,
                label: String::new(),
            })
            .await
            .unwrap();
        issued.id
    }

    async fn can(&self, line: &str, repo: &str, access: Access) -> Option<Principal> {
        match self
            .authz
            .authorize_ssh_key(&key_of(line), repo, access)
            .await
            .unwrap()
        {
            Decision::Allowed(p) => Some(p),
            _ => None,
        }
    }

    async fn identify(&self, line: &str) -> Option<SshIdentity> {
        self.authz.identify_ssh_key(&key_of(line)).await.unwrap()
    }
}

fn invalid(r: Result<impl std::fmt::Debug, TokenError>) -> String {
    match r {
        Err(TokenError::Invalid(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn add_identify_authorize_expire_and_revoke() {
    let f = fixture().await;
    f.pat("alice", &["acme/web"], Access::Write, None).await;
    let mut req = user_req("alice", ED_A);
    req.title = "laptop".into();
    req.expires_in = Some(1000);
    let info = f.store.add_ssh_key(&req).await.unwrap();
    assert_eq!(info.principal, "pat:alice");
    assert_eq!(info.fingerprint, key_of(ED_A).fingerprint());
    assert_eq!(info.expires, Some(T0 + 1000));

    let id = f.identify(ED_A).await.unwrap();
    assert_eq!(
        id,
        SshIdentity {
            key_id: info.id.clone(),
            kind: SshKeyKind::User,
            principal: Principal {
                id: "pat:alice".into()
            },
            repo: None,
        }
    );
    assert_eq!(
        f.can(ED_A, "acme/web", Access::Write).await.unwrap().id,
        "pat:alice"
    );
    assert!(f.can(ED_A, "acme/web", Access::Read).await.is_some());
    assert!(f.can(ED_A, "acme/web", Access::Admin).await.is_none());
    // An unknown key is not recognised and not authorised.
    assert!(f.identify(ED_B).await.is_none());
    assert!(f.can(ED_B, "acme/web", Access::Read).await.is_none());

    // The listing shows the key without its blob, and the use of it.
    let listed = f.store.list_ssh_keys().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "laptop");
    assert_eq!(listed[0].status(f.store.now()), "active");
    assert_eq!(listed[0].last_used, Some(T0));

    // Expiry: the key is refused from the instant it expires, even from the cache.
    f.advance(999);
    assert!(f.can(ED_A, "acme/web", Access::Read).await.is_some());
    f.advance(1);
    assert!(f.identify(ED_A).await.is_none());
    assert!(f.can(ED_A, "acme/web", Access::Read).await.is_none());

    // Revocation through this store is immediate.
    let b = f.store.add_ssh_key(&user_req("alice", ED_B)).await.unwrap();
    assert!(f.can(ED_B, "acme/web", Access::Read).await.is_some());
    assert!(
        f.store
            .revoke_ssh_key(&b.fingerprint.to_string())
            .await
            .unwrap()
    );
    assert!(f.identify(ED_B).await.is_none());
    assert!(f.can(ED_B, "acme/web", Access::Read).await.is_none());
    assert!(
        !f.store
            .revoke_ssh_key(&b.fingerprint.to_string())
            .await
            .unwrap()
    );
    assert!(matches!(
        f.store.revoke_ssh_key("ffffffffffffffff").await,
        Err(TokenError::NotFound)
    ));
    assert!(matches!(
        f.store
            .revoke_ssh_key(&SshFingerprint::of_blob(b"nope").to_string())
            .await,
        Err(TokenError::NotFound)
    ));
    invalid(f.store.revoke_ssh_key("SHA256:short").await);
    // By id as well.
    let c = f.store.add_ssh_key(&user_req("alice", ED_C)).await.unwrap();
    assert!(f.store.revoke_ssh_key(&c.id).await.unwrap());
    assert!(f.identify(ED_C).await.is_none());
}

#[tokio::test]
async fn a_revoked_key_is_refused_within_the_cache_bound_by_another_process() {
    let f = fixture().await;
    f.pat("alice", &["acme/web"], Access::Read, None).await;
    let info = f.store.add_ssh_key(&user_req("alice", ED_A)).await.unwrap();
    assert!(f.can(ED_A, "acme/web", Access::Read).await.is_some());

    // `asc serve ssh-key revoke` runs in another process with its own cache.
    let cli = store_on(&f.pool, &f.clock);
    assert!(cli.revoke_ssh_key(&info.id).await.unwrap());
    // This process may still answer from its cache, but never past the bound.
    f.advance(CACHE_TTL_SECS);
    assert!(f.can(ED_A, "acme/web", Access::Read).await.is_none());
    assert!(f.identify(ED_A).await.is_none());

    // A refusal is cached for the same bound: a key added elsewhere shows up within it.
    assert!(f.identify(ED_B).await.is_none());
    cli.add_ssh_key(&user_req("alice", ED_B)).await.unwrap();
    f.advance(CACHE_TTL_SECS);
    assert!(f.identify(ED_B).await.is_some());
}

#[tokio::test]
async fn a_user_key_never_has_more_than_its_user() {
    let f = fixture().await;
    f.pat("alice", &["acme/web"], Access::Read, None).await;
    f.pat("bob", &["acme/*"], Access::Write, None).await;
    f.store.add_ssh_key(&user_req("alice", ED_A)).await.unwrap();
    f.store.add_ssh_key(&user_req("bob", ED_B)).await.unwrap();
    f.store.add_ssh_key(&user_req("carol", ED_C)).await.unwrap();

    assert!(f.can(ED_A, "acme/web", Access::Read).await.is_some());
    assert!(f.can(ED_A, "acme/web", Access::Write).await.is_none());
    assert!(f.can(ED_A, "acme/other", Access::Read).await.is_none());
    assert!(f.can(ED_A, "other/web", Access::Read).await.is_none());
    assert_eq!(
        f.can(ED_B, "acme/other", Access::Write).await.unwrap().id,
        "pat:bob"
    );
    assert!(f.can(ED_B, "other/x", Access::Read).await.is_none());
    // A user with no live token has no rights, though the key is recognised.
    assert!(f.identify(ED_C).await.is_some());
    assert!(f.can(ED_C, "acme/web", Access::Read).await.is_none());

    // The rights follow the user's tokens: a token that expires or is revoked takes them away.
    let short = f
        .pat("carol", &["acme/web"], Access::Write, Some(100))
        .await;
    f.advance(CACHE_TTL_SECS);
    assert!(f.can(ED_C, "acme/web", Access::Write).await.is_some());
    f.advance(100);
    assert!(f.can(ED_C, "acme/web", Access::Read).await.is_none());
    let t = f.pat("carol", &["acme/web"], Access::Read, None).await;
    f.advance(CACHE_TTL_SECS);
    assert!(f.can(ED_C, "acme/web", Access::Read).await.is_some());
    f.store.revoke(&t).await.unwrap();
    assert!(f.can(ED_C, "acme/web", Access::Read).await.is_none());
    let _ = short;
}

#[tokio::test]
async fn a_deploy_key_stays_on_its_repository_and_is_read_only_unless_registered_with_write() {
    let f = fixture().await;
    // Rights elsewhere in the store do not widen a deploy key.
    f.pat("alice", &["*"], Access::Admin, None).await;
    let ro = f
        .store
        .add_ssh_key(&deploy_req("acme/web", false, ED_A))
        .await
        .unwrap();
    f.store
        .add_ssh_key(&deploy_req("acme/api", true, ED_B))
        .await
        .unwrap();
    assert_eq!(ro.repo.as_deref(), Some("acme/web"));

    let id = f.identify(ED_A).await.unwrap();
    assert_eq!(id.kind, SshKeyKind::Deploy);
    assert_eq!(id.principal.id, "deploy:acme/web");
    assert_eq!(id.repo.as_deref(), Some("acme/web"));

    assert_eq!(
        f.can(ED_A, "acme/web", Access::Read).await.unwrap().id,
        "deploy:acme/web"
    );
    assert!(f.can(ED_A, "acme/web", Access::Write).await.is_none());
    assert!(f.can(ED_A, "acme/api", Access::Read).await.is_none());
    assert!(f.can(ED_A, "acme/web2", Access::Read).await.is_none());
    assert!(f.can(ED_A, "acme/web", Access::Admin).await.is_none());

    assert!(f.can(ED_B, "acme/api", Access::Write).await.is_some());
    assert!(f.can(ED_B, "acme/api", Access::Read).await.is_some());
    assert!(f.can(ED_B, "acme/web", Access::Read).await.is_none());
    assert!(f.can(ED_B, "acme/api", Access::Admin).await.is_none());
}

#[tokio::test]
async fn one_fingerprint_has_one_owner() {
    let f = fixture().await;
    f.store.add_ssh_key(&user_req("alice", ED_A)).await.unwrap();
    // Twice for one user, for another user, and as a deploy key.
    invalid(f.store.add_ssh_key(&user_req("alice", ED_A)).await);
    invalid(f.store.add_ssh_key(&user_req("bob", ED_A)).await);
    let m = invalid(
        f.store
            .add_ssh_key(&deploy_req("acme/web", false, ED_A))
            .await,
    );
    assert!(m.contains("already registered"), "{m}");
    // A different comment, spacing or title does not make it a different key.
    let b64 = ED_A.split_whitespace().nth(1).unwrap();
    invalid(
        f.store
            .add_ssh_key(&user_req(
                "bob",
                &format!("ssh-ed25519   {b64}   other comment"),
            ))
            .await,
    );
    // And the other way round: a deploy key cannot become a user key.
    f.store
        .add_ssh_key(&deploy_req("acme/web", false, ED_B))
        .await
        .unwrap();
    invalid(f.store.add_ssh_key(&user_req("alice", ED_B)).await);
    assert_eq!(f.store.list_ssh_keys().await.unwrap().len(), 2);

    // A revoked key can be registered again (to anyone); the dead row is replaced.
    f.store
        .revoke_ssh_key(&key_of(ED_A).fingerprint().to_string())
        .await
        .unwrap();
    f.store.add_ssh_key(&user_req("bob", ED_A)).await.unwrap();
    assert_eq!(f.identify(ED_A).await.unwrap().principal.id, "pat:bob");
    assert_eq!(f.store.list_ssh_keys().await.unwrap().len(), 2);
}

#[tokio::test]
async fn weak_odd_and_optioned_keys_are_refused_and_strong_ones_accepted() {
    let f = fixture().await;
    for ok in [ED_A, ECDSA_256, ECDSA_384, ECDSA_521, RSA_2048, RSA_3072] {
        let info = f.store.add_ssh_key(&user_req("alice", ok)).await.unwrap();
        assert_eq!(info.fingerprint, key_of(ok).fingerprint(), "{ok}");
    }
    let weak = invalid(f.store.add_ssh_key(&user_req("alice", RSA_1024)).await);
    assert!(weak.contains("1024"), "{weak}");
    invalid(f.store.add_ssh_key(&user_req("alice", DSA)).await);
    // The floor is the caller's: a listener that asks for 3072 gets it.
    let mut strict = user_req("alice", RSA_2048);
    strict.min_rsa_bits = 3072;
    let _ = f
        .store
        .revoke_ssh_key(&key_of(RSA_2048).fingerprint().to_string())
        .await;
    invalid(f.store.add_ssh_key(&strict).await);

    for options in [
        "command=\"ls\"",
        "restrict",
        "no-port-forwarding,no-pty",
        "from=\"10.0.0.1\"",
        "environment=\"A=b\"",
        "cert-authority",
    ] {
        let m = invalid(
            f.store
                .add_ssh_key(&user_req("alice", &format!("{options} {ED_D}")))
                .await,
        );
        assert!(m.contains("options"), "{options}: {m}");
    }
    // Certificates, security keys and unknown types are not keys either.
    for bad in [
        "ssh-ed25519-cert-v01@openssh.com AAAA x",
        "sk-ssh-ed25519@openssh.com AAAA x",
        "ssh-made-up AAAA x",
        "ssh-ed25519 not-base64!! x",
        "",
        "# nothing here",
    ] {
        invalid(f.store.add_ssh_key(&user_req("alice", bad)).await);
    }
    // Two keys in one file, a private key, and a key file that is far too big.
    invalid(
        f.store
            .add_ssh_key(&user_req("alice", &format!("{ED_C}\n{ED_D}")))
            .await,
    );
    let mut private = user_req("alice", "");
    private.key_file =
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n"
            .into();
    invalid(f.store.add_ssh_key(&private).await);
    let mut big = user_req("alice", ED_C);
    big.key_file = format!("{ED_C}\n{}", "#".repeat(ssh_keys::MAX_KEY_FILE));
    invalid(f.store.add_ssh_key(&big).await);
    assert_eq!(f.store.list_ssh_keys().await.unwrap().len(), 6);
}

#[tokio::test]
async fn requests_are_validated() {
    let f = fixture().await;
    let mut r = user_req("alice", ED_A);
    r.write = true;
    invalid(f.store.add_ssh_key(&r).await);
    let mut r = user_req("alice", ED_A);
    r.repo = Some("acme/web".into());
    invalid(f.store.add_ssh_key(&r).await);
    for user in ["", "a b", "pat:x", "../x", "a/b", &"u".repeat(65)] {
        invalid(f.store.add_ssh_key(&user_req(user, ED_A)).await);
    }
    let mut r = user_req("alice", ED_A);
    r.user = None;
    invalid(f.store.add_ssh_key(&r).await);
    for repo in ["acme/*", "*", "acme", "acme/web/x", ".a/b", ""] {
        invalid(f.store.add_ssh_key(&deploy_req(repo, false, ED_A)).await);
    }
    let mut r = deploy_req("acme/web", false, ED_A);
    r.user = Some("alice".into());
    invalid(f.store.add_ssh_key(&r).await);
    let mut r = deploy_req("acme/web", false, ED_A);
    r.repo = None;
    invalid(f.store.add_ssh_key(&r).await);
    let mut r = user_req("alice", ED_A);
    r.title = "bad\ntitle".into();
    invalid(f.store.add_ssh_key(&r).await);
    r.title = "t".repeat(129);
    invalid(f.store.add_ssh_key(&r).await);
    for secs in [0, -5] {
        let mut r = user_req("alice", ED_A);
        r.expires_in = Some(secs);
        invalid(f.store.add_ssh_key(&r).await);
    }
    let mut r = user_req("alice", ED_A);
    r.expires_in = Some(i64::MAX);
    invalid(f.store.add_ssh_key(&r).await);
    assert!(f.store.list_ssh_keys().await.unwrap().is_empty());
    // The comment becomes the title when none is given.
    let info = f.store.add_ssh_key(&user_req("alice", ED_A)).await.unwrap();
    assert_eq!(info.title, "ed25519@test");
}

#[tokio::test]
async fn the_row_holds_the_public_blob_and_no_private_material() {
    let f = fixture().await;
    f.store.add_ssh_key(&user_req("alice", ED_A)).await.unwrap();
    f.store
        .add_ssh_key(&deploy_req("acme/web", true, RSA_2048))
        .await
        .unwrap();
    let rows = sqlx::query("SELECT kind, principal, repo_id, algo, public_key, fingerprint, title FROM ssh_keys ORDER BY kind DESC")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        let blob: Vec<u8> = r.try_get("public_key").unwrap();
        let fp: Vec<u8> = r.try_get("fingerprint").unwrap();
        let algo: String = r.try_get("algo").unwrap();
        // The blob is the key's own wire encoding (it starts with the algorithm name), its
        // fingerprint is the SHA-256 of it, and it parses as a public key and nothing else.
        assert_eq!(SshFingerprint::of_blob(&blob).as_bytes().as_slice(), fp);
        assert_eq!(&blob[4..4 + algo.len()], algo.as_bytes());
        let key = ssh_keys::check_blob(&blob, DEFAULT_MIN_RSA_BITS).unwrap();
        assert_eq!(key.to_bytes().unwrap(), blob);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&blob),
            r.try_get::<String, _>("title").unwrap()
        );
        assert!(!text.contains("PRIVATE"));
    }
    let user: String = rows[0].try_get("principal").unwrap();
    assert_eq!(user, "pat:alice");
    let repo: Option<String> = rows[1].try_get("repo_id").unwrap();
    assert_eq!(repo.as_deref(), Some("acme/web"));
}

/// Inserts a row directly, as an operator or an older release might have.
async fn raw_row(f: &Fixture, kind: &str, principal: &str, repo: Option<&str>, line: &str) {
    let k = ssh_keys::parse_key_file(line, DEFAULT_MIN_RSA_BITS).unwrap();
    sqlx::query(
        "INSERT INTO ssh_keys (id, kind, principal, repo_id, \"write\", algo, public_key, \
         fingerprint, title, created) VALUES ($1, $2, $3, $4, 1, $5, $6, $7, '', $8)",
    )
    .bind(format!("raw{}", &k.fingerprint().to_string()[7..15]))
    .bind(kind.to_owned())
    .bind(principal.to_owned())
    .bind(repo.map(str::to_owned))
    .bind(k.algo.clone())
    .bind(k.blob.clone())
    .bind(k.fingerprint().as_bytes().to_vec())
    .bind(T0)
    .execute(&f.pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_row_without_a_typed_principal_is_refused() {
    let f = fixture().await;
    f.pat("alice", &["acme/web"], Access::Write, None).await;
    // Positive controls: a bare user name and a typed one both rebuild to `pat:alice`; a stored
    // principal text never becomes the identity of a deploy key.
    raw_row(&f, "user", "alice", None, ED_A).await;
    raw_row(&f, "user", "pat:alice", None, ED_B).await;
    raw_row(&f, "deploy", "forgejo-internal", Some("acme/web"), ED_C).await;
    assert_eq!(f.identify(ED_A).await.unwrap().principal.id, "pat:alice");
    assert_eq!(f.identify(ED_B).await.unwrap().principal.id, "pat:alice");
    assert_eq!(
        f.identify(ED_C).await.unwrap().principal.id,
        "deploy:acme/web"
    );
    // A user key can be named like the internal principal and still cannot be it.
    f.store
        .revoke_ssh_key(&key_of(ED_B).fingerprint().to_string())
        .await
        .unwrap();
    raw_row(&f, "user", "forgejo-internal", None, ED_D).await;
    assert_eq!(
        f.identify(ED_D).await.unwrap().principal.id,
        "pat:forgejo-internal"
    );

    let g = fixture().await;
    for (i, (kind, principal, repo)) in [
        ("user", "", None),
        ("user", "bad principal", None),
        ("user", "deploy:acme/web", None),
        ("user", "pat:pat:alice", None),
        ("user", "alice", Some("acme/web")),
        ("deploy", "deploy:acme/web", None),
        ("deploy", "x", Some("acme/*")),
        ("deploy", "x", Some("*")),
        ("deploy", "x", Some("not a repo")),
        ("admin", "alice", None),
        ("", "alice", None),
        ("USER", "alice", None),
    ]
    .into_iter()
    .enumerate()
    {
        let line = ed_line(2000 + u32::try_from(i).unwrap());
        sqlx::query("DELETE FROM ssh_keys")
            .execute(&g.pool)
            .await
            .unwrap();
        raw_row(&g, kind, principal, repo, &line).await;
        assert!(
            g.identify(&line).await.is_none(),
            "{kind:?} {principal:?} {repo:?}"
        );
        assert!(g.can(&line, "acme/web", Access::Read).await.is_none());
    }
}

#[tokio::test]
async fn a_row_that_disagrees_with_its_key_is_refused() {
    let f = fixture().await;
    f.pat("alice", &["acme/web"], Access::Write, None).await;
    raw_row(&f, "user", "alice", None, ED_A).await;
    assert!(f.identify(ED_A).await.is_some());
    f.advance(CACHE_TTL_SECS);
    // The public blob is swapped for another key's: the fingerprint no longer matches it.
    let other = ssh_keys::parse_key_file(ED_B, 2048).unwrap();
    sqlx::query("UPDATE ssh_keys SET public_key = $1")
        .bind(other.blob.clone())
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(f.identify(ED_A).await.is_none());
    assert!(f.can(ED_A, "acme/web", Access::Read).await.is_none());

    // A weak RSA key put in the table by hand is refused at login, not only at `add`.
    let g = fixture().await;
    g.pat("alice", &["acme/web"], Access::Write, None).await;
    let weak = PublicKeyLine::raw(RSA_1024);
    sqlx::query(
        "INSERT INTO ssh_keys (id, kind, principal, \"write\", algo, public_key, fingerprint, \
         title, created) VALUES ('w', 'user', 'alice', 0, 'ssh-rsa', $1, $2, '', $3)",
    )
    .bind(weak.blob.clone())
    .bind(SshFingerprint::of_blob(&weak.blob).as_bytes().to_vec())
    .bind(T0)
    .execute(&g.pool)
    .await
    .unwrap();
    let key = SshKey::new("ssh-rsa", &weak.blob).unwrap();
    assert_eq!(g.authz.identify_ssh_key(&key).await, Ok(None));
    assert_eq!(
        g.authz
            .authorize_ssh_key(&key, "acme/web", Access::Read)
            .await,
        Ok(Decision::Denied)
    );
}

/// The blob of a line without the policy check.
struct PublicKeyLine {
    blob: Vec<u8>,
}

impl PublicKeyLine {
    fn raw(line: &str) -> Self {
        let b64 = line.split_whitespace().nth(1).unwrap();
        Self {
            blob: base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap(),
        }
    }
}

#[tokio::test]
async fn the_tables_are_bounded() {
    let f = fixture().await;
    // Per owner.
    for i in 0..MAX_KEYS_PER_OWNER {
        f.store
            .add_ssh_key(&user_req("alice", &ed_line(u32::try_from(i).unwrap())))
            .await
            .unwrap();
    }
    let m = invalid(
        f.store
            .add_ssh_key(&user_req("alice", &ed_line(5000)))
            .await,
    );
    assert!(m.contains("100"), "{m}");
    f.store
        .add_ssh_key(&user_req("bob", &ed_line(5000)))
        .await
        .unwrap();

    // Overall: fill the live count with one statement, then one more key is refused.
    let live = i64::try_from(f.store.list_ssh_keys().await.unwrap().len()).unwrap();
    sqlx::query(
        "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < $1) \
         INSERT INTO ssh_keys (id, kind, principal, \"write\", algo, public_key, fingerprint, \
         title, created) \
         SELECT 'fill' || i, 'user', 'pat:filler' || i, 0, 'ssh-ed25519', x'00', \
                CAST('fill' || i AS BLOB), '', 1 FROM c",
    )
    .bind(MAX_LIVE_SSH_KEYS - live)
    .execute(&f.pool)
    .await
    .unwrap();
    let m = invalid(
        f.store
            .add_ssh_key(&user_req("carol", &ed_line(9000)))
            .await,
    );
    assert!(m.contains("full"), "{m}");
    // The filler rows were rejected as keys anyway: nothing authorises through them.
    assert!(f.identify(&ed_line(9000)).await.is_none());
}

#[tokio::test]
async fn long_dead_rows_are_deleted_when_a_key_is_added() {
    let f = fixture().await;
    let old = f.store.add_ssh_key(&user_req("alice", ED_A)).await.unwrap();
    f.store.revoke_ssh_key(&old.id).await.unwrap();
    let mut expiring = user_req("alice", ED_B);
    expiring.expires_in = Some(10);
    f.store.add_ssh_key(&expiring).await.unwrap();
    f.advance(DEAD_RETENTION_SECS - 1);
    f.store.add_ssh_key(&user_req("alice", ED_C)).await.unwrap();
    // Dead for under the retention: both are still listed, with their status.
    let listed = f.store.list_ssh_keys().await.unwrap();
    assert_eq!(listed.len(), 3);
    let now = f.store.now();
    let mut statuses: Vec<_> = listed.iter().map(|k| k.status(now)).collect();
    statuses.sort_unstable();
    assert_eq!(statuses, ["active", "expired", "revoked"]);
    f.advance(20);
    f.store.add_ssh_key(&user_req("alice", ED_D)).await.unwrap();
    let ids: Vec<_> = f
        .store
        .list_ssh_keys()
        .await
        .unwrap()
        .into_iter()
        .map(|k| k.fingerprint)
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(!ids.contains(&key_of(ED_A).fingerprint()));
    assert!(!ids.contains(&key_of(ED_B).fingerprint()));
}

#[tokio::test]
async fn authorizers_that_do_not_opt_in_refuse_and_a_failing_lookup_is_an_error() {
    // The static table never opts in.
    let table = asc_rpc::auth::StaticTokens::new().with_token("t", "alice", Access::Admin);
    let key = key_of(ED_A);
    assert_eq!(table.identify_ssh_key(&key).await, Ok(None));
    assert_eq!(
        table.authorize_ssh_key(&key, "a/b", Access::Read).await,
        Ok(Decision::Denied)
    );
    // The token store answers errors of its own lookup as errors, never as a grant.
    let f = fixture().await;
    sqlx::query("DROP TABLE ssh_keys")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.authz.identify_ssh_key(&key).await,
        Err(AuthError::Forbidden)
    );
    assert_eq!(
        f.authz.authorize_ssh_key(&key, "a/b", Access::Read).await,
        Err(AuthError::Forbidden)
    );
}

#[tokio::test]
async fn the_admission_wrapper_forwards_ssh_keys_to_the_inner_authorizer() {
    let f = fixture().await;
    f.pat("alice", &["acme/web"], Access::Write, None).await;
    f.store.add_ssh_key(&user_req("alice", ED_A)).await.unwrap();
    let admission = asc_serve::limits::Admission::new(&asc_serve::config::Limits::default());
    let wrapped = admission.recording(Arc::new(TokenStoreAuthz::new(f.store.clone())));
    let key = key_of(ED_A);
    assert_eq!(
        wrapped
            .identify_ssh_key(&key)
            .await
            .unwrap()
            .unwrap()
            .principal
            .id,
        "pat:alice"
    );
    assert!(matches!(
        wrapped
            .authorize_ssh_key(&key, "acme/web", Access::Write)
            .await,
        Ok(Decision::Allowed(_))
    ));
    assert_eq!(
        wrapped
            .authorize_ssh_key(&key, "acme/api", Access::Read)
            .await,
        Ok(Decision::Denied)
    );
}

fn config(dir: &std::path::Path) -> ServeConfig {
    let db = dir.join("asc.db").to_string_lossy().replace('\\', "/");
    let key = dir.join("token.key").to_string_lossy().replace('\\', "/");
    ServeConfig::parse(&format!(
        "store = \"memory:///\"\ndb = \"sqlite:{db}?mode=rwc\"\n[auth]\nprovider = \"token_store\"\ntoken_key_file = \"{key}\"\n"
    ))
    .unwrap()
}

#[tokio::test]
async fn the_cli_adds_lists_and_revokes() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    asc_serve::run_migrate(&cfg).await.unwrap();
    let write_key = |name: &str, text: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, text).unwrap();
        p
    };
    let user_args = |path| SshKeyAddArgs {
        user: Some("alice".into()),
        repo: None,
        deploy: false,
        write: false,
        key_file: path,
        title: "work laptop".into(),
        expires: Some("30d".into()),
    };

    let said = token_cli::ssh_key_add(&cfg, user_args(write_key("a.pub", ED_A)))
        .await
        .unwrap();
    assert!(said.contains("pat:alice"), "{said}");
    assert!(said.contains(&key_of(ED_A).fingerprint().to_string()));
    assert!(said.contains("0 grants"), "{said}");
    let deploy = token_cli::ssh_key_add(
        &cfg,
        SshKeyAddArgs {
            user: None,
            repo: Some("acme/web".into()),
            deploy: true,
            write: true,
            key_file: write_key("b.pub", &format!("# deploy key\n{ED_B}\n")),
            title: String::new(),
            expires: None,
        },
    )
    .await
    .unwrap();
    assert!(deploy.contains("acme/web (read and write)"), "{deploy}");

    // A running server (another authorizer over the database) accepts them.
    let server = asc_serve::authz::authorizer(&cfg).await.unwrap();
    let decision = server
        .authorize_ssh_key(&key_of(ED_B), "acme/web", Access::Write)
        .await
        .unwrap();
    assert!(matches!(decision, Decision::Allowed(p) if p.id == "deploy:acme/web"));

    let listed = token_cli::ssh_key_list(&cfg).await.unwrap();
    assert!(
        listed.contains("work laptop") && listed.contains("active"),
        "{listed}"
    );
    assert!(listed.contains(&key_of(ED_B).fingerprint().to_string()));
    assert!(!listed.contains(ED_A.split_whitespace().nth(1).unwrap()));

    // Refusals: a missing file, options, mixed flags, a weak key, a repeat.
    assert!(
        token_cli::ssh_key_add(&cfg, user_args(dir.path().join("missing.pub")))
            .await
            .is_err()
    );
    let m = token_cli::ssh_key_add(
        &cfg,
        user_args(write_key("c.pub", &format!("restrict {ED_C}"))),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(m.contains("options"), "{m}");
    assert!(
        token_cli::ssh_key_add(&cfg, user_args(write_key("d.pub", RSA_1024)))
            .await
            .is_err()
    );
    assert!(
        token_cli::ssh_key_add(&cfg, user_args(write_key("e.pub", ED_A)))
            .await
            .is_err()
    );
    let mut mixed = user_args(write_key("f.pub", ED_D));
    mixed.repo = Some("acme/web".into());
    assert!(token_cli::ssh_key_add(&cfg, mixed).await.is_err());
    let mut huge = user_args(write_key("g.pub", ED_D));
    std::fs::write(&huge.key_file, "#".repeat(ssh_keys::MAX_KEY_FILE + 1)).unwrap();
    huge.expires = None;
    assert!(token_cli::ssh_key_add(&cfg, huge).await.is_err());
    let mut bad_life = user_args(write_key("h.pub", ED_D));
    bad_life.expires = Some("soon".into());
    assert!(token_cli::ssh_key_add(&cfg, bad_life).await.is_err());

    let fp = key_of(ED_A).fingerprint().to_string();
    assert!(
        server
            .identify_ssh_key(&key_of(ED_A))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        token_cli::ssh_key_revoke(&cfg, &fp)
            .await
            .unwrap()
            .contains("revoked")
    );
    assert!(
        token_cli::ssh_key_revoke(&cfg, &fp)
            .await
            .unwrap()
            .contains("already")
    );
    assert!(
        token_cli::ssh_key_revoke(&cfg, "ffffffffffffffff")
            .await
            .is_err()
    );
    assert!(
        token_cli::ssh_key_list(&cfg)
            .await
            .unwrap()
            .contains("revoked")
    );
}
