//! Updates: what could move, and recording the moves as changes.
//!
//! [`DepsHub::plan`] turns a revision's packages into one row each: the newest version, the version a
//! proposal would pin ([`super::plan`]), what that fixes and what it leaves unfixed. `ListUpdates` shows
//! the rows. `ProposeUpdate` groups them ([`super::repo_config`]), rewrites the files of each group
//! ([`super::apply`]) and records each group as one changeset on `refs/heads/agent/deps/<group>-<hash>`.
//! The ref is built from that prefix and a slug, so no request can name trunk or any other ref.

use std::collections::{BTreeMap, HashMap};

use asc_core::policy::DepSeverity;
use asc_rpc::server::{CallCtx, MutationsHandler, wire};
use bytes::Bytes;
use futures::StreamExt as _;
use tonic::Status;

use super::REF_PREFIX;
use super::apply::{Move, apply};
use super::hub::{DepsHub, ecosystems_of, hex, kind_from_wire, kind_wire};
use super::listing::{Meta, Versions};
use super::model::{Dep, Ecosystem, Lookup};
use super::osv::Query;
use super::plan::{self, Pick, effective};
use super::repo_config::{Config as RepoConfig, MAX_PROPOSALS, slugify};
use super::scan::{Snapshot, distinct};
use super::version::{UpdateKind, Version};
use crate::audit::NewEvent;
use crate::audit::actor::actor_of;
use crate::registry::RepoEntry;
use crate::review::{handle_of, internal, sql};

/// How many candidate versions of a security update are checked against the advisories of the
/// candidate itself before the update is given up on.
const RECHECKS: usize = 5;

/// One package that could move.
#[derive(Clone, Debug)]
pub struct Row {
    pub eco: Ecosystem,
    pub name: String,
    pub current: String,
    pub latest: Option<String>,
    pub latest_kind: Option<UpdateKind>,
    pub target: Option<String>,
    pub target_kind: Option<UpdateKind>,
    pub direct: bool,
    pub paths: Vec<String>,
    pub fixes: Vec<String>,
    pub unfixed: Vec<String>,
    pub group: String,
    pub ignored: bool,
    /// Why there is no target, or what is not known.
    pub error: String,
    pub meta: Meta,
}

pub struct Plan {
    pub rows: Vec<Row>,
    /// Packages whose registry could not be asked.
    pub unknown_packages: u32,
    pub errors: Vec<String>,
}

fn kind_of(cur: &str, to: &str, eco: Ecosystem) -> Option<UpdateKind> {
    let s = eco.scheme();
    UpdateKind::between(&Version::parse(s, cur)?, &Version::parse(s, to)?)
}

