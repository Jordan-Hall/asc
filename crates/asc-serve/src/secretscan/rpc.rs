//! The `SecretScan` service (`secretscan.proto`).
//!
//! `ListRules` needs repo read; the other three need repo admin, because findings tell where the
//! secrets of a repository are. A finding never holds the secret: the response carries the rule,
//! the place, a keyed fingerprint and a preview of at most two characters.

use std::sync::Arc;

use asc_core::id::ChangesetId;
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_core::policy::SecretsPolicy;
use asc_core::secrets::Severity;
use asc_engine::graph::LogQuery;
use asc_engine::tree::Pathspec;
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::convert::WireId;
use asc_rpc::server::{CallCtx, MAX_MESSAGE, MAX_STRING, ServerInfo, wire};
use asc_rpc::services::secret_scan_server::{SecretScan, SecretScanServer};
use futures::TryStreamExt;
use tokio::sync::Semaphore;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::scan::FileRef;
use super::store::{self, Filter, FindingRow, State};
use super::{DEFAULT_PAGE, MAX_NOTE, MAX_PAGE, Scanner, added_files};
use crate::audit::NewEvent;
use crate::audit::actor::actor_of;
use crate::idmode::{Eng, IdCodec};
use crate::policy::PolicyStore;
use crate::registry::{Registry, RepoEntry};
use crate::review::{Env, internal, now, page_limit, parse_after, registry_status, sql};

/// Scans running at once, server wide; further callers wait their turn.
const CONCURRENT_SCANS: usize = 2;
/// Most paths a scan may be narrowed to.
const MAX_PATHS: usize = 64;
/// Most rule ids and states a list filter names.
const MAX_FILTER: usize = 64;

/// The state the `SecretScan` service shares.
pub struct SecretScanHub {
    registry: Arc<Registry>,
    scanner: Arc<Scanner>,
    policies: Arc<PolicyStore>,
    permits: Semaphore,
}

fn severity_to_wire(s: Severity) -> i32 {
    (match s {
        Severity::Low => wire::SecretSeverity::Low,
        Severity::Medium => wire::SecretSeverity::Medium,
        Severity::High => wire::SecretSeverity::High,
        Severity::Critical => wire::SecretSeverity::Critical,
    }) as i32
}

fn state_to_wire(s: State) -> i32 {
    (match s {
        State::Open => wire::SecretFindingState::Open,
        State::Revoked => wire::SecretFindingState::Revoked,
        State::FalsePositive => wire::SecretFindingState::FalsePositive,
        State::AcceptedRisk => wire::SecretFindingState::AcceptedRisk,
    }) as i32
}

fn state_of_wire(n: i32) -> Result<Option<State>, Status> {
    use wire::SecretFindingState as W;
    match W::try_from(n).map_err(|_| Status::invalid_argument("unknown finding state"))? {
        W::Unspecified => Ok(None),
        W::Open => Ok(Some(State::Open)),
        W::Revoked => Ok(Some(State::Revoked)),
        W::FalsePositive => Ok(Some(State::FalsePositive)),
        W::AcceptedRisk => Ok(Some(State::AcceptedRisk)),
    }
}

impl SecretScanHub {
    pub fn new(registry: Arc<Registry>, scanner: Arc<Scanner>, policies: Arc<PolicyStore>) -> Self {
        Self {
            registry,
            scanner,
            policies,
            permits: Semaphore::new(CONCURRENT_SCANS),
        }
    }

    async fn open(&self, ctx: &CallCtx) -> Result<Arc<RepoEntry>, Status> {
        self.registry.open(&ctx.repo).await.map_err(registry_status)
    }

    /// The `[secrets]` policy in force: the trunk tip's, or the safe defaults when there is none
    /// or it cannot be used (scanning stays on and blocking).
    pub async fn policy_of(&self, entry: &RepoEntry) -> Result<SecretsPolicy, Status> {
        let loaded = self.policies.in_force(entry).await?;
        Ok(loaded
            .policy()
            .map(|p| p.secrets.clone())
            .unwrap_or_default())
    }

