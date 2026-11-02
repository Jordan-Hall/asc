//! Per-directory access rules of a repository and the team members they name
//! (`pathrules.proto`, docs/SPEC.md section 15.5.1).
//!
//! The rules are kept here, never in the repository. [`PathRulesHub::acl`] gives the compiled
//! rules of a repository to every read and write path; it caches them for at most
//! [`CACHE_TTL`], and a change made through this server drops the entry at once.

pub mod guard;
pub mod objpaths;
pub mod rpc;
pub mod scope;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use asc_core::pathacl::{Level, PathAcl, Rule, Subject, Viewer};
use asc_core::policy::Who;
use sqlx::{AnyPool, Row};
use tonic::Status;

use crate::ai::audit::Audit;
use crate::ai::clock;
use crate::ai::ops::Ops;

/// What `Version.features` adds for this service.
pub const FEATURE: &str = "pathrules.v1";
/// `GetMyPathAccess`: any reader learns its own levels, so a client keeps them on disk.
pub const ACCESS_FEATURE: &str = "pathaccess.v1";
/// Most members of one team.
pub const MAX_MEMBERS: usize = 5000;
/// Most teams listed for one organisation.
pub const MAX_TEAMS: usize = 1000;
/// Most paths one `CheckPathAccess` decides.
pub const MAX_CHECK_PATHS: usize = 200;
/// Longest member or team name, in bytes.
pub const MAX_NAME: usize = 128;
/// How long a compiled rule set is reused before it is read again (another replica may have
/// changed it).
pub const CACHE_TTL: Duration = Duration::from_secs(5);

/// One repository's stored rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
    pub revision: u64,
    pub updated: i64,
    pub updated_by: String,
}

/// Why a path rules call failed.
#[derive(Debug)]
pub enum RulesError {
    Invalid(String),
    Stale { current: u64 },
    Sql(String),
}

impl From<sqlx::Error> for RulesError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(crate::ai::store::class(&e))
    }
}

impl RulesError {
    pub fn status(&self) -> Status {
        match self {
            Self::Invalid(m) => Status::invalid_argument(m.clone()),
            Self::Stale { current } => Status::failed_precondition(format!(
                "the path rules changed (revision {current}); reload them and try again"
            )),
            Self::Sql(c) => {
                tracing::error!(class = c.as_str(), "path rules database error");
                Status::internal("internal error")
            }
        }
    }
}

/// Parses one wire rule.
pub fn parse_rule(dir: &str, subject: &str, level: &str) -> Result<Rule, RulesError> {
    let subject = Subject::parse(subject).map_err(|e| RulesError::Invalid(e.0))?;
    let level = Level::parse(level).ok_or_else(|| {
        RulesError::Invalid(format!(
            "level {level:?} is not none, build, read, write or admin"
        ))
    })?;
    let rule = Rule {
        dir: dir.to_owned(),
        subject,
        level,
    };
    // Compiling the one rule checks its directory.
    PathAcl::new(std::slice::from_ref(&rule)).map_err(|e| RulesError::Invalid(e.0))?;
    Ok(rule)
}

/// A team or member name: printable, no spaces, at most [`MAX_NAME`] bytes.
pub fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._@+-:/".contains(&b))
}

/// The member string a principal is listed under in a team: the login of a person, else
/// `<kind>:<name>`.
pub fn member_key(who: &Who) -> Option<String> {
    let kind = who.kind?;
    if who.name.is_empty() {
        return None;
    }
    Some(match kind {
        asc_core::policy::Kind::User => who.name.to_ascii_lowercase(),
        k => format!("{}:{}", k.as_str(), who.name),
    })
}

struct Cached {
    at: Instant,
    acl: Arc<PathAcl>,
}

/// The hub of the path rules service.
pub struct PathRulesHub {
    pool: AnyPool,
    pub(crate) ops: Ops,
    pub(crate) audit: Arc<dyn Audit>,
    cache: Mutex<HashMap<String, Cached>>,
}

/// The place the single [`PathRulesHub`] is registered.
#[derive(Default)]
pub struct PathRulesSlot(OnceLock<Arc<PathRulesHub>>);