impl DepsHub {
    /// The rows for the packages of `snap`. `security_only` keeps only those a security fix moves.
    pub async fn plan(
        &self,
        snap: &Snapshot,
        cfg: &RepoConfig,
        security_only: bool,
        max_kind: UpdateKind,
        refresh: bool,
    ) -> Plan {
        let today = self.today();
        let assessed = self.assess(&snap.deps, refresh).await;
        let mut errors = assessed.errors.clone();
        let (pkgs, _) = distinct(&snap.deps, self.cfg.max_packages);
        // The registries: each package once, a few at a time.
        let mut names: Vec<(Ecosystem, String)> = pkgs
            .iter()
            .filter(|(e, _, _)| self.latest.available(*e))
            .map(|(e, n, _)| (*e, n.clone()))
            .collect();
        names.sort();
        names.dedup();
        let looked: Vec<(
            (Ecosystem, String),
            Result<std::sync::Arc<Versions>, String>,
        )> = futures::stream::iter(names.into_iter().map(|(e, n)| async move {
            let r = self.latest.versions(e, &n, refresh).await;
            ((e, n), r)
        }))
        .buffer_unordered(self.cfg.concurrency.max(1))
        .collect()
        .await;
        let registry: HashMap<(Ecosystem, String), Result<std::sync::Arc<Versions>, String>> =
            looked.into_iter().collect();
        let mut unknown = 0u32;
        let mut rows: Vec<Row> = Vec::new();
        for (eco, name, current) in &pkgs {
            if !cfg.wants(*eco) {
                continue;
            }
            let occurrences: Vec<&Dep> = snap
                .deps
                .iter()
                .filter(|d| d.ecosystem == *eco && d.name == *name && d.version == *current)
                .collect();
            let mut paths: Vec<String> = occurrences.iter().map(|d| d.path.clone()).collect();
            paths.sort();
            paths.dedup();
            let mut row = Row {
                eco: *eco,
                name: name.clone(),
                current: current.clone(),
                latest: None,
                latest_kind: None,
                target: None,
                target_kind: None,
                direct: occurrences.iter().any(|d| d.direct),
                paths,
                fixes: Vec::new(),
                unfixed: Vec::new(),
                group: String::new(),
                ignored: false,
                error: String::new(),
                meta: Meta::default(),
            };
            let versions = match registry.get(&(*eco, name.clone())) {
                Some(Ok(v)) => v.clone(),
                Some(Err(e)) => {
                    unknown += 1;
                    row.error = e.clone();
                    if !errors.contains(e) && errors.len() < 20 {
                        errors.push(e.clone());
                    }
                    rows.push(row);
                    continue;
                }
                None => {
                    // No registry API (Swift, CocoaPods) or switched off.
                    unknown += 1;
                    row.error = "no registry lookup for this ecosystem".into();
                    rows.push(row);
                    continue;
                }
            };
            let advisories: Vec<&super::model::Advisory> =
                match assessed.answers.get(&(*eco, name.clone(), current.clone())) {
                    Some(Lookup::Known(a)) => a
                        .iter()
                        .filter(|a| {
                            let mut ids = vec![a.id.as_str()];
                            ids.extend(a.aliases.iter().map(String::as_str));
                            !cfg.advisory_ignored(*eco, name, &ids, &today)
                        })
                        .filter(|a| cfg.severity_threshold.is_none_or(|t| effective(a) >= t))
                        .collect(),
                    _ => Vec::new(),
                };
            row.latest = versions.latest.clone();
            row.latest_kind = versions
                .latest
                .as_deref()
                .and_then(|l| kind_of(current, l, *eco));
            let pick = plan::choose(
                *eco,
                current,
                &versions.versions,
                &advisories,
                security_only,
                max_kind,
            );
            let mut target = match pick {
                Pick::Target(t) => Some(t),
                Pick::Nothing => {
                    if security_only || row.latest_kind.is_none() {
                        continue;
                    }
                    None
                }
                Pick::Blocked(why) => {
                    row.error = why;
                    None
                }
            };
            // The target's own advisories: a candidate that has more of them is not a fix.
            if let Some(t) = target.clone() {
                let candidates: Vec<String> = if security_only {
                    plan::security_candidates(
                        *eco,
                        current,
                        &versions.versions,
                        &advisories,
                        max_kind,
                    )
                    .unwrap_or_else(|_| vec![t.clone()])
                } else {
                    vec![t.clone()]
                };
                let mut settled = None;
                let mut check_failed = false;
                for cand in candidates
                    .iter()
                    .take(if security_only { RECHECKS } else { 1 })
                {
                    let q = Query {
                        eco: *eco,
                        name: name.clone(),
                        version: cand.clone(),
                    };
                    let a = self.osv.lookup(std::slice::from_ref(&q), refresh).await;
                    match a.lookups.into_iter().next() {
                        Some(Lookup::Known(own)) => {
                            let fixable_later = own.iter().any(|x| {
                                plan::fixable(*eco, cand, x)
                                    && effective(x)
                                        >= cfg.severity_threshold.unwrap_or(DepSeverity::Low)
                            });
                            if security_only && fixable_later {
                                continue;
                            }
                            settled = Some((cand.clone(), own));
                            break;
                        }
                        Some(Lookup::Uncovered) => {
                            settled = Some((cand.clone(), Vec::new()));
                            break;
                        }
                        _ => {
                            check_failed = true;
                            break;
                        }
                    }
                }
                match settled {
                    Some((cand, own)) => {
                        let mut unfixed = plan::unfixed(*eco, current, &cand, &advisories);
                        for a in &own {
                            if !unfixed.contains(&a.id) {
                                unfixed.push(a.id.clone());
                            }
                        }
                        unfixed.sort();
                        row.fixes = plan::fixes(*eco, current, &cand, &advisories)
                            .into_iter()
                            .filter(|id| !unfixed.contains(id))
                            .collect();
                        row.unfixed = unfixed;
                        row.meta = versions.meta.get(&cand).cloned().unwrap_or_default();
                        row.target_kind = kind_of(current, &cand, *eco);
                        target = Some(cand);
                    }
                    None if check_failed && security_only => {
                        row.error = "the advisories of the new version could not be checked".into();
                        target = None;
                    }
                    None if check_failed => {
                        row.error = "the advisories of the new version could not be checked".into();
                        row.target_kind = kind_of(current, &t, *eco);
                        row.meta = versions.meta.get(&t).cloned().unwrap_or_default();
                    }
                    None => {
                        row.error =
                            "every candidate version has further advisories with fixes".into();
                        target = None;
                    }
                }
            }
            row.target = target;
            let kind = row.target_kind.or(row.latest_kind);
            if let Some(k) = kind {
                row.ignored = cfg.update_ignored(*eco, name, k, &today);
                row.group = cfg.group_of(*eco, name, k);
            }
            rows.push(row);
        }
        rows.sort_by(|a, b| (a.eco, &a.name, &a.current).cmp(&(b.eco, &b.name, &b.current)));
        Plan {
            rows,
            unknown_packages: unknown,
            errors,
        }
    }

