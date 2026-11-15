//! The worktree report of this device (SPEC §21.3.10): after each cloud sync the daemon tells the
//! server which worktrees of the repository this device holds, metadata only.
//!
//! * `ws_hash` is BLAKE3 over a domain tag, the per-device salt and the worktree path, 32 bytes.
//!   The salt is 32 random bytes kept in `<asc home>/worktree-salt` (mode 0600 on Unix), made on
//!   first use. `meta.machine_id` is not used: it is written to buckets, so it is not secret. A
//!   path, a file name, the salt and content never go on the wire.
//! * `head_ref` is sent only when it passes the server's `git check-ref-format` rule, else empty.
//! * At most [`MAX_WORKTREES`] rows, the most recently used first.
//! * Ahead and behind need the commit graph, which only the `ws-api` child has; the daemon sends 0.

use std::path::Path;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_rpc::client::CallError;
use asc_rpc::server::wire;
use asc_state::state::StateDb;
use asc_state::state::workspaces::{self, Mode};
use asc_store::refs::{Refs as _, RepoKey};
use async_trait::async_trait;
use tonic::Code;
use uuid::Uuid;

use super::{RpcCloudStore, note};

/// Worktrees in one report, at most (SPEC §21.3.10).
pub const MAX_WORKTREES: usize = 256;
const MAX_HEAD_REF: usize = 256;
const MAX_MODE: usize = 32;
const MAX_IN_PROGRESS: usize = 64;
const SALT_FILE: &str = "worktree-salt";
const SALT_LEN: usize = 32;
const DOMAIN: &[u8] = b"asc worktree report v1\0";

/// What the daemon knows locally about one worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalWorktree {
    pub path: String,
    pub mode: String,
    pub head_ref: String,
    pub head: Option<ChangesetId>,
    pub dirty: bool,
    pub ahead: u32,
    pub behind: u32,
    pub ephemeral_until: i64,
    pub in_progress: String,
    /// Unix seconds of the last use; 0 when unknown.
    pub last_use: i64,
}

/// Why a send failed.
#[derive(Debug)]
pub enum SendError {
    /// The repository has no `cloud` flag on the server (`FAILED_PRECONDITION`).
    Precondition(String),
    Other(String),
}

/// Sends the worktree report of one device.
#[async_trait]
pub trait WorktreeReporter: Send + Sync {
    async fn report(&self, req: &wire::ReportWorktreesRequest) -> Result<(), SendError>;
}

#[async_trait]
impl WorktreeReporter for RpcCloudStore {
    async fn report(&self, req: &wire::ReportWorktreesRequest) -> Result<(), SendError> {
        let mut req = req.clone();
        req.repo = Some(self.repo_ref());
        match self
            .stub()
            .report_worktrees(self.wire.request(req, None))
            .await
        {
            Ok(_) => Ok(()),
            Err(s) => {
                let precondition = s.code() == Code::FailedPrecondition;
                let text = note(&CallError::from(s).to_string());
                Err(if precondition {
                    SendError::Precondition(text)
                } else {
                    SendError::Other(text)
                })
            }
        }
    }
}