    async fn finding_out(
        &self,
        ids: &IdCodec,
        r: &FindingRow,
    ) -> Result<wire::SecretFinding, Status> {
        let changeset = match ChangesetId::from_slice(&r.changeset) {
            Ok(c) => ids.changeset_out(c).await?,
            Err(_) => bytes::Bytes::new(),
        };
        Ok(wire::SecretFinding {
            id: u64::try_from(r.id).unwrap_or(0),
            rule_id: r.rule_id.clone(),
            severity: severity_to_wire(r.severity),
            state: state_to_wire(r.state),
            path: r.path.clone().into(),
            line: u32::try_from(r.line).unwrap_or(0),
            column: u32::try_from(r.column).unwrap_or(0),
            changeset,
            change: r.change.clone().into(),
            fingerprint: r.fingerprint.clone(),
            preview: r.preview.clone(),
            first_seen: r.first_seen,
            last_seen: r.last_seen,
            resolved_by: r.resolved_by.clone(),
            resolved_at: r.resolved_at,
            note: r.note.clone(),
        })
    }

    /// `SecretScan.ListRules`.
    pub async fn list_rules(&self, ctx: &CallCtx) -> Result<wire::ListSecretRulesResponse, Status> {
        let entry = self.open(ctx).await?;
        let sp = self.policy_of(&entry).await?;
        let rules = Scanner::rules(&sp)?;
        Ok(wire::ListSecretRulesResponse {
            rules: rules
                .infos()
                .iter()
                .map(|r| wire::SecretRule {
                    id: r.id.clone(),
                    name: r.name.clone(),
                    description: r.description.clone(),
                    severity: severity_to_wire(r.severity),
                    builtin: r.builtin,
                    enabled: sp.enabled,
                    detector: r.detector.to_owned(),
                })
                .collect(),
            block_on_push: sp.block_on_push,
            allow_paths: sp
                .allow_paths
                .iter()
                .map(|g| g.as_str().to_owned())
                .collect(),
        })
    }

    fn spec(paths: &[String]) -> Result<Option<Pathspec>, Status> {
        if paths.len() > MAX_PATHS || paths.iter().any(|p| p.len() > MAX_STRING) {
            return Err(Status::invalid_argument("too many or too long paths"));
        }
        if paths.is_empty() {
            return Ok(None);
        }
        Pathspec::parse(paths)
            .map(Some)
            .map_err(|e| Status::invalid_argument(e.to_string()))
    }