    pub async fn list_updates(
        &self,
        ctx: &CallCtx,
        req: wire::ListDepUpdatesRequest,
    ) -> Result<wire::ListDepUpdatesResponse, Status> {
        let _permit = self.permit().await?;
        let entry = self.entry(ctx).await?;
        let ids = self.codec(ctx, &entry);
        let trunk = self.trunk(&entry).await?;
        let rev = self.rev(&ids, &entry, &req.rev).await?;
        let loaded = self.repo_config(&entry, trunk).await?;
        let mut ecos = ecosystems_of(&req.ecosystems)?;
        if ecos.is_empty() {
            ecos = loaded.config.ecosystems.clone();
        }
        let snap = self.snapshot(&entry, rev, &ecos).await?;
        let security = req.security_only || loaded.config.security_only;
        let plan = self
            .plan(
                &snap,
                &loaded.config,
                security,
                loaded.config.max_kind,
                false,
            )
            .await;
        let updates: Vec<wire::DepUpdate> = plan
            .rows
            .iter()
            .filter(|r| req.include_ignored || !r.ignored)
            .map(update_wire)
            .collect();
        let (pkgs, _) = distinct(&snap.deps, usize::MAX);
        Ok(wire::ListDepUpdatesResponse {
            updates,
            packages: u32::try_from(pkgs.len()).unwrap_or(u32::MAX),
            unknown_packages: plan.unknown_packages,
            lookup_errors: plan.errors,
            scanned: ids.changeset_out(rev).await?,
        })
    }