/// The per-device salt, made on first use. The content is written to a temp file in the same
/// directory, synced, then linked to the final name without overwriting, so a crash leaves no
/// short file and racing writers agree on one salt. A file of the wrong length is replaced once.
pub fn salt(root: &Path) -> Result<[u8; SALT_LEN], String> {
    let file = root.join(SALT_FILE);
    let read = |f: &Path| -> Option<[u8; SALT_LEN]> { std::fs::read(f).ok()?.try_into().ok() };
    if let Some(s) = read(&file) {
        return Ok(s);
    }
    if file.exists() {
        eprintln!("asc daemon: cloud: worktree salt has the wrong length; replacing it");
        std::fs::remove_file(&file).map_err(|e| note(&e.to_string()))?;
    }
    let mut fresh = [0u8; SALT_LEN];
    fresh[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    fresh[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    let tmp = root.join(format!("{SALT_FILE}.{}.tmp", Uuid::new_v4().simple()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let written = (|| {
        use std::io::Write;
        let mut f = opts.open(&tmp)?;
        f.write_all(&fresh)?;
        f.sync_all()
    })();
    let linked = written.and_then(|()| std::fs::hard_link(&tmp, &file));
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(fresh),
        // Another run made it first: its salt is the one.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            read(&file).ok_or_else(|| "worktree salt unreadable".to_owned())
        }
        Err(e) => Err(note(&e.to_string())),
    }
}

/// The newest `MAX_WORKTREES` of `items` by last use (ties keep their order).
fn newest<T>(mut items: Vec<T>, last_use: impl Fn(&T) -> i64) -> Vec<T> {
    items.sort_by_key(|i| std::cmp::Reverse(last_use(i)));
    items.truncate(MAX_WORKTREES);
    items
}

/// The opaque id of a worktree on this device.
pub fn ws_hash(salt: &[u8; SALT_LEN], path: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(DOMAIN);
    h.update(salt);
    h.update(path.as_bytes());
    *h.finalize().as_bytes()
}

/// `git check-ref-format`, the server's rule (`asc-serve` `ref_text`).
fn ref_ok(raw: &str) -> bool {
    !(raw.len() > MAX_HEAD_REF
        || raw == "@"
        || raw.starts_with(['/', '-'])
        || raw.ends_with(['/', '.'])
        || raw.contains("..")
        || raw.contains("//")
        || raw.contains("@{")
        || raw.chars().any(|c| {
            c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
        || raw
            .split('/')
            .any(|p| p.starts_with('.') || p.ends_with(".lock")))
}

fn cut(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// The request for `device`: no path, salt or content in it.
pub fn build(
    salt: &[u8; SALT_LEN],
    device: &str,
    rows: &[LocalWorktree],
) -> wire::ReportWorktreesRequest {
    let mut newest: Vec<&LocalWorktree> = rows.iter().collect();
    newest.sort_by(|a, b| {
        b.last_use
            .cmp(&a.last_use)
            .then_with(|| a.path.cmp(&b.path))
    });
    newest.truncate(MAX_WORKTREES);
    wire::ReportWorktreesRequest {
        repo: None,
        device: device.to_owned(),
        worktrees: newest
            .into_iter()
            .map(|w| wire::WorktreeReport {
                ws_hash: ws_hash(salt, &w.path).to_vec().into(),
                head_ref: if ref_ok(&w.head_ref) {
                    w.head_ref.clone()
                } else {
                    String::new()
                },
                head: w
                    .head
                    .map(|h| h.as_bytes().to_vec())
                    .unwrap_or_default()
                    .into(),
                mode: cut(&w.mode, MAX_MODE),
                dirty: w.dirty,
                ahead: w.ahead,
                behind: w.behind,
                ephemeral_until: w.ephemeral_until.max(0),
                in_progress: cut(&w.in_progress, MAX_IN_PROGRESS),
            })
            .collect(),
    }
}

/// The worktrees of `repo` from `state.db`.
pub async fn local_rows(db: &StateDb, repo: RepoKey) -> Result<Vec<LocalWorktree>, String> {
    let list = workspaces::list(db, Some(repo))
        .await
        .map_err(|e| note(&e.to_string()))?;
    let list = newest(list, |r| r.epoch_time);
    let mut out = Vec::with_capacity(list.len());
    for r in list {
        let dirty: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM journal WHERE ws_id = ?")
            .bind(r.ws.id.to_string())
            .fetch_one(db.pool())
            .await
            .map_err(|e| note(&e.to_string()))?;
        let head = if r.ws.head.is_empty() {
            r.ws.base_cs
        } else {
            match RefName::new(r.ws.head.as_bytes().to_vec()) {
                Ok(name) => db
                    .refs()
                    .get(repo, &name)
                    .await
                    .map_err(|e| note(&e.to_string()))?
                    .and_then(|e| match e.target {
                        RefTarget::Changeset(c) => Some(c),
                        RefTarget::Tag(_) => None,
                    }),
                Err(_) => None,
            }
        };
        out.push(LocalWorktree {
            path: r.ws.path.clone(),
            mode: match r.ws.mode {
                Mode::Virtual => "virtual",
                Mode::Plain => "plain",
            }
            .to_owned(),
            head_ref: r.ws.head.clone(),
            head,
            dirty: dirty > 0,
            ahead: 0,
            behind: 0,
            ephemeral_until: r.ws.ephemeral_until.unwrap_or(0),
            in_progress: String::new(),
            last_use: r.epoch_time,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use asc_engine::branchless::sync::{CloudDoc, CloudStore, CloudUpdate, StoreError};
    use asc_state::AscHome;
    use asc_state::home::RepoId;
    use asc_state::state::repos::{NewRepo, RemoteKind};

    use super::super::{Clock, CloudSync, StateRows};
    use super::*;

    fn row(path: &str, head_ref: &str, last_use: i64) -> LocalWorktree {
        LocalWorktree {
            path: path.into(),
            mode: "virtual".into(),
            head_ref: head_ref.into(),
            head: Some(ChangesetId::of(b"x")),
            dirty: true,
            ahead: 1,
            behind: 2,
            ephemeral_until: 0,
            in_progress: String::new(),
            last_use,
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn hash_is_stable_per_path_and_the_request_holds_no_path() {
        let salt = [7u8; 32];
        let rows = [
            row("/home/me/secret-project", "refs/heads/main", 1),
            row("/tmp/other", "", 2),
        ];
        let a = build(&salt, "desk", &rows);
        assert_eq!(a, build(&salt, "desk", &rows));
        assert_eq!(a.worktrees.len(), 2);
        assert_eq!(a.worktrees[0].ws_hash.len(), 32);
        assert_ne!(a.worktrees[0].ws_hash, a.worktrees[1].ws_hash);
        assert_eq!(
            a.worktrees[1].ws_hash.to_vec(),
            ws_hash(&salt, "/home/me/secret-project").to_vec()
        );
        assert_ne!(ws_hash(&salt, "/a"), ws_hash(&[8u8; 32], "/a"));
        let dump = format!("{a:?}");
        assert!(!dump.contains("secret-project") && !dump.contains("/tmp/other"));
        assert!(!dump.contains(&hex(&salt)));
    }

    #[test]
    fn a_bad_ref_is_sent_empty() {
        let salt = [1u8; 32];
        for bad in ["C:/x", "~/x", "/x", "a\\b", "a..b", "x.lock", "a b", "a@{b"] {
            let r = build(&salt, "d", &[row("/p", bad, 0)]);
            assert_eq!(r.worktrees[0].head_ref, "", "{bad}");
        }
        let r = build(&salt, "d", &[row("/p", "refs/heads/ok", 0)]);
        assert_eq!(r.worktrees[0].head_ref, "refs/heads/ok");
    }

    #[test]
    fn the_cap_is_256_and_the_newest_stay() {
        let rows: Vec<LocalWorktree> = (0..300).map(|i| row(&format!("/w/{i}"), "", i)).collect();
        let r = build(&[0u8; 32], "d", &rows);
        assert_eq!(r.worktrees.len(), 256);
        assert_eq!(
            r.worktrees[0].ws_hash.to_vec(),
            ws_hash(&[0u8; 32], "/w/299").to_vec()
        );
    }

    #[test]
    fn a_truncated_salt_is_replaced_once_and_racers_agree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SALT_FILE), b"short").unwrap();
        let a = salt(dir.path()).unwrap();
        assert_eq!(a, salt(dir.path()).unwrap());
        assert_eq!(std::fs::read(dir.path().join(SALT_FILE)).unwrap().len(), 32);

        let race = tempfile::tempdir().unwrap();
        let root = race.path().to_path_buf();
        let got: Vec<[u8; 32]> = (0..8)
            .map(|_| {
                let r = root.clone();
                std::thread::spawn(move || salt(&r).unwrap())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        assert!(got.iter().all(|s| *s == got[0]));
        let left: Vec<_> = std::fs::read_dir(&root).unwrap().collect();
        assert_eq!(left.len(), 1, "no temp file stays");
    }

    #[test]
    fn the_newest_survive_more_than_512_rows() {
        let rows: Vec<i64> = (0..600).collect();
        let kept = newest(rows, |&t| t);
        assert_eq!(kept.len(), 256);
        assert_eq!(kept[0], 599);
        assert_eq!(*kept.last().unwrap(), 344);
    }

    #[test]
    fn the_salt_is_made_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = salt(dir.path()).unwrap();
        assert_eq!(a, salt(dir.path()).unwrap());
        assert_ne!(a, [0u8; 32]);
    }

    struct Store;

    #[async_trait]
    impl CloudStore for Store {
        async fn get(&self) -> Result<CloudDoc, StoreError> {
            Ok(CloudDoc::default())
        }

        async fn update(&self, _op: &str, _u: &CloudUpdate) -> Result<u64, StoreError> {
            Ok(1)
        }
    }

    struct Fixed;

    impl Clock for Fixed {
        fn now(&self) -> i64 {
            100
        }
    }

    struct Rep {
        sent: Mutex<usize>,
        err: Option<fn() -> SendError>,
    }

    #[async_trait]
    impl WorktreeReporter for Rep {
        async fn report(&self, _req: &wire::ReportWorktreesRequest) -> Result<(), SendError> {
            *self.sent.lock().unwrap() += 1;
            self.err.map_or(Ok(()), |f| Err(f()))
        }
    }

    #[tokio::test]
    async fn a_failed_send_does_not_fail_the_sync() {
        let dir = tempfile::tempdir().unwrap();
        let home = AscHome::open(dir.path().join("home")).unwrap();
        let db = StateDb::open(&home).await.unwrap();
        let repo = asc_state::state::repos::insert(
            &db,
            &NewRepo {
                id: RepoId::new(),
                name: "r".into(),
                ns: vec![1],
                remote_url: None,
                remote_kind: RemoteKind::Server,
                default_branch: "master".into(),
                windows_safe: true,
            },
        )
        .await
        .unwrap();
        let errs: [Option<fn() -> SendError>; 3] = [
            None,
            Some(|| SendError::Other("Unavailable: down".into())),
            Some(|| SendError::Precondition("FailedPrecondition: no cloud".into())),
        ];
        for err in errs {
            let rep = Arc::new(Rep {
                sent: Mutex::new(0),
                err,
            });
            let sync = CloudSync::new(
                StateRows::new(db.clone(), repo),
                Store,
                "default".into(),
                Arc::new(Fixed),
            )
            .with_worktrees("desk".into(), [3u8; 32], rep.clone());
            assert!(sync.run().await.is_ok());
            assert_eq!(*rep.sent.lock().unwrap(), 1, "sent once, not retried");
        }
    }
}
