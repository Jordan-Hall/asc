//! The `Audit` tonic service (`audit.proto`) and who may read which scope.
//!
//! Reading needs repo admin for a repository, the person for their own user scope, the owner of
//! every repository of an organisation for an organisation, and a site administrator for the
//! instance. A site administrator is the host application's service credential or a user named
//! in `[audit] admins`. A caller without the right gets `NOT_FOUND` for a scope they cannot see,
//! so the answer does not tell whether it exists; no credential at all is `UNAUTHENTICATED`.

use std::collections::HashSet;
use std::sync::Arc;

use asc_rpc::auth::{Access, AuthError, AuthInterceptor, Authorizer, Credential};
use asc_rpc::server::{MAX_MESSAGE, RepoName, RpcStream, ServerInfo, wire};
use asc_rpc::services::audit_server::{Audit as AuditApi, AuditServer};
use bytes::Bytes;
use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use sqlx::Row;
use tonic::service::interceptor::InterceptedService;
use tonic::{Extensions as TonicExt, Request, Response, Status};

use super::actor::actor_of;
use super::chain::Event;
use super::export::{self, Format};
use super::store::{self, Filter};
use super::{Actor, Audit, AuditError, MAX_FILTER_ITEMS, NewEvent, Outcome, Scope, catalogue};

/// Repositories of an organisation the caller may be asked about, at most; an organisation with
/// more is readable by site administrators only.
const MAX_ORG_REPOS: usize = 200;
/// Longest actor filter, in bytes.
const MAX_ACTOR_FILTER: usize = 255;
/// Longest action filter, in bytes.
const MAX_ACTION_FILTER: usize = 128;

fn not_found() -> Status {
    Status::not_found("no such audit scope")
}

fn db(e: AuditError) -> Status {
    crate::commit_graph_rpc::internal("audit", &e)
}

/// The `Audit` service.
pub struct AuditService {
    audit: Arc<Audit>,
    authorizer: Arc<dyn Authorizer>,
    admins: HashSet<String>,
}

impl AuditService {
    pub fn new(audit: Arc<Audit>, authorizer: Arc<dyn Authorizer>, _info: ServerInfo) -> Self {
        let admins = audit.cfg.admins.iter().cloned().collect();
        Self {
            audit,
            authorizer,
            admins,
        }
    }

    /// The tonic service with message limits and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<AuditServer<Self>, AuthInterceptor> {
        let server = AuditServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// Whether `who` is a site administrator.
    fn is_site_admin(&self, who: &Actor) -> bool {
        who.kind == "host" || (who.kind == "user" && self.admins.contains(&who.name))
    }

    /// The scope `owner` names, if the caller may read it, and who the caller is.
    async fn resolve(
        &self,
        ext: &TonicExt,
        owner: Option<&wire::OwnerRef>,
    ) -> Result<(Scope, Actor), Status> {
        let owner = owner.ok_or_else(|| Status::invalid_argument("missing scope"))?;
        let credential = ext.get::<Credential>();
        let kind = wire::OwnerKind::try_from(owner.kind)
            .map_err(|_| Status::invalid_argument("unknown scope kind"))?;
        let principal = self.authorizer.authenticate(credential).await?;
        let who = actor_of(&principal.id);
        match kind {
            wire::OwnerKind::Unspecified => Err(Status::invalid_argument("scope kind is required")),
            wire::OwnerKind::Instance => {
                if !owner.name.is_empty() {
                    return Err(Status::invalid_argument("the instance scope has no name"));
                }
                if self.is_site_admin(&who) {
                    Ok((Scope::instance(), who))
                } else {
                    Err(not_found())
                }
            }
            wire::OwnerKind::Repo => {
                let name = RepoName::parse(&owner.name)?;
                if self.is_site_admin(&who) {
                    return Ok((Scope::repo(name.as_str()), who));
                }
                match self
                    .authorizer
                    .authorize_credential(credential, name.as_str(), Access::Admin)
                    .await
                {
                    Ok(_) => Ok((Scope::repo(name.as_str()), who)),
                    Err(AuthError::Unauthenticated) => Err(AuthError::Unauthenticated.into()),
                    Err(_) => Err(not_found()),
                }
            }
            wire::OwnerKind::User | wire::OwnerKind::Org => {
                if !crate::review::valid_handle(&owner.name) {
                    return Err(Status::invalid_argument("invalid owner name"));
                }
                let scope = Scope::owner(owner.name.clone());
                if self.is_site_admin(&who) {
                    return Ok((scope, who));
                }
                let allowed = if kind == wire::OwnerKind::User {
                    who.kind == "user" && who.name == owner.name && who.name != "anonymous"
                } else {
                    self.owns_every_repo(credential, &owner.name).await?
                };
                if allowed {
                    Ok((scope, who))
                } else {
                    Err(not_found())
                }
            }
        }
    }