impl PathRulesSlot {
    pub fn set(&self, h: Arc<PathRulesHub>) -> bool {
        self.0.set(h).is_ok()
    }
    pub fn get(&self) -> Option<&Arc<PathRulesHub>> {
        self.0.get()
    }
}

fn key(repo: &str) -> String {
    repo.to_ascii_lowercase()
}

impl PathRulesHub {
    pub fn new(pool: AnyPool, audit: Arc<dyn Audit>) -> Arc<Self> {
        Arc::new(Self {
            ops: Ops::new(pool.clone()),
            pool,
            audit,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// The stored rules of `repo`, in the order they were set.
    pub async fn get(&self, repo: &str) -> Result<RuleSet, RulesError> {
        let repo = key(repo);
        let mut set = RuleSet::default();
        if let Some(r) =
            sqlx::query("SELECT revision, updated, updated_by FROM path_rule_sets WHERE repo = $1")
                .bind(repo.clone())
                .fetch_optional(&self.pool)
                .await?
        {
            set.revision = u64::try_from(r.try_get::<i64, _>("revision")?).unwrap_or(0);
            set.updated = r.try_get("updated")?;
            set.updated_by = r.try_get("updated_by")?;
        }
        for r in sqlx::query(
            "SELECT dir, subject, level FROM path_rules WHERE repo = $1 ORDER BY ord LIMIT 600",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?
        {
            let dir: String = r.try_get("dir")?;
            let subject: String = r.try_get("subject")?;
            let level: String = r.try_get("level")?;
            // A stored row was validated when it was written; one that no longer parses is
            // dropped rather than trusted, and the set compiles from the rest.
            match parse_rule(&dir, &subject, &level) {
                Ok(rule) => set.rules.push(rule),
                Err(_) => {
                    tracing::warn!(dir, subject, "an unreadable stored path rule was skipped")
                }
            }
        }
        Ok(set)
    }

    /// Replaces the rules of `repo` when its revision is still `expected`. Returns the new
    /// revision.
    pub async fn set(
        &self,
        repo: &str,
        rules: &[Rule],
        expected: u64,
        by: &str,
    ) -> Result<u64, RulesError> {
        PathAcl::new(rules).map_err(|e| RulesError::Invalid(e.0))?;
        let repo = key(repo);
        let now = clock::now();
        let mut tx = self.pool.begin().await?;
        let current: u64 = sqlx::query("SELECT revision FROM path_rule_sets WHERE repo = $1")
            .bind(repo.clone())
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| r.try_get::<i64, _>("revision"))
            .transpose()?
            .map_or(0, |v| u64::try_from(v).unwrap_or(0));
        if current != expected {
            return Err(RulesError::Stale { current });
        }
        let next = current + 1;
        let next_i = i64::try_from(next).map_err(|_| RulesError::Stale { current })?;
        let expected_i = i64::try_from(expected).map_err(|_| RulesError::Stale { current })?;
        let moved = if current == 0 {
            sqlx::query(
                "INSERT INTO path_rule_sets (repo, revision, updated, updated_by) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (repo) DO NOTHING",
            )
            .bind(repo.clone())
            .bind(next_i)
            .bind(now)
            .bind(by.to_owned())
            .execute(&mut *tx)
            .await?
            .rows_affected()
        } else {
            sqlx::query(
                "UPDATE path_rule_sets SET revision = $1, updated = $2, updated_by = $3 \
                 WHERE repo = $4 AND revision = $5",
            )
            .bind(next_i)
            .bind(now)
            .bind(by.to_owned())
            .bind(repo.clone())
            .bind(expected_i)
            .execute(&mut *tx)
            .await?
            .rows_affected()
        };
        if moved != 1 {
            return Err(RulesError::Stale { current });
        }
        sqlx::query("DELETE FROM path_rules WHERE repo = $1")
            .bind(repo.clone())
            .execute(&mut *tx)
            .await?;
        for (i, r) in rules.iter().enumerate() {
            sqlx::query(
                "INSERT INTO path_rules (repo, ord, dir, subject, level) VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(repo.clone())
            .bind(i64::try_from(i).unwrap_or(i64::MAX))
            .bind(r.dir.clone())
            .bind(r.subject.render())
            .bind(r.level.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.forget(&repo);
        Ok(next)
    }

    /// The compiled rules of `repo` (empty when it has none).
    pub async fn acl(&self, repo: &str) -> Result<Arc<PathAcl>, RulesError> {
        let k = key(repo);
        {
            let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(c) = cache.get(&k)
                && c.at.elapsed() < CACHE_TTL
            {
                return Ok(c.acl.clone());
            }
        }
        let set = self.get(&k).await?;
        let acl = Arc::new(PathAcl::new(&set.rules).map_err(|e| RulesError::Invalid(e.0))?);
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.len() > 10_000 {
            cache.retain(|_, c| c.at.elapsed() < CACHE_TTL);
        }
        cache.insert(
            k,
            Cached {
                at: Instant::now(),
                acl: acl.clone(),
            },
        );
        Ok(acl)
    }

    fn forget(&self, repo: &str) {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key(repo));
    }

    /// The teams of `org` that list `who`.
    pub async fn teams_of(&self, org: &str, who: &Who) -> Result<Vec<String>, RulesError> {
        let Some(member) = member_key(who) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for r in sqlx::query(
            "SELECT team FROM path_team_members WHERE org = $1 AND member = $2 ORDER BY team LIMIT 1000",
        )
        .bind(org.to_ascii_lowercase())
        .bind(member)
        .fetch_all(&self.pool)
        .await?
        {
            out.push(r.try_get("team")?);
        }
        Ok(out)
    }

    /// The viewer `who` with `repo_level` on `repo` (`owner/name`), with its teams.
    pub async fn viewer(
        &self,
        repo: &str,
        who: Who,
        repo_level: Level,
    ) -> Result<Viewer, RulesError> {
        let org = repo.split_once('/').map_or(repo, |(o, _)| o);
        let teams = self.teams_of(org, &who).await?;
        Ok(Viewer {
            who,
            teams,
            repo_level,
        })
    }

    /// The teams of `org` with their members, by name.
    pub async fn teams(&self, org: &str) -> Result<Vec<(String, Vec<String>)>, RulesError> {
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for r in sqlx::query(
            "SELECT team, member FROM path_team_members WHERE org = $1 ORDER BY team, member LIMIT 200000",
        )
        .bind(org.to_ascii_lowercase())
        .fetch_all(&self.pool)
        .await?
        {
            let team: String = r.try_get("team")?;
            let member: String = r.try_get("member")?;
            match out.last_mut() {
                Some((t, m)) if *t == team => m.push(member),
                _ => {
                    if out.len() == MAX_TEAMS {
                        break;
                    }
                    out.push((team, vec![member]));
                }
            }
        }
        Ok(out)
    }

    /// Replaces the members of `team` in `org`; none removes it.
    pub async fn set_team(
        &self,
        org: &str,
        team: &str,
        members: &[String],
    ) -> Result<usize, RulesError> {
        if !name_ok(team) || team.contains(':') {
            return Err(RulesError::Invalid(
                "a team name is 1 to 128 characters of [A-Za-z0-9._@+-/]".into(),
            ));
        }
        if members.len() > MAX_MEMBERS {
            return Err(RulesError::Invalid(format!(
                "a team has at most {MAX_MEMBERS} members"
            )));
        }
        let mut list: Vec<String> = Vec::with_capacity(members.len());
        for m in members {
            if !name_ok(m) {
                return Err(RulesError::Invalid(format!(
                    "member {m:?} is not a valid name"
                )));
            }
            // A person is listed by login, case-insensitively.
            list.push(if m.contains(':') {
                m.clone()
            } else {
                m.to_ascii_lowercase()
            });
        }
        list.sort();
        list.dedup();
        let org = org.to_ascii_lowercase();
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM path_team_members WHERE org = $1 AND team = $2")
            .bind(org.clone())
            .bind(team.to_owned())
            .execute(&mut *tx)
            .await?;
        for m in &list {
            sqlx::query("INSERT INTO path_team_members (org, team, member) VALUES ($1, $2, $3)")
                .bind(org.clone())
                .bind(team.to_owned())
                .bind(m.clone())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        // Teams are resolved per call (see `viewer`), so no cached rule set goes stale here.
        Ok(list.len())
    }
}
