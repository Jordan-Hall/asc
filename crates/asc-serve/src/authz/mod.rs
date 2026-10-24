//! Authorizer implementations that belong to `asc serve` itself (SPEC §15.1; PLAN M30).
//!
//! [`authorizer`] builds the one [`Authorizer`] the `[auth] provider` key names; every surface of
//! the server (RPC, `/v8`, `/seeds`, admin and `/mcp`) is handed that one value.

pub mod identity;
pub(crate) mod key_file;
#[cfg(feature = "git-ssh")]
pub mod ssh_keys;
pub mod token_cli;
pub mod token_store;

use std::sync::Arc;

use asc_rpc::auth::Authorizer;

use crate::config::{AuthProvider, ConfigError, Role, ServeConfig};
use crate::{DenyAll, ServeError, migrate};

pub use identity::{
    HOST_SERVICE_PRINCIPAL, LEGACY_HOST_SERVICE_PRINCIPAL, NEGATIVE_TTL, POSITIVE_TTL,
    UserIdentity, is_host_service,
};
pub use token_store::{TokenKey, TokenStore, TokenStoreAuthz};

/// Pool size of the token store's own connections.
const TOKEN_DB_CONNECTIONS: u32 = 4;

/// The authorizer for `cfg`: the token store, forge delegation, or (neither configured) one that
/// refuses every credential. Only the primary role authorises locally.
pub async fn authorizer(cfg: &ServeConfig) -> Result<Arc<dyn Authorizer>, ServeError> {
    if cfg.role != Role::Primary {
        return Ok(Arc::new(DenyAll));
    }
    let invalid = |e: String| ServeError::Config(ConfigError::Invalid(e));
    match cfg.auth_provider() {
        Some(AuthProvider::TokenStore) => {
            let (Some(db), Some(key)) = (&cfg.db, &cfg.auth.token_key_file) else {
                return Err(invalid(
                    "the token store needs `db` and `auth.token_key_file`".into(),
                ));
            };
            let key = TokenKey::load(key).map_err(|e| invalid(e.to_string()))?;
            let pool = migrate::connect(db, TOKEN_DB_CONNECTIONS)
                .await
                .map_err(|e| invalid(e.to_string()))?;
            migrate::ensure_current(&pool).await?;
            Ok(Arc::new(TokenStoreAuthz::new(Arc::new(TokenStore::new(
                pool, key,
            )))))
        }
        #[cfg(feature = "forgejo")]
        Some(AuthProvider::Forgejo) => crate::forgejo::authorizer(cfg)
            .map_err(|e| invalid(e.to_string()))?
            .ok_or_else(|| invalid("auth.provider = \"forgejo\" needs a [forgejo] section".into())),
        #[cfg(not(feature = "forgejo"))]
        Some(AuthProvider::Forgejo) => Err(invalid(
            "auth.provider = \"forgejo\" needs a build with the `forgejo` feature".into(),
        )),
        None => Ok(Arc::new(DenyAll)),
    }
}
