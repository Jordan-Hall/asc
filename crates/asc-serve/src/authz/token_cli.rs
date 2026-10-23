//! `asc serve token create|list|revoke` (SPEC §15.1; PLAN M30): the operator's side of the token
//! store. The `asc` binary parses the flags into [`CreateArgs`] and prints what these functions
//! return; the logic lives here so it is tested with the store.
//!
//! `create` is the only place a token is ever shown: [`Created::token`] goes to stdout, the
//! summary (id, principal, grants, expiry) to stderr, and nothing keeps either.

use std::fmt::Write as _;

use asc_rpc::auth::Access;

#[cfg(feature = "git-ssh")]
use crate::authz::ssh_keys;
use crate::authz::token_store::{
    Grant, NewToken, TokenError, TokenInfo, TokenKey, TokenKind, TokenStore,
};
use crate::config::ServeConfig;
use crate::migrate;

/// Connections the CLI opens.
const CLI_DB_CONNECTIONS: u32 = 2;

/// The flags of `asc serve token create`.
#[derive(Clone, Debug)]
pub struct CreateArgs {
    pub kind: TokenKind,
    pub user: Option<String>,
    pub repos: Vec<String>,
    pub access: Access,
    /// `30d`, `12h`, `90m`, `3600s` or `2w`.
    pub expires: Option<String>,
    pub label: String,
}

/// What `create` produced. `Debug` is not implemented: the token must not reach a log.
pub struct Created {
    /// The token, shown once.
    pub token: String,
    /// The id, principal, grants and expiry, for the operator.
    pub summary: String,
}

/// Parses a lifetime such as `30d`, `12h`, `90m`, `3600s` or `2w` into seconds.
pub fn parse_lifetime(s: &str) -> Result<i64, TokenError> {
    let bad = || {
        TokenError::Invalid(format!(
            "`{s}` is not a lifetime such as 30d, 12h, 90m or 2w"
        ))
    };
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (n, unit) = s.split_at(split);
    let n: i64 = n.parse().map_err(|_| bad())?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(bad()),
    };
    n.checked_mul(mult).filter(|v| *v > 0).ok_or_else(bad)
}

async fn open(cfg: &ServeConfig, create_key: bool) -> Result<TokenStore, TokenError> {
    let Some(db) = &cfg.db else {
        return Err(TokenError::Invalid("`db` is required".into()));
    };
    let Some(path) = &cfg.auth.token_key_file else {
        return Err(TokenError::Invalid(
            "`auth.token_key_file` is required".into(),
        ));
    };
    let key = if create_key {
        TokenKey::load_or_create(path)?
    } else {
        TokenKey::load(path)?
    };
    let pool = migrate::connect(db, CLI_DB_CONNECTIONS).await?;
    migrate::ensure_current(&pool)
        .await
        .map_err(|e| TokenError::Invalid(e.to_string()))?;
    Ok(TokenStore::new(pool, key))
}

/// `YYYY-MM-DDTHH:MM:SSZ` for unix seconds.
pub fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn grants_text(grants: &[Grant]) -> String {
    let mut parts: Vec<String> = grants
        .iter()
        .map(|g| {
            let a = match g.access {
                Access::Read => "read",
                Access::Write => "write",
                Access::Admin => "admin",
            };
            format!("{}:{a}", g.repo)
        })
        .collect();
    parts.sort();
    parts.join(",")
}

fn expiry_text(expires: Option<i64>) -> String {
    expires.map_or_else(|| "never".to_owned(), format_utc)
}

/// `asc serve token create`. Makes the key file when it does not exist yet.
pub async fn create(cfg: &ServeConfig, args: CreateArgs) -> Result<Created, TokenError> {
    let store = open(cfg, true).await?;
    let expires_in = args.expires.as_deref().map(parse_lifetime).transpose()?;
    let issued = store
        .create(&NewToken {
            kind: args.kind,
            user: args.user,
            repos: args.repos,
            access: args.access,
            expires_in,
            label: args.label,
        })
        .await?;
    let mut summary = String::new();
    let _ = writeln!(summary, "id:        {}", issued.id);
    let _ = writeln!(summary, "kind:      {}", issued.kind.as_str());
    let _ = writeln!(summary, "principal: {}", issued.principal);
    let _ = writeln!(summary, "grants:    {}", grants_text(&issued.grants));
    let _ = writeln!(summary, "expires:   {}", expiry_text(issued.expires));
    summary.push_str("This is the only time the token is shown; it cannot be recovered.\n");
    Ok(Created {
        token: issued.secret().to_owned(),
        summary,
    })
}

fn list_text(rows: &[TokenInfo], now: i64) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<16}  {:<6}  {:<8}  {:<20}  {:<20}  GRANTS  LABEL",
        "ID", "KIND", "STATUS", "PRINCIPAL", "EXPIRES"
    );
    for r in rows {
        let _ = writeln!(
            out,
            "{:<16}  {:<6}  {:<8}  {:<20}  {:<20}  {}  {}",
            r.id,
            r.kind.as_str(),
            r.status(now),
            r.principal,
            expiry_text(r.expires),
            grants_text(&r.grants),
            r.label
        );
    }
    out
}

