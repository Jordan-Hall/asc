//! The daemon's server tier for turbo (docs/designs/build-cache.md §4.3): a turbo front miss is
//! fetched from the build cache's server before it is answered, and an upload goes on to the
//! server in the background when the tier is read-write.
//!
//! Turbo names its team in every request (`?slug=`). `asc cache env` run in a work tree sets
//! `TURBO_TEAM` to the team of that tree (`asc-` and 16 hex digits of a hash of its root) and
//! records the team and the root in `<ASC_HOME>/buildcache-teams.json`. The daemon opens the
//! work tree of a team the way any command does, so the tree's own configuration decides the
//! server (`asc.buildcache.server`, else `origin`), the credentials and the mode
//! (`asc.buildcache.remote`). A team that is not recorded, or a tree with no server, is served
//! from the local store alone.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use asc_buildcache::{ActionKey, Store};
use asc_state::AscHome;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use super::remote::{self, Fetched, Mode};
use crate::cli::ci::cache::HttpApi;

/// The teams file's name in the asc home.
const FILE: &str = "buildcache-teams.json";
/// How long a team whose server could not be opened is served locally before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How long an opened server tier is used before the tree's configuration is read again.
const REFRESH_AFTER: Duration = Duration::from_secs(600);
/// Uploads in flight at once.
const UPLOADS: usize = 4;
/// Teams recorded at most; the oldest recorded goes first.
const MAX_TEAMS: usize = 256;

/// The server tier the turbo front consults, per team.
pub(crate) trait Upstream: Send + Sync {
    /// Fetches `key` from `team`'s server into `store`; true when the entry is there now.
    fn fetch<'a>(
        &'a self,
        team: &'a str,
        store: &'a Store,
        key: &'a ActionKey,
    ) -> BoxFuture<'a, bool>;

    /// Uploads `key` from `store` to `team`'s server in the background, when its tier is
    /// read-write.
    fn send(&self, team: String, store: Store, key: ActionKey);
}

/// The team of the work tree at `root`.
pub(crate) fn team_of(root: &Path) -> String {
    let hash = blake3::hash(root.to_string_lossy().as_bytes()).to_hex();
    format!("asc-{}", &hash.as_str()[..16])
}