    async fn resolve(
        ids: &IdCodec,
        entry: &RepoEntry,
        spec: &str,
        fallback: &str,
    ) -> Result<ChangesetId, Status> {
        if spec.len() > MAX_STRING {
            return Err(Status::invalid_argument("revision is too long"));
        }
        let spec = if spec.is_empty() { fallback } else { spec };
        ids.resolve(spec, &entry.default_branch, |e| internal("history", &e))
            .await?
            .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))
    }

    /// `SecretScan.Scan`.
    pub async fn scan(
        &self,
        ctx: &CallCtx,
        req: wire::ScanSecretsRequest,
    ) -> Result<wire::ScanSecretsResponse, Status> {
        use wire::scan_secrets_request::At;
        let spec = Self::spec(&req.paths)?;
        let entry = self.open(ctx).await?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let engine: &Eng = entry.backend.engine();
        let policy = self.policy_of(&entry).await?;
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|e| internal("permits", &e))?;
        // Set when the named range had more changesets than one scan reads.
        let mut cut = false;
        let files: Vec<FileRef> = match req.at {
            Some(At::Rev(rev)) => {
                let cs = Self::resolve(&ids, &entry, &rev, "HEAD").await?;
                let root = engine
                    .get_changeset(cs)
                    .await
                    .map_err(|e| internal("changeset", &e))?
                    .root;
                let mut out = Vec::new();
                let mut walk = engine.walk(root, spec.as_ref());
                while let Some((path, e)) = walk
                    .try_next()
                    .await
                    .map_err(|e| internal("tree walk", &e))?
                {
                    if matches!(e.kind, Kind::File | Kind::Exec) {
                        out.push(FileRef {
                            path: path.as_bytes().to_vec(),
                            file: asc_core::id::FileId::from_bytes(e.id),
                            size: e.size,
                            changeset: cs,
                        });
                        // The scan stops at its own limits; do not walk a tree of millions.
                        if out.len() > self.scanner.cfg.max_files.saturating_mul(4) {
                            break;
                        }
                    }
                }
                out
            }
            Some(At::Change(change)) => {
                let change = <[u8; 16]>::try_from(change.as_ref())
                    .map_err(|_| Status::invalid_argument("a change id is 16 bytes"))?;
                let cs: Option<Vec<u8>> = sqlx::query_scalar(
                    "SELECT cs FROM change_versions WHERE ns = $1 AND change = $2 ORDER BY v DESC LIMIT 1",
                )
                .bind(entry.ns.as_bytes().to_vec())
                .bind(change.to_vec())
                .fetch_optional(self.registry.pool())
                .await
                .map_err(sql)?;
                let cs = cs.ok_or_else(|| Status::not_found("unknown change"))?;
                let cs = ChangesetId::from_slice(&cs).map_err(|e| internal("change index", &e))?;
                filtered(added_files(engine, &[cs]).await?.files, spec.as_ref())
            }
            Some(At::Range(range)) => {
                let head = Self::resolve(&ids, &entry, &range.head, "HEAD").await?;
                let base = Self::resolve(&ids, &entry, &range.base, &entry.default_branch).await?;
                let graph = crate::policy::graph_of(&self.registry, &entry);
                let list: Vec<ChangesetId> = graph
                    .log(LogQuery {
                        include: vec![head],
                        exclude: vec![base],
                        limit: Some(super::MAX_CHANGESETS + 1),
                        ..LogQuery::default()
                    })
                    .try_collect()
                    .await
                    .map_err(|e| internal("history", &e))?;
                let added = added_files(engine, &list).await?;
                cut = added.truncated;
                filtered(added.files, spec.as_ref())
            }
            None => {
                return Err(Status::invalid_argument(
                    "name what to scan: rev, change or range",
                ));
            }
        };
        let mut report = self
            .scanner
            .scan_refs(engine, entry.key.0, &policy, files)
            .await?;
        report.truncated |= cut;
        let new: Vec<store::NewFinding> = report
            .found
            .iter()
            .map(|f| store::NewFinding {
                rule_id: f.rule_id.clone(),
                severity: f.severity,
                path: f.path.clone(),
                line: f.line,
                column: f.column,
                changeset: f.changeset,
                fingerprint: f.fingerprint.clone(),
                preview: f.preview.clone(),
            })
            .collect();
        let rows = store::upsert(self.registry.pool(), entry.key.0, &new, now())
            .await
            .map_err(sql)?;
        let mut findings = Vec::with_capacity(rows.len());
        for r in &rows {
            findings.push(self.finding_out(&ids, r).await?);
        }
        Ok(wire::ScanSecretsResponse {
            findings,
            files_scanned: report.files_scanned,
            truncated: report.truncated,
        })
    }

    /// `SecretScan.ListFindings`.
    pub async fn list_findings(
        &self,
        ctx: &CallCtx,
        req: wire::ListSecretFindingsRequest,
    ) -> Result<wire::ListSecretFindingsResponse, Status> {
        if req.states.len() > MAX_FILTER || req.rule_ids.len() > MAX_FILTER {
            return Err(Status::invalid_argument("too many filters"));
        }
        let mut states = Vec::new();
        for s in &req.states {
            if let Some(s) = state_of_wire(*s)? {
                states.push(s);
            }
        }
        if req.rule_ids.iter().any(|r| r.len() > 128) {
            return Err(Status::invalid_argument("a rule id is too long"));
        }
        let min_severity = match req.min_severity {
            0 => None,
            n => Some(
                Severity::from_number(n)
                    .ok_or_else(|| Status::invalid_argument("unknown severity"))?,
            ),
        };
        let limit = page_limit(req.limit, DEFAULT_PAGE, MAX_PAGE);
        let after = parse_after(&req.page_token)?;
        let entry = self.open(ctx).await?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let filter = Filter {
            states,
            rule_ids: req.rule_ids,
            min_severity,
        };
        let pool = self.registry.pool();
        let mut rows = store::list(pool, entry.key.0, &filter, after, limit)
            .await
            .map_err(sql)?;
        let next_page_token = if rows.len() > limit {
            rows.truncate(limit);
            rows.last().map(|r| r.id.to_string()).unwrap_or_default()
        } else {
            String::new()
        };
        let mut findings = Vec::with_capacity(rows.len());
        for r in &rows {
            findings.push(self.finding_out(&ids, r).await?);
        }
        let counts = store::counts(pool, entry.key.0).await.map_err(sql)?;
        Ok(wire::ListSecretFindingsResponse {
            findings,
            next_page_token,
            counts_by_state: counts.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        })
    }

    /// `SecretScan.UpdateFinding`.
    pub async fn update_finding(
        &self,
        ctx: &CallCtx,
        req: wire::UpdateSecretFindingRequest,
    ) -> Result<wire::UpdateSecretFindingResponse, Status> {
        let state = state_of_wire(req.state)?
            .ok_or_else(|| Status::invalid_argument("name the new state"))?;
        if req.note.len() > MAX_NOTE || req.note.chars().any(|c| c.is_control() && c != '\n') {
            return Err(Status::invalid_argument(format!(
                "a note is at most {MAX_NOTE} bytes of text"
            )));
        }
        let id = i64::try_from(req.id)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Status::invalid_argument("invalid finding id"))?;
        let entry = self.open(ctx).await?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let pool = self.registry.pool();
        let Some(current) = store::get(pool, entry.key.0, id).await.map_err(sql)? else {
            return Err(Status::not_found("no such finding"));
        };
        let event = NewEvent::new("secret.finding.updated", actor_of(&ctx.principal.id))
            .repo(ctx.repo.as_str())
            .target(&id.to_string())
            .detail("finding", id.to_string())
            .detail("state", state.name())
            .detail("rule", &current.rule_id);
        let row = store::set_state(
            pool,
            entry.key.0,
            id,
            state,
            &req.note,
            &ctx.principal.id,
            now(),
            Some(&event),
        )
        .await
        .map_err(sql)?
        .ok_or_else(|| Status::not_found("no such finding"))?;
        Ok(wire::UpdateSecretFindingResponse {
            finding: Some(self.finding_out(&ids, &row).await?),
        })
    }
}