/// `asc serve token list`: one line per token, never a secret.
pub async fn list(cfg: &ServeConfig) -> Result<String, TokenError> {
    let store = open(cfg, false).await?;
    let rows = store.list().await?;
    Ok(list_text(&rows, store.now()))
}

/// `asc serve token revoke <id>`.
pub async fn revoke(cfg: &ServeConfig, id: &str) -> Result<String, TokenError> {
    let store = open(cfg, false).await?;
    Ok(if store.revoke(id).await? {
        format!("revoked {id}; running servers stop accepting it within 5 seconds\n")
    } else {
        format!("{id} was already revoked\n")
    })
}

// ------------------------------------------------------------------------------ ssh-key

/// The flags of `asc serve ssh-key add` (SPEC §25.11.3; PLAN U84).
#[cfg(feature = "git-ssh")]
#[derive(Clone, Debug)]
pub struct SshKeyAddArgs {
    /// The user a user key acts as.
    pub user: Option<String>,
    /// The repository a deploy key is bound to.
    pub repo: Option<String>,
    /// Registers a deploy key (needs `repo`); without it the key is a user key (needs `user`).
    pub deploy: bool,
    /// A deploy key may push.
    pub write: bool,
    /// The public key file: one `authorized_keys` line.
    pub key_file: std::path::PathBuf,
    pub title: String,
    /// `30d`, `12h`, `90m`, `3600s` or `2w`.
    pub expires: Option<String>,
}

/// Reads at most [`ssh_keys::MAX_KEY_FILE`] bytes of the key file.
#[cfg(feature = "git-ssh")]
fn read_key_file(path: &std::path::Path) -> Result<String, TokenError> {
    use std::io::Read as _;
    let bad = |e: std::io::Error| TokenError::Invalid(format!("{}: {e}", path.display()));
    let file = std::fs::File::open(path).map_err(bad)?;
    let mut bytes = Vec::new();
    file.take(ssh_keys::MAX_KEY_FILE as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(bad)?;
    if bytes.len() > ssh_keys::MAX_KEY_FILE {
        return Err(TokenError::Invalid(format!(
            "{}: a key file is at most {} bytes",
            path.display(),
            ssh_keys::MAX_KEY_FILE
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| TokenError::Invalid(format!("{}: not a text file", path.display())))
}

/// `asc serve ssh-key add`.
#[cfg(feature = "git-ssh")]
pub async fn ssh_key_add(cfg: &ServeConfig, args: SshKeyAddArgs) -> Result<String, TokenError> {
    use asc_rpc::auth::SshKeyKind;

    let kind = if args.deploy {
        SshKeyKind::Deploy
    } else {
        if args.repo.is_some() || args.write {
            return Err(TokenError::Invalid(
                "`--repo` and `--write` are for deploy keys; add `--deploy`".into(),
            ));
        }
        SshKeyKind::User
    };
    let key_file = read_key_file(&args.key_file)?;
    let expires_in = args.expires.as_deref().map(parse_lifetime).transpose()?;
    let store = open(cfg, true).await?;
    let info = store
        .add_ssh_key(&ssh_keys::NewSshKey {
            kind,
            user: args.user.clone(),
            repo: args.repo,
            write: args.write,
            key_file,
            title: args.title,
            expires_in,
            min_rsa_bits: ssh_keys::DEFAULT_MIN_RSA_BITS,
        })
        .await?;
    let grants = match (&kind, args.user.as_deref()) {
        (SshKeyKind::User, Some(user)) => Some(store.live_grants_of(user).await?),
        _ => None,
    };
    Ok(ssh_keys::add_text(&info, grants))
}

/// `asc serve ssh-key list`: one line per key, never a blob.
#[cfg(feature = "git-ssh")]
pub async fn ssh_key_list(cfg: &ServeConfig) -> Result<String, TokenError> {
    let store = open(cfg, false).await?;
    let rows = store.list_ssh_keys().await?;
    Ok(ssh_keys::list_text(&rows, store.now()))
}

/// `asc serve ssh-key revoke <fingerprint|id>`.
#[cfg(feature = "git-ssh")]
pub async fn ssh_key_revoke(cfg: &ServeConfig, selector: &str) -> Result<String, TokenError> {
    let store = open(cfg, false).await?;
    Ok(if store.revoke_ssh_key(selector).await? {
        format!(
            "revoked {selector}; running servers stop accepting it within 5 seconds
"
        )
    } else {
        format!(
            "{selector} was already revoked
"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetimes_parse() {
        assert_eq!(parse_lifetime("30d").unwrap(), 30 * 86_400);
        assert_eq!(parse_lifetime("12h").unwrap(), 43_200);
        assert_eq!(parse_lifetime("90m").unwrap(), 5400);
        assert_eq!(parse_lifetime("2w").unwrap(), 14 * 86_400);
        assert_eq!(parse_lifetime("45s").unwrap(), 45);
        for bad in [
            "",
            "d",
            "10",
            "0d",
            "-1d",
            "1y",
            "1.5h",
            "9223372036854775807w",
        ] {
            assert!(parse_lifetime(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
    }
}