    pub async fn propose_update(
        &self,
        ctx: &CallCtx,
        req: wire::ProposeDepUpdateRequest,
    ) -> Result<wire::ProposeDepUpdateResponse, Status> {
        let _permit = self.permit().await?;
        let entry = self.entry(ctx).await?;
        let ids = self.codec(ctx, &entry);
        let trunk = self.trunk(&entry).await?;
        let base = self.rev(&ids, &entry, &req.rev).await?;
        let loaded = self.repo_config(&entry, trunk).await?;
        if !loaded.error.is_empty() {
            return Err(Status::failed_precondition(format!(
                ".asc/deps.toml cannot be used: {}",
                loaded.error
            )));
        }
        let cfg = &loaded.config;
        let ecos = cfg.ecosystems.clone();
        let snap = self.snapshot(&entry, base, &ecos).await?;
        let security = req.security_only || cfg.security_only;
        let max_kind = kind_from_wire(req.max_kind)?.unwrap_or(cfg.max_kind);
        let plan = self.plan(&snap, cfg, security, max_kind, false).await;
        let mut selected = Vec::new();
        for s in &req.selectors {
            let eco = Ecosystem::parse(&s.ecosystem)
                .ok_or_else(|| Status::invalid_argument("a selector names an unknown ecosystem"))?;
            selected.push((eco, eco.canonical(&s.name)));
        }
        if selected.len() > 1000 {
            return Err(Status::invalid_argument("too many selectors"));
        }
        let mut skipped: Vec<wire::DepSkipped> = Vec::new();
        let mut skip = |r: &Row, why: String| {
            skipped.push(wire::DepSkipped {
                ecosystem: r.eco.osv_name().to_owned(),
                name: r.name.clone(),
                reason: why,
            });
        };
        let mut groups: BTreeMap<String, Vec<Row>> = BTreeMap::new();
        for r in plan.rows {
            let wanted = selected.is_empty()
                || selected
                    .iter()
                    .any(|(e, n)| *e == r.eco && *n == r.eco.canonical(&r.name));
            if !wanted {
                continue;
            }
            if r.ignored {
                skip(&r, "ignored by .asc/deps.toml".into());
                continue;
            }
            let Some(_) = &r.target else {
                if !r.error.is_empty() {
                    skip(&r, r.error.clone());
                }
                continue;
            };
            groups.entry(r.group.clone()).or_default().push(r);
        }
        let limit = match req.max_groups {
            0 => cfg.max_proposals,
            n => n.min(MAX_PROPOSALS),
        } as usize;
        let mut proposals: Vec<wire::DepProposal> = Vec::new();
        for (n, (group, rows)) in groups.into_iter().enumerate() {
            if n >= limit {
                for r in &rows {
                    skip(r, format!("over the limit of {limit} changes in one run"));
                }
                continue;
            }
            let moves: Vec<Move> = rows
                .iter()
                .map(|r| Move {
                    eco: r.eco,
                    name: r.name.clone(),
                    from: r.current.clone(),
                    to: r.target.clone().unwrap_or_default(),
                    meta: r.meta.clone(),
                })
                .collect();
            let applied = apply(&snap.texts, &snap.deps, &moves);
            for (i, why) in &applied.failed {
                skip(&rows[*i], why.clone());
            }
            let done: Vec<&Row> = applied.done.iter().map(|i| &rows[*i]).collect();
            if done.is_empty() {
                continue;
            }
            let hash = update_hash(&done);
            let ref_name = format!("{REF_PREFIX}{}-{hash}", slugify(&group));
            let message = commit_message(&group, &done, &applied.notes);
            let mut proposal = wire::DepProposal {
                group: group.clone(),
                r#ref: ref_name.clone(),
                changeset: Bytes::new(),
                change: Bytes::new(),
                updates: done.iter().map(|r| update_wire(r)).collect(),
                files: applied
                    .files
                    .keys()
                    .map(|p| Bytes::from(p.clone().into_bytes()))
                    .collect(),
                message: message.clone(),
                notes: applied.notes.clone(),
            };
            if !req.dry_run {
                match self
                    .commit_files(
                        ctx,
                        &entry,
                        &ids,
                        base,
                        &ref_name,
                        &message,
                        &applied.files,
                        &[],
                    )
                    .await
                {
                    Ok((cs, change)) => {
                        proposal.changeset = cs;
                        proposal.change = change.clone();
                        let event = NewEvent::new("deps.proposed", actor_of(&ctx.principal.id))
                            .repo(ctx.repo.as_str())
                            .target(&ref_name)
                            .detail("ref", ref_name.as_str())
                            .detail("change", hex(&change))
                            .detail("updates", done.len().to_string());
                        crate::audit::store::record_best_effort(self.registry.pool(), &event).await;
                    }
                    Err(s)
                        if matches!(
                            s.code(),
                            tonic::Code::Aborted | tonic::Code::AlreadyExists
                        ) =>
                    {
                        for r in &done {
                            skip(r, format!("already proposed on {ref_name}"));
                        }
                        continue;
                    }
                    Err(s) => return Err(s),
                }
            }
            proposals.push(proposal);
        }
        Ok(wire::ProposeDepUpdateResponse {
            proposals,
            skipped,
            base: ids.changeset_out(base).await?,
        })
    }

