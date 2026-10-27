//! Access tokens and SSH keys are audited in the transaction that changes them, with the public
//! id and never the secret.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sqlx::AnyPool;

use super::store::{self, Filter};
use super::{Event, Scope};
use crate::authz::token_store::{NewToken, TokenError, TokenKey, TokenKind, TokenStore};
use crate::migrate;
use asc_rpc::auth::Access;

async fn store_and_pool() -> (TokenStore, AnyPool) {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    (
        TokenStore::new(pool.clone(), TokenKey::generate().unwrap()),
        pool,
    )
}

async fn events(pool: &AnyPool, scope: &Scope, action: &str) -> Vec<Event> {
    let filter = Filter {
        actions: vec![action.to_owned()],
        ..Filter::default()
    };
    store::list(pool, scope, &filter, true, 100, None)
        .await
        .unwrap()
        .events
}

fn pat(user: &str, label: &str) -> NewToken {
    NewToken {
        kind: TokenKind::Pat,
        user: Some(user.into()),
        repos: vec!["acme/app".into()],
        access: Access::Write,
        expires_in: Some(3600),
        label: label.into(),
    }
}

#[tokio::test]
async fn a_token_is_audited_when_created_and_when_revoked_and_the_secret_never_is() {
    let (tokens, pool) = store_and_pool().await;
    let issued = tokens.create(&pat("alice", "laptop")).await.unwrap();
    let secret = issued.secret().to_owned();
    let created = events(&pool, &Scope::instance(), "auth.token.created").await;
    assert_eq!(created.len(), 1);
    let e = &created[0];
    assert_eq!(
        (e.actor.kind.as_str(), e.actor.name.as_str()),
        ("system", "operator")
    );
    assert_eq!(e.details.get("token"), Some(&issued.id));
    assert_eq!(e.details.get("name").map(String::as_str), Some("laptop"));
    assert_eq!(e.details.get("kind").map(String::as_str), Some("pat"));
    assert!(e.details.contains_key("expires_at"));
    // The token's user can see it in their own scope.
    assert_eq!(
        events(&pool, &Scope::owner("alice"), "auth.token.created")
            .await
            .len(),
        1
    );

    assert!(tokens.revoke(&issued.id).await.unwrap());
    let revoked = events(&pool, &Scope::instance(), "auth.token.revoked").await;
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0].details.get("token"), Some(&issued.id));
    assert_eq!(
        revoked[0].details.get("name").map(String::as_str),
        Some("laptop")
    );
    // Revoking again, or an unknown token, changes nothing and writes nothing.
    assert!(!tokens.revoke(&issued.id).await.unwrap());
    assert!(matches!(
        tokens.revoke("nosuchtoken").await,
        Err(TokenError::NotFound)
    ));
    assert_eq!(
        events(&pool, &Scope::instance(), "auth.token.revoked")
            .await
            .len(),
        1
    );

    // Nothing in the whole log holds the secret, or the digest it is stored under.
    let all = store::list(
        &pool,
        &Scope::instance(),
        &Filter::default(),
        true,
        100,
        None,
    )
    .await
    .unwrap()
    .events;
    let text = format!("{all:?}");
    assert!(!text.contains(&secret));
    assert!(!text.contains(secret.rsplit('_').next().unwrap()));
    let v = store::verify_chain(&pool, &Scope::instance(), 0, 0)
        .await
        .unwrap();
    assert!(v.ok, "{}", v.reason);
}

#[tokio::test]
async fn a_deploy_token_is_audited_in_the_scope_of_its_repository() {
    let (tokens, pool) = store_and_pool().await;
    let issued = tokens
        .create(&NewToken {
            kind: TokenKind::Deploy,
            user: None,
            repos: vec!["acme/app".into()],
            access: Access::Read,
            expires_in: None,
            label: "ci".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        events(&pool, &Scope::repo("acme/app"), "auth.token.created")
            .await
            .len(),
        1
    );
    assert_eq!(
        events(&pool, &Scope::owner("acme"), "auth.token.created")
            .await
            .len(),
        1
    );
    assert!(tokens.revoke(&issued.id).await.unwrap());
    assert_eq!(
        events(&pool, &Scope::repo("acme/app"), "auth.token.revoked")
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn a_refused_token_leaves_no_event() {
    let (tokens, pool) = store_and_pool().await;
    let mut bad = pat("alice", "x");
    bad.repos.clear();
    assert!(tokens.create(&bad).await.is_err());
    let total = store::list(
        &pool,
        &Scope::instance(),
        &Filter::default(),
        true,
        100,
        None,
    )
    .await
    .unwrap()
    .events;
    assert!(total.is_empty());
}

#[cfg(feature = "git-ssh")]
mod ssh {
    use super::*;
    use crate::authz::ssh_keys::NewSshKey;
    use asc_rpc::auth::SshKeyKind;

    const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti foo@bar.com";

    fn user_key() -> NewSshKey {
        NewSshKey {
            kind: SshKeyKind::User,
            user: Some("alice".into()),
            repo: None,
            write: false,
            key_file: ED25519.into(),
            title: "work laptop".into(),
            expires_in: None,
            min_rsa_bits: 3072,
        }
    }

    #[tokio::test]
    async fn an_ssh_key_is_audited_when_added_and_when_revoked() {
        let (tokens, pool) = store_and_pool().await;
        let info = tokens.add_ssh_key(&user_key()).await.unwrap();
        let added = events(&pool, &Scope::owner("alice"), "auth.ssh_key.added").await;
        assert_eq!(added.len(), 1);
        assert_eq!(
            added[0].details.get("fingerprint"),
            Some(&info.fingerprint.to_string())
        );
        assert_eq!(
            added[0].details.get("title").map(String::as_str),
            Some("work laptop")
        );
        // The same key twice is refused and writes nothing.
        assert!(tokens.add_ssh_key(&user_key()).await.is_err());
        assert_eq!(
            events(&pool, &Scope::instance(), "auth.ssh_key.added")
                .await
                .len(),
            1
        );

        // Revoked by fingerprint, once.
        assert!(
            tokens
                .revoke_ssh_key(&info.fingerprint.to_string())
                .await
                .unwrap()
        );
        assert!(
            !tokens
                .revoke_ssh_key(&info.fingerprint.to_string())
                .await
                .unwrap()
        );
        assert!(matches!(
            tokens.revoke_ssh_key("nosuchkey").await,
            Err(TokenError::NotFound)
        ));
        let removed = events(&pool, &Scope::owner("alice"), "auth.ssh_key.removed").await;
        assert_eq!(removed.len(), 1);
        assert_eq!(
            removed[0].details.get("fingerprint"),
            Some(&info.fingerprint.to_string())
        );
        let v = store::verify_chain(&pool, &Scope::instance(), 0, 0)
            .await
            .unwrap();
        assert!(v.ok, "{}", v.reason);
    }

    #[tokio::test]
    async fn an_ssh_key_revoked_by_id_is_audited_too() {
        let (tokens, pool) = store_and_pool().await;
        let info = tokens.add_ssh_key(&user_key()).await.unwrap();
        assert!(tokens.revoke_ssh_key(&info.id).await.unwrap());
        assert_eq!(
            events(&pool, &Scope::instance(), "auth.ssh_key.removed")
                .await
                .len(),
            1
        );
    }
}