    /// Whether the credential has admin on every repository of `org` (and the organisation has
    /// at least one, and not more than [`MAX_ORG_REPOS`]).
    async fn owns_every_repo(
        &self,
        credential: Option<&Credential>,
        org: &str,
    ) -> Result<bool, Status> {
        let prefix = format!("{org}/");
        let rows = sqlx::query(
            "SELECT name FROM repos WHERE substr(name, 1, CAST($1 AS INTEGER)) = $2 \
             ORDER BY name LIMIT $3",
        )
        .bind(i64::try_from(prefix.chars().count()).unwrap_or(0))
        .bind(prefix)
        .bind(i64::try_from(MAX_ORG_REPOS + 1).unwrap_or(i64::MAX))
        .fetch_all(self.audit.registry.pool())
        .await
        .map_err(|e| crate::commit_graph_rpc::internal("audit", &e))?;
        if rows.is_empty() || rows.len() > MAX_ORG_REPOS {
            return Ok(false);
        }
        for r in rows {
            let name: String = r
                .try_get("name")
                .map_err(|e| crate::commit_graph_rpc::internal("audit", &e))?;
            if self
                .authorizer
                .authorize_credential(credential, &name, Access::Admin)
                .await
                .is_err()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn plain_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}

/// The filter of a request, validated.
fn filter_of(f: Option<&wire::AuditFilter>) -> Result<Filter, Status> {
    let Some(f) = f else {
        return Ok(Filter::default());
    };
    let bad = |m: &str| Status::invalid_argument(m.to_owned());
    if f.actors.len() > MAX_FILTER_ITEMS || f.actions.len() > MAX_FILTER_ITEMS {
        return Err(bad("too many filter items"));
    }
    if f.actors.iter().any(|a| !plain_text(a, MAX_ACTOR_FILTER)) {
        return Err(bad("invalid actor filter"));
    }
    for a in &f.actions {
        let name = a.strip_suffix(".*").map_or(a.as_str(), |p| p);
        let ok = plain_text(a, MAX_ACTION_FILTER)
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_'));
        if !ok {
            return Err(bad("invalid action filter"));
        }
    }
    if f.target.len() > super::MAX_TARGET_BYTES || f.target.chars().any(char::is_control) {
        return Err(bad("invalid target filter"));
    }
    if f.since_ms < 0 || f.until_ms < 0 || (f.until_ms > 0 && f.since_ms >= f.until_ms) {
        return Err(bad("invalid time range"));
    }
    let outcome = match wire::AuditOutcome::try_from(f.outcome) {
        Ok(wire::AuditOutcome::Unspecified) => None,
        Ok(wire::AuditOutcome::Success) => Some(Outcome::Success),
        Ok(wire::AuditOutcome::Denied) => Some(Outcome::Denied),
        Ok(wire::AuditOutcome::Failed) => Some(Outcome::Failed),
        Err(_) => return Err(bad("unknown outcome")),
    };
    Ok(Filter {
        actors: f.actors.clone(),
        // `a.*` stays as it is: the store matches the prefix `a.`.
        actions: f.actions.clone(),
        outcome,
        target: f.target.clone(),
        since_ms: f.since_ms,
        until_ms: f.until_ms,
    })
}

fn page_after(token: &str) -> Result<Option<u64>, Status> {
    if token.is_empty() {
        return Ok(None);
    }
    if token.len() > 19 || !token.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Status::invalid_argument("malformed page token"));
    }
    token
        .parse()
        .map(Some)
        .map_err(|_| Status::invalid_argument("malformed page token"))
}