    /// Records `files` (path to new text; a path in `delete` is removed) as one changeset on `ref_name`.
    #[allow(clippy::too_many_arguments)] // one commit, called from two places
    pub(crate) async fn commit_files(
        &self,
        ctx: &CallCtx,
        entry: &RepoEntry,
        ids: &crate::idmode::IdCodec,
        base: asc_core::id::ChangesetId,
        ref_name: &str,
        message: &str,
        files: &BTreeMap<String, String>,
        delete: &[String],
    ) -> Result<(Bytes, Bytes), Status> {
        // The only refs this module writes are under the agent/deps namespace.
        if !ref_name.starts_with(REF_PREFIX)
            || ref_name.contains("..")
            || ref_name.contains("//")
            || ref_name.ends_with('/')
        {
            return Err(internal("deps: ref", &"a proposal ref outside agent/deps"));
        }
        let mut ops: Vec<wire::FileOp> = files
            .iter()
            .map(|(p, t)| wire::FileOp {
                path: Bytes::from(p.clone().into_bytes()),
                op: Some(wire::file_op::Op::PutContent(Bytes::from(
                    t.clone().into_bytes(),
                ))),
                mode: 0,
            })
            .collect();
        ops.extend(delete.iter().map(|p| wire::FileOp {
            path: Bytes::from(p.clone().into_bytes()),
            op: Some(wire::file_op::Op::Delete(true)),
            mode: 0,
        }));
        let name = handle_of(&ctx.principal.id).unwrap_or_else(|| "asc".to_owned());
        let sig = wire::Signature {
            name: format!("{name} (asc deps)").into_bytes().into(),
            email: format!("{name}@asc.invalid").into_bytes().into(),
            time: self.now(),
            tz_min: 0,
        };
        let commit = wire::CommitRequest {
            repo: Some(wire::RepoRef {
                name: ctx.repo.to_string(),
                id_mode: ctx.id_mode.to_wire(),
            }),
            parent: hex(&ids.changeset_out(base).await?),
            ops,
            message: Bytes::from(message.as_bytes().to_vec()),
            author: Some(sig.clone()),
            committer: Some(sig),
            update_ref: ref_name.to_owned(),
            expect: None,
            sign: false,
        };
        // One request may record several changes; each gets an operation id derived from the
        // request's, so a retry of the request replays each of them and none collides with another.
        let mut ctx = ctx.clone();
        ctx.op_id = ctx.op_id.map(|op| {
            let mut h = blake3::Hasher::new();
            h.update(op.as_bytes());
            h.update(ref_name.as_bytes());
            let mut b = [0u8; 16];
            b.copy_from_slice(&h.finalize().as_bytes()[..16]);
            uuid::Uuid::from_bytes(b)
        });
        let made = self.commits.commit(&ctx, commit).await?;
        let cs = ids.changeset_in(&made.id).await?;
        let change: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT change FROM changes WHERE ns = $1 AND cs = $2")
                .bind(entry.ns.as_bytes().to_vec())
                .bind(cs.as_bytes().to_vec())
                .fetch_optional(self.registry.pool())
                .await
                .map_err(sql)?;
        Ok((made.id, change.map(Bytes::from).unwrap_or_default()))
    }
}

/// The first 8 hex characters of a hash of the moves, so the same moves name the same ref.
fn update_hash(rows: &[&Row]) -> String {
    let mut lines: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{}|{}|{}|{}",
                r.eco.osv_name(),
                r.name,
                r.current,
                r.target.as_deref().unwrap_or("")
            )
        })
        .collect();
    lines.sort();
    blake3::hash(lines.join("\n").as_bytes()).to_hex()[..8].to_owned()
}

fn commit_message(group: &str, rows: &[&Row], notes: &[String]) -> String {
    let mut m = match rows {
        [r] => format!(
            "deps: update {} {} to {} ({})",
            r.name,
            r.current,
            r.target.as_deref().unwrap_or(""),
            r.eco.osv_name()
        ),
        _ => format!("deps: update {} dependencies ({group})", rows.len()),
    };
    m.push_str("\n\n");
    for r in rows {
        m.push_str(&format!(
            "- {} {} {} -> {}",
            r.eco.osv_name(),
            r.name,
            r.current,
            r.target.as_deref().unwrap_or("")
        ));
        if let Some(k) = r.target_kind {
            m.push_str(&format!(" ({k})"));
        }
        if !r.fixes.is_empty() {
            m.push_str(&format!(", fixes {}", r.fixes.join(", ")));
        }
        if !r.unfixed.is_empty() {
            m.push_str(&format!(", still affected by {}", r.unfixed.join(", ")));
        }
        m.push('\n');
    }
    if !notes.is_empty() {
        m.push_str("\nStill to do:\n");
        for n in notes {
            m.push_str(&format!("- {n}\n"));
        }
    }
    m.push_str("\nProposed by asc deps; review it like any change.\n");
    m
}

pub fn update_wire(r: &Row) -> wire::DepUpdate {
    wire::DepUpdate {
        ecosystem: r.eco.osv_name().to_owned(),
        name: r.name.clone(),
        current: r.current.clone(),
        latest: r.latest.clone().unwrap_or_default(),
        kind: kind_wire(r.latest_kind),
        direct: r.direct,
        path: Bytes::from(r.paths.first().cloned().unwrap_or_default().into_bytes()),
        target: r.target.clone().unwrap_or_default(),
        fixes: r.fixes.clone(),
        group: r.group.clone(),
        ignored: r.ignored,
        error: r.error.clone(),
        unfixed: r.unfixed.clone(),
    }
}
