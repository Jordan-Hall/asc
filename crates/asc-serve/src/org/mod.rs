//! Organisation settings the server owns (`orgsettings.proto`, docs/BUILDOUT.md section 28):
//! the agents card, the default merge policy, the organisation's agent identities, and the
//! instance's agent switch and default budget.
//!
//! The resolution order is written in `orgsettings.proto` and implemented in [`policy`] (merge
//! requirements) and [`OrgHub::agent_gate`] (agents). The agent runtime asks `agent_gate` before
//! it starts a run; the land gate asks [`OrgHub::merge_defaults`] and applies [`policy::apply`].

pub mod policy;
pub mod rpc;

#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};

use sqlx::{AnyPool, Row};
use tonic::Status;

use crate::ai::audit::Audit;
use crate::ai::clock;
use crate::ai::ops::Ops;
use policy::MergeDefaults;

/// What `Version.features` adds for this service.
pub const FEATURE: &str = "orgsettings.v1";
/// Most agent identities of one organisation.
pub const MAX_AGENTS: i64 = 100;
/// Most paths and most repositories of one agent.
pub const MAX_LIST: usize = 64;

/// The agent policy of an organisation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentPolicy {
    pub allow_agents: bool,
    pub need_person: bool,
    pub ceiling: u64,
}

impl Default for AgentPolicy {
    fn default() -> Self {
        Self {
            allow_agents: true,
            need_person: true,
            ceiling: 0,
        }
    }
}

/// The instance's agent settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstanceAgents {
    pub allow_agents: bool,
    pub default_budget: u64,
}

impl Default for InstanceAgents {
    fn default() -> Self {
        Self {
            allow_agents: true,
            default_budget: 0,
        }
    }
}

/// One organisation's row.
#[derive(Clone, Debug, Default)]
pub struct OrgRow {
    pub agents: AgentPolicy,
    pub merge: Option<MergeDefaults>,
    pub updated: i64,
    pub updated_by: String,
}

/// An agent identity of an organisation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRow {
    pub name: String,
    pub display: String,
    pub run_by: String,
    pub paths: Vec<String>,
    pub repos: Vec<String>,
    pub paused: bool,
    pub created: i64,
    pub created_by: String,
    pub updated: i64,
}

/// What the agent runtime may do in an organisation, all sources combined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentGate {
    pub allowed: bool,
    /// A person must approve what an agent proposes.
    pub need_person: bool,
    /// The smallest non-zero monthly spend limit that applies; 0 = none.
    pub ceiling: u64,
}

/// Why an organisation call failed.
#[derive(Debug)]
pub enum OrgError {
    NotFound,
    Exists,
    Invalid(&'static str),
    TooMany,
    Sql(String),
}

impl From<sqlx::Error> for OrgError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(crate::ai::store::class(&e))
    }
}

impl OrgError {
    pub fn status(&self) -> Status {
        match self {
            Self::NotFound => Status::not_found("no such agent"),
            Self::Exists => {
                Status::already_exists("the organisation already has an agent with that name")
            }
            Self::Invalid(m) => Status::invalid_argument(*m),
            Self::TooMany => {
                Status::resource_exhausted("an organisation may have at most 100 agents")
            }
            Self::Sql(class) => {
                tracing::error!(class, "organisation settings database error");
                Status::internal("internal error")
            }
        }
    }
}

/// The hub of the organisation settings service.
pub struct OrgHub {
    pool: AnyPool,
    pub(crate) ops: Ops,
    pub(crate) audit: Arc<dyn Audit>,
}

/// The place the single [`OrgHub`] is registered.
#[derive(Default)]
pub struct OrgSlot(OnceLock<Arc<OrgHub>>);

impl OrgSlot {
    pub fn set(&self, h: Arc<OrgHub>) -> bool {
        self.0.set(h).is_ok()
    }
    pub fn get(&self) -> Option<&Arc<OrgHub>> {
        self.0.get()
    }
}

fn b(v: bool) -> i64 {
    i64::from(v)
}