/// Whether `team` has the form [`team_of`] gives.
pub(crate) fn is_team(team: &str) -> bool {
    team.strip_prefix("asc-").is_some_and(|h| {
        h.len() == 16
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Teams {
    /// Team to work tree root, with the time it was recorded (seconds since the epoch).
    #[serde(default)]
    teams: BTreeMap<String, Recorded>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Recorded {
    root: PathBuf,
    #[serde(default)]
    at: u64,
}

fn path(home: &AscHome) -> PathBuf {
    home.root().join(FILE)
}

fn load(home: &AscHome) -> Teams {
    std::fs::read(path(home))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Records the work tree at `root` as a team and returns the team.
pub(crate) fn register(home: &AscHome, root: &Path) -> std::io::Result<String> {
    let root = std::path::absolute(root)?;
    let team = team_of(&root);
    let mut teams = load(home);
    if teams.teams.get(&team).is_some_and(|r| r.root == root) {
        return Ok(team);
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    teams.teams.insert(team.clone(), Recorded { root, at });
    while teams.teams.len() > MAX_TEAMS {
        let Some(oldest) = teams
            .teams
            .iter()
            .min_by_key(|(_, r)| r.at)
            .map(|(t, _)| t.clone())
        else {
            break;
        };
        teams.teams.remove(&oldest);
    }
    let file = path(home);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&teams)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    if let Err(e) = std::fs::rename(&tmp, &file) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(team)
}

/// The work tree root recorded for `team`.
fn root_of(home: &AscHome, team: &str) -> Option<PathBuf> {
    load(home).teams.remove(team).map(|r| r.root)
}

/// One team's server tier.
struct Tier {
    api: Option<HttpApi>,
    mode: Mode,
}

enum Slot {
    Open(Arc<Tier>, Instant),
    Failed(Instant),
}

struct Inner {
    home: AscHome,
    slots: Mutex<HashMap<String, Slot>>,
    uploads: Arc<Semaphore>,
}

/// The daemon's [`Upstream`]: the server of each team's work tree.
#[derive(Clone)]
pub(crate) struct ServerTier {
    inner: Arc<Inner>,
}

impl ServerTier {
    pub(crate) fn new(home: AscHome) -> Self {
        Self {
            inner: Arc::new(Inner {
                home,
                slots: Mutex::new(HashMap::new()),
                uploads: Arc::new(Semaphore::new(UPLOADS)),
            }),
        }
    }
}

impl Inner {
    /// The tier of `team`: from the memo while it is fresh, else opened from its work tree.
    async fn tier(&self, team: &str) -> Option<Arc<Tier>> {
        if !is_team(team) {
            return None;
        }
        let now = Instant::now();
        {
            let slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            match slots.get(team) {
                Some(Slot::Open(t, at)) if now.duration_since(*at) < REFRESH_AFTER => {
                    return Some(t.clone());
                }
                Some(Slot::Failed(at)) if now.duration_since(*at) < RETRY_AFTER => return None,
                _ => {}
            }
        }
        let opened = self.open(team).await;
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        match opened {
            Some(t) => {
                let t = Arc::new(t);
                slots.insert(team.to_owned(), Slot::Open(t.clone(), now));
                Some(t)
            }
            None => {
                slots.insert(team.to_owned(), Slot::Failed(now));
                None
            }
        }
    }

    /// Opens the work tree of `team` and its server. The context is not `Send`, so it is built and
    /// used on a blocking thread; only the tier comes back.
    async fn open(&self, team: &str) -> Option<Tier> {
        let root = root_of(&self.home, team)?;
        let home = self.home.clone();
        let rt = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            rt.block_on(async move {
                let ctx = crate::daemon::cloud::prod::open_ctx(&home, root)
                    .await
                    .ok()?;
                let mode = remote::mode(&ctx).ok()?;
                if mode == Mode::Off {
                    return Some(Tier { api: None, mode });
                }
                let api = remote::api(&ctx).await.ok()?;
                Some(Tier {
                    api: Some(api),
                    mode,
                })
            })
        })
        .await
        .ok()
        .flatten()
    }
}

impl Upstream for ServerTier {
    fn fetch<'a>(
        &'a self,
        team: &'a str,
        store: &'a Store,
        key: &'a ActionKey,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let Some(tier) = self.inner.tier(team).await else {
                return false;
            };
            let Some(api) = &tier.api else {
                return false;
            };
            // An unreachable server is a miss: the build goes on.
            matches!(remote::fetch(api, store, key).await, Ok(Fetched::Stored(_)))
        })
    }

    fn send(&self, team: String, store: Store, key: ActionKey) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let Some(tier) = inner.tier(&team).await else {
                return;
            };
            let (Some(api), Mode::ReadWrite) = (&tier.api, tier.mode) else {
                return;
            };
            let Ok(_permit) = inner.uploads.clone().acquire_owned().await else {
                return;
            };
            // The outcome is recorded for `asc cache status`; a failure leaves the entry local.
            let _ = remote::send(api, &store, &key).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_team_names_its_tree_and_the_file_maps_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let home = AscHome::open(dir.path().join("home")).unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let ta = register(&home, &a).unwrap();
        let tb = register(&home, &b).unwrap();
        assert!(is_team(&ta) && is_team(&tb), "{ta} {tb}");
        assert_ne!(ta, tb);
        assert_eq!(ta, team_of(&std::path::absolute(&a).unwrap()));
        assert_eq!(register(&home, &a).unwrap(), ta);
        assert_eq!(root_of(&home, &ta), Some(std::path::absolute(&a).unwrap()));
        assert_eq!(root_of(&home, "asc-0000000000000000"), None);
        for bad in [
            "asc",
            "asc-0123",
            "asc-0123456789ABCDEF",
            "x-0123456789abcdef",
        ] {
            assert!(!is_team(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn an_unknown_or_malformed_team_has_no_tier_and_is_not_retried_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = AscHome::open(dir.path().join("home")).unwrap();
        let tier = ServerTier::new(home);
        let store = Store::open(dir.path().join("s")).unwrap();
        let key = ActionKey([7; 32]);
        assert!(!tier.fetch("asc", &store, &key).await);
        assert!(!tier.fetch("asc-0123456789abcdef", &store, &key).await);
        let slots = tier.inner.slots.lock().unwrap();
        assert!(matches!(
            slots.get("asc-0123456789abcdef"),
            Some(Slot::Failed(_))
        ));
        assert!(!slots.contains_key("asc"));
    }
}