fn principal_ref(a: &Actor) -> wire::PrincipalRef {
    wire::PrincipalRef {
        kind: a.kind.clone(),
        name: a.name.clone(),
    }
}

fn wire_event(e: &Event) -> wire::AuditEvent {
    wire::AuditEvent {
        seq: e.seq,
        id: e.id.clone(),
        time_ms: e.time_ms,
        actor: Some(principal_ref(&e.actor)),
        on_behalf_of: e.on_behalf_of.as_ref().map(principal_ref),
        action: e.action.clone(),
        repo: e.repo.clone(),
        target: e.target.clone(),
        outcome: match e.outcome {
            Outcome::Success => wire::AuditOutcome::Success,
            Outcome::Denied => wire::AuditOutcome::Denied,
            Outcome::Failed => wire::AuditOutcome::Failed,
        } as i32,
        request_id: e.request_id.clone(),
        remote_addr: e.remote_addr.clone(),
        details: e.details.clone().into_iter().collect(),
        prev_hash: Bytes::copy_from_slice(&e.prev_hash),
        hash: Bytes::copy_from_slice(&e.hash),
        user_agent: e.user_agent.clone(),
    }
}

/// A short, secret-free text of a filter for the export event.
fn filter_summary(f: &Filter) -> String {
    let mut parts = Vec::new();
    if !f.actors.is_empty() {
        parts.push(format!("actors={}", f.actors.len()));
    }
    if !f.actions.is_empty() {
        parts.push(format!("actions={}", f.actions.join("|")));
    }
    if let Some(o) = f.outcome {
        parts.push(format!("outcome={}", o.as_str()));
    }
    if !f.target.is_empty() {
        parts.push(format!("target={}", f.target));
    }
    if f.since_ms > 0 {
        parts.push(format!("since={}", f.since_ms));
    }
    if f.until_ms > 0 {
        parts.push(format!("until={}", f.until_ms));
    }
    parts.join(" ")
}