fn u(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn list_of(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}

/// A valid agent name: `[a-z0-9-]`, 1 to 40 characters, starting with a letter or digit.
pub fn agent_name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 40
        && !n.starts_with('-')
        && n.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

impl OrgHub {
    pub fn new(pool: AnyPool, audit: Arc<dyn Audit>) -> Arc<Self> {
        Arc::new(Self {
            ops: Ops::new(pool.clone()),
            pool,
            audit,
        })
    }

    // -------------------------------------------------------------------------- settings

    pub async fn row(&self, org: &str) -> Result<OrgRow, OrgError> {
        let r = sqlx::query(
            "SELECT allow_agents, agent_changesets_need_person, agent_spend_ceiling, merge_json, updated, updated_by \
             FROM org_settings WHERE org = $1",
        )
        .bind(org.to_owned())
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = r else {
            return Ok(OrgRow::default());
        };
        let json: String = r.try_get("merge_json")?;
        Ok(OrgRow {
            agents: AgentPolicy {
                allow_agents: r.try_get::<i64, _>("allow_agents")? != 0,
                need_person: r.try_get::<i64, _>("agent_changesets_need_person")? != 0,
                ceiling: u(r.try_get("agent_spend_ceiling")?),
            },
            merge: if json.is_empty() {
                None
            } else {
                serde_json::from_str(&json).ok()
            },
            updated: r.try_get("updated")?,
            updated_by: r.try_get("updated_by")?,
        })
    }

    async fn ensure(&self, org: &str) -> Result<(), OrgError> {
        sqlx::query("INSERT INTO org_settings (org) VALUES ($1) ON CONFLICT (org) DO NOTHING")
            .bind(org.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_agents(
        &self,
        org: &str,
        p: AgentPolicy,
        by: &str,
    ) -> Result<OrgRow, OrgError> {
        let max = u64::try_from(i64::MAX).unwrap_or(u64::MAX);
        if p.ceiling > max {
            return Err(OrgError::Invalid("the ceiling is at most 2^63 - 1"));
        }
        self.ensure(org).await?;
        sqlx::query(
            "UPDATE org_settings SET allow_agents = $2, agent_changesets_need_person = $3, agent_spend_ceiling = $4, \
             updated = $5, updated_by = $6 WHERE org = $1",
        )
        .bind(org.to_owned())
        .bind(b(p.allow_agents))
        .bind(b(p.need_person))
        .bind(i64::try_from(p.ceiling).unwrap_or(i64::MAX))
        .bind(clock::now())
        .bind(by.to_owned())
        .execute(&self.pool)
        .await?;
        self.row(org).await
    }

    pub async fn set_merge(
        &self,
        org: &str,
        d: MergeDefaults,
        by: &str,
    ) -> Result<OrgRow, OrgError> {
        d.validate().map_err(OrgError::Invalid)?;
        let json = serde_json::to_string(&d.normalised())
            .map_err(|_| OrgError::Invalid("unencodable defaults"))?;
        self.ensure(org).await?;
        sqlx::query(
            "UPDATE org_settings SET merge_json = $2, updated = $3, updated_by = $4 WHERE org = $1",
        )
        .bind(org.to_owned())
        .bind(json)
        .bind(clock::now())
        .bind(by.to_owned())
        .execute(&self.pool)
        .await?;
        self.row(org).await
    }

    /// The organisation's merge defaults, when it has set any. The land gate applies them with
    /// [`policy::apply`].
    pub async fn merge_defaults(&self, org: &str) -> Option<MergeDefaults> {
        self.row(&org.to_ascii_lowercase())
            .await
            .ok()
            .and_then(|r| r.merge)
    }

    // -------------------------------------------------------------------------- instance

    pub async fn instance(&self) -> Result<InstanceAgents, OrgError> {
        let r = sqlx::query(
            "SELECT allow_agents, default_agent_budget FROM instance_settings WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = r else {
            return Ok(InstanceAgents::default());
        };
        Ok(InstanceAgents {
            allow_agents: r.try_get::<i64, _>("allow_agents")? != 0,
            default_budget: u(r.try_get("default_agent_budget")?),
        })
    }

    pub async fn set_instance(
        &self,
        s: InstanceAgents,
        by: &str,
    ) -> Result<InstanceAgents, OrgError> {
        if s.default_budget > u64::try_from(i64::MAX).unwrap_or(u64::MAX) {
            return Err(OrgError::Invalid("the budget is at most 2^63 - 1"));
        }
        sqlx::query("DELETE FROM instance_settings WHERE id = 1")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "INSERT INTO instance_settings (id, allow_agents, default_agent_budget, updated, updated_by) VALUES (1, $1, $2, $3, $4)",
        )
        .bind(b(s.allow_agents))
        .bind(i64::try_from(s.default_budget).unwrap_or(i64::MAX))
        .bind(clock::now())
        .bind(by.to_owned())
        .execute(&self.pool)
        .await?;
        self.instance().await
    }

    /// What the agent runtime may do in `org`: instance AND organisation allow it, a person
    /// approves if the organisation says so, and the smallest non-zero limit applies.
    pub async fn agent_gate(&self, org: &str) -> Result<AgentGate, OrgError> {
        let inst = self.instance().await?;
        let row = self.row(&org.to_ascii_lowercase()).await?;
        let ceiling = [inst.default_budget, row.agents.ceiling]
            .into_iter()
            .filter(|c| *c > 0)
            .min()
            .unwrap_or(0);
        Ok(AgentGate {
            allowed: inst.allow_agents && row.agents.allow_agents,
            need_person: row.agents.need_person,
            ceiling,
        })
    }

    /// The land requirements of `repo` (`owner/name`) under its organisation's defaults. A failure
    /// to read them leaves the repository's own requirements (the stricter side is never lost
    /// silently: an unreadable default is logged).
    pub async fn apply_requirements(
        &self,
        repo: &str,
        own: asc_core::policy::LandRequirements,
    ) -> asc_core::policy::LandRequirements {
        let owner = repo.split('/').next().unwrap_or("").to_ascii_lowercase();
        match self.row(&owner).await {
            Ok(r) => policy::apply(r.merge.as_ref(), own),
            Err(e) => {
                tracing::error!(error = ?e, "organisation merge defaults unreadable");
                own
            }
        }
    }

    /// Whether the organisation of `repo` requires signed changes on trunk.
    pub async fn requires_signed(&self, repo: &str) -> bool {
        let owner = repo.split('/').next().unwrap_or("").to_ascii_lowercase();
        self.row(&owner)
            .await
            .ok()
            .and_then(|r| r.merge)
            .is_some_and(|m| m.require_signed)
    }

    /// What the organisation's agents have spent this month (UTC), in micro-USD.
    pub async fn month_agent_spend(&self, org: &str) -> Result<u64, OrgError> {
        let prefix = format!("{}/", org.to_ascii_lowercase());
        let first = clock::day(clock::month_start(clock::now()));
        let n: i64 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(SUM(cost), 0) AS BIGINT) FROM agent_spend WHERE SUBSTR(repo, 1, $2) = $1 AND day >= $3",
        )
        .bind(prefix.clone())
        .bind(i64::try_from(prefix.len()).unwrap_or(0))
        .bind(first)
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    // -------------------------------------------------------------------------- agents

    pub async fn agent_count(&self, org: &str) -> Result<i64, OrgError> {
        Ok(
            sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM org_agents WHERE org = $1")
                .bind(org.to_owned())
                .fetch_one(&self.pool)
                .await?,
        )
    }

    fn agent_of(r: &sqlx::any::AnyRow) -> Result<AgentRow, sqlx::Error> {
        Ok(AgentRow {
            name: r.try_get("name")?,
            display: r.try_get("display")?,
            run_by: r.try_get("run_by")?,
            paths: list_of(&r.try_get::<String, _>("paths_json")?),
            repos: list_of(&r.try_get::<String, _>("repos_json")?),
            paused: r.try_get::<i64, _>("state")? != 0,
            created: r.try_get("created")?,
            created_by: r.try_get("created_by")?,
            updated: r.try_get("updated")?,
        })
    }

    const AGENT_COLS: &'static str =
        "name, display, run_by, paths_json, repos_json, state, created, created_by, updated";

    pub async fn agents(&self, org: &str) -> Result<Vec<AgentRow>, OrgError> {
        let q = format!(
            "SELECT {} FROM org_agents WHERE org = $1 ORDER BY name",
            Self::AGENT_COLS
        );
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(org.to_owned())
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(Self::agent_of)
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    pub async fn agent(&self, org: &str, name: &str) -> Result<AgentRow, OrgError> {
        let q = format!(
            "SELECT {} FROM org_agents WHERE org = $1 AND name = $2",
            Self::AGENT_COLS
        );
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(org.to_owned())
            .bind(name.to_owned())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(Self::agent_of)
            .transpose()?
            .ok_or(OrgError::NotFound)
    }

    /// Checks the fields of an agent against `org`.
    pub fn check_agent(org: &str, a: &AgentRow) -> Result<(), OrgError> {
        if !agent_name_ok(&a.name) {
            return Err(OrgError::Invalid(
                "an agent name is 1 to 40 characters of [a-z0-9-]",
            ));
        }
        if a.display.len() > 80 || a.display.chars().any(char::is_control) {
            return Err(OrgError::Invalid(
                "display_name is at most 80 bytes without control characters",
            ));
        }
        if !a.run_by.is_empty() && crate::settings::access::Subject::user(&a.run_by).is_err() {
            return Err(OrgError::Invalid("run_by is a login"));
        }
        if a.paths.len() > MAX_LIST || a.repos.len() > MAX_LIST {
            return Err(OrgError::Invalid(
                "an agent has at most 64 paths and 64 repositories",
            ));
        }
        for p in &a.paths {
            asc_core::policy::PathGlob::parse(p)
                .map_err(|_| OrgError::Invalid("a path pattern is not valid"))?;
        }
        let prefix = format!("{org}/");
        for r in &a.repos {
            let ok = r.strip_prefix(&prefix).is_some_and(|n| {
                !n.is_empty()
                    && n.len() <= 100
                    && !n.contains('/')
                    && n.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
            });
            if !ok {
                return Err(OrgError::Invalid(
                    "a repository of an agent is <organisation>/<name>",
                ));
            }
        }
        Ok(())
    }

    pub async fn create_agent(
        &self,
        org: &str,
        mut a: AgentRow,
        by: &str,
    ) -> Result<AgentRow, OrgError> {
        a.repos.iter_mut().for_each(|r| *r = r.to_ascii_lowercase());
        a.run_by = a.run_by.to_ascii_lowercase();
        Self::check_agent(org, &a)?;
        if self.agent_count(org).await? >= MAX_AGENTS {
            return Err(OrgError::TooMany);
        }
        let now = clock::now();
        let r = sqlx::query(
            "INSERT INTO org_agents (org, name, display, run_by, paths_json, repos_json, state, created, created_by, updated) \
             VALUES ($1, $2, $3, $4, $5, $6, 0, $7, $8, $7) ON CONFLICT (org, name) DO NOTHING",
        )
        .bind(org.to_owned())
        .bind(a.name.clone())
        .bind(a.display.clone())
        .bind(a.run_by.clone())
        .bind(serde_json::to_string(&a.paths).unwrap_or_else(|_| "[]".into()))
        .bind(serde_json::to_string(&a.repos).unwrap_or_else(|_| "[]".into()))
        .bind(now)
        .bind(by.to_owned())
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(OrgError::Exists);
        }
        self.agent(org, &a.name).await
    }

    pub async fn update_agent(&self, org: &str, a: AgentRow) -> Result<AgentRow, OrgError> {
        Self::check_agent(org, &a)?;
        let r = sqlx::query(
            "UPDATE org_agents SET display = $3, run_by = $4, paths_json = $5, repos_json = $6, state = $7, updated = $8 \
             WHERE org = $1 AND name = $2",
        )
        .bind(org.to_owned())
        .bind(a.name.clone())
        .bind(a.display.clone())
        .bind(a.run_by.clone())
        .bind(serde_json::to_string(&a.paths).unwrap_or_else(|_| "[]".into()))
        .bind(serde_json::to_string(&a.repos).unwrap_or_else(|_| "[]".into()))
        .bind(b(a.paused))
        .bind(clock::now())
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(OrgError::NotFound);
        }
        self.agent(org, &a.name).await
    }

    pub async fn delete_agent(&self, org: &str, name: &str) -> Result<(), OrgError> {
        let r = sqlx::query("DELETE FROM org_agents WHERE org = $1 AND name = $2")
            .bind(org.to_owned())
            .bind(name.to_owned())
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(OrgError::NotFound);
        }
        Ok(())
    }
}