fn filtered(files: Vec<FileRef>, spec: Option<&Pathspec>) -> Vec<FileRef> {
    match spec {
        None => files,
        Some(s) => files
            .into_iter()
            .filter(|f| RepoPath::new(f.path.as_slice()).is_ok_and(|p| s.matches(&p)))
            .collect(),
    }
}

// ------------------------------------------------------------------------------ service

/// The `asc.v1.SecretScan` tonic service.
pub struct SecretScanService {
    hub: Arc<SecretScanHub>,
    env: Env,
}

impl SecretScanService {
    pub fn new(hub: Arc<SecretScanHub>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<SecretScanServer<Self>, AuthInterceptor> {
        let server = SecretScanServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }
}

#[tonic::async_trait]
impl SecretScan for SecretScanService {
    async fn list_rules(
        &self,
        request: Request<wire::ListSecretRulesRequest>,
    ) -> Result<Response<wire::ListSecretRulesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.list_rules(&ctx).await.map(Response::new)
    }

    async fn scan(
        &self,
        request: Request<wire::ScanSecretsRequest>,
    ) -> Result<Response<wire::ScanSecretsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        self.hub.scan(&ctx, msg).await.map(Response::new)
    }

    async fn list_findings(
        &self,
        request: Request<wire::ListSecretFindingsRequest>,
    ) -> Result<Response<wire::ListSecretFindingsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        self.hub.list_findings(&ctx, msg).await.map(Response::new)
    }

    async fn update_finding(
        &self,
        request: Request<wire::UpdateSecretFindingRequest>,
    ) -> Result<Response<wire::UpdateSecretFindingResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, true)
            .await?;
        self.hub.update_finding(&ctx, msg).await.map(Response::new)
    }
}