#[tonic::async_trait]
impl AuditApi for AuditService {
    async fn list(
        &self,
        request: Request<wire::ListAuditRequest>,
    ) -> Result<Response<wire::ListAuditResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let (scope, _) = self.resolve(&ext, msg.scope.as_ref()).await?;
        let filter = filter_of(msg.filter.as_ref())?;
        let after = page_after(&msg.page_token)?;
        let page = store::list(
            self.audit.registry.pool(),
            &scope,
            &filter,
            msg.oldest_first,
            usize::try_from(msg.limit).unwrap_or(super::MAX_PAGE),
            after,
        )
        .await
        .map_err(db)?;
        Ok(Response::new(wire::ListAuditResponse {
            events: page.events.iter().map(wire_event).collect(),
            next_page_token: page.next.map(|n| n.to_string()).unwrap_or_default(),
        }))
    }

    type ExportStream = RpcStream<wire::ExportAuditResponse>;

    async fn export(
        &self,
        request: Request<wire::ExportAuditRequest>,
    ) -> Result<Response<Self::ExportStream>, Status> {
        let (_, ext, msg) = request.into_parts();
        let (scope, who) = self.resolve(&ext, msg.scope.as_ref()).await?;
        let filter = filter_of(msg.filter.as_ref())?;
        let format = match wire::AuditExportFormat::try_from(msg.format) {
            Ok(wire::AuditExportFormat::Jsonl) => Format::Jsonl,
            Ok(wire::AuditExportFormat::Csv) => Format::Csv,
            Err(_) => return Err(Status::invalid_argument("unknown export format")),
        };
        // The export is itself an audited action; it is written first, so the export holds it.
        let mut event = NewEvent::new("audit.exported", who)
            .detail("scope", scope.to_string())
            .detail(
                "format",
                match format {
                    Format::Jsonl => "jsonl",
                    Format::Csv => "csv",
                },
            )
            .detail("filter", filter_summary(&filter));
        match scope.kind {
            super::ScopeKind::Repo => event = event.repo(&scope.name),
            super::ScopeKind::Owner => event = event.owner(&scope.name),
            super::ScopeKind::Instance => {}
        }
        store::record(self.audit.registry.pool(), &event)
            .await
            .map_err(db)?;
        let pool = self.audit.registry.pool().clone();
        let max = self.audit.cfg.export_max_events;
        let (mut tx, rx) = mpsc::channel::<Result<wire::ExportAuditResponse, Status>>(2);
        tokio::spawn(async move {
            let chunks = tx.clone();
            let result = export::run(&pool, &scope, &filter, format, max, move |data| {
                let mut chunks = chunks.clone();
                async move {
                    chunks
                        .send(Ok(wire::ExportAuditResponse {
                            data: Bytes::from(data),
                            ..Default::default()
                        }))
                        .await
                        .is_ok()
                }
            })
            .await;
            let last = match result {
                Ok(Some(s)) => Ok(wire::ExportAuditResponse {
                    last: true,
                    digest: Bytes::copy_from_slice(&s.digest),
                    events: s.events,
                    truncated: s.truncated,
                    ..Default::default()
                }),
                // The reader went away.
                Ok(None) => return,
                Err(e) => Err(db(e)),
            };
            let _ = tx.send(last).await;
        });
        Ok(Response::new(rx.boxed()))
    }

    async fn verify_chain(
        &self,
        request: Request<wire::VerifyAuditChainRequest>,
    ) -> Result<Response<wire::VerifyAuditChainResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let (scope, _) = self.resolve(&ext, msg.scope.as_ref()).await?;
        if msg.to_seq != 0 && msg.from_seq > msg.to_seq {
            return Err(Status::invalid_argument("from_seq is above to_seq"));
        }
        let v = store::verify_chain(self.audit.registry.pool(), &scope, msg.from_seq, msg.to_seq)
            .await
            .map_err(db)?;
        Ok(Response::new(wire::VerifyAuditChainResponse {
            ok: v.ok,
            checked: v.checked,
            first_bad_seq: v.first_bad_seq,
            reason: v.reason,
            head_hash: Bytes::copy_from_slice(&v.head_hash),
        }))
    }

    async fn list_actions(
        &self,
        request: Request<wire::ListAuditActionsRequest>,
    ) -> Result<Response<wire::ListAuditActionsResponse>, Status> {
        let (_, ext, _) = request.into_parts();
        self.authorizer
            .authenticate(ext.get::<Credential>())
            .await?;
        Ok(Response::new(wire::ListAuditActionsResponse {
            actions: catalogue::CATALOGUE
                .iter()
                .map(|i| wire::AuditActionInfo {
                    action: i.action.to_owned(),
                    description: i.description.to_owned(),
                    detail_keys: i.detail_keys.iter().map(|k| (*k).to_owned()).collect(),
                })
                .collect(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(actions: &[&str]) -> wire::AuditFilter {
        wire::AuditFilter {
            actions: actions.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn action_filters_keep_the_dot_of_a_prefix() {
        let got = filter_of(Some(&f(&["repo.*", "push.refused"]))).unwrap();
        assert_eq!(got.actions, vec!["repo.*", "push.refused"]);
    }

    #[test]
    fn bad_filters_are_refused() {
        assert!(filter_of(Some(&f(&["Repo.Ref"]))).is_err());
        assert!(filter_of(Some(&f(&["a b"]))).is_err());
        assert!(filter_of(Some(&f(&[""]))).is_err());
        let many = vec!["a".to_owned(); MAX_FILTER_ITEMS + 1];
        assert!(
            filter_of(Some(&wire::AuditFilter {
                actors: many,
                ..Default::default()
            }))
            .is_err()
        );
        assert!(
            filter_of(Some(&wire::AuditFilter {
                since_ms: 10,
                until_ms: 10,
                ..Default::default()
            }))
            .is_err()
        );
        assert!(
            filter_of(Some(&wire::AuditFilter {
                since_ms: -1,
                ..Default::default()
            }))
            .is_err()
        );
        assert!(
            filter_of(Some(&wire::AuditFilter {
                outcome: 99,
                ..Default::default()
            }))
            .is_err()
        );
        assert!(filter_of(None).unwrap().actions.is_empty());
    }

    #[test]
    fn page_tokens_are_numbers() {
        assert_eq!(page_after("").unwrap(), None);
        assert_eq!(page_after("42").unwrap(), Some(42));
        assert!(page_after("4 2").is_err());
        assert!(page_after("-1").is_err());
        assert!(page_after("99999999999999999999").is_err());
    }
}
