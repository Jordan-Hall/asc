//! `GetConfig` and `SetConfig`: the repository settings as the service shows and changes them.
//!
//! `SetConfig` never writes trunk. It validates the settings (by writing them as `.asc/deps.toml` and
//! reading that text back with the strict reader), adds the scheduled workflow when there is a schedule
//! (and removes the one it generated when there is none), and records the files as one changeset on
//! `refs/heads/agent/deps/config-<hash>`, to be reviewed and landed like any change.

use std::collections::BTreeMap;

use asc_rpc::server::{CallCtx, wire};
use tonic::Status;

use super::REF_PREFIX;
use super::hub::{DepsHub, ecosystems_of, hex, kind_from_wire, kind_wire, sev_from_wire, sev_wire};
use super::model::Ecosystem;
use super::repo_config::{self, Config as RepoConfig, DEFAULT_PROPOSALS, Group, Grouping, Ignore};
use super::version::UpdateKind;
use super::workflow;
use crate::audit::NewEvent;
use crate::audit::actor::actor_of;

/// The line the generated workflow starts with, which marks it as ours to remove.
const GENERATED: &str = "# Generated from .asc/deps.toml";

pub fn config_wire(c: &RepoConfig) -> wire::DepsConfig {
    wire::DepsConfig {
        version: repo_config::VERSION,
        ecosystems: c
            .ecosystems
            .iter()
            .map(|e| e.osv_name().to_owned())
            .collect(),
        schedule: c.schedule.clone().unwrap_or_default(),
        security_only: c.security_only,
        severity_threshold: sev_wire(c.severity_threshold),
        max_kind: kind_wire(Some(c.max_kind)),
        max_proposals: c.max_proposals,
        grouping: c.grouping.name().to_owned(),
        groups: c
            .groups
            .iter()
            .map(|g| wire::DepGroup {
                name: g.name.clone(),
                ecosystem: g
                    .ecosystem
                    .map(|e| e.osv_name().to_owned())
                    .unwrap_or_default(),
                patterns: g.patterns.clone(),
                kinds: g.kinds.iter().map(|k| kind_wire(Some(*k))).collect(),
            })
            .collect(),
        ignore: c
            .ignore
            .iter()
            .map(|i| wire::DepIgnore {
                ecosystem: i
                    .ecosystem
                    .map(|e| e.osv_name().to_owned())
                    .unwrap_or_default(),
                name: i.name.clone(),
                kinds: i.kinds.iter().map(|k| kind_wire(Some(*k))).collect(),
                advisories: i.advisories.clone(),
                until: i.until.clone(),
                reason: i.reason.clone(),
            })
            .collect(),
    }
}

fn kinds_from(list: &[i32]) -> Result<Vec<UpdateKind>, Status> {
    let mut out = Vec::new();
    for k in list {
        if let Some(kind) = kind_from_wire(*k)? {
            out.push(kind);
        }
    }
    Ok(out)
}

fn eco_from(s: &str) -> Result<Option<Ecosystem>, Status> {
    if s.is_empty() {
        return Ok(None);
    }
    Ecosystem::parse(s)
        .map(Some)
        .ok_or_else(|| Status::invalid_argument(format!("{s:?} is not a known ecosystem")))
}

/// The settings a request carries, validated by the strict reader.
pub fn config_from_wire(c: &wire::DepsConfig) -> Result<RepoConfig, Status> {
    let bad = |m: String| Status::invalid_argument(m);
    if c.version != 0 && c.version != repo_config::VERSION {
        return Err(bad(format!("version must be {}", repo_config::VERSION)));
    }
    if c.groups.len() > repo_config::MAX_ENTRIES || c.ignore.len() > repo_config::MAX_ENTRIES {
        return Err(bad("too many groups or ignore entries".into()));
    }
    let draft = RepoConfig {
        ecosystems: ecosystems_of(&c.ecosystems)?,
        schedule: (!c.schedule.trim().is_empty()).then(|| c.schedule.trim().to_owned()),
        security_only: c.security_only,
        severity_threshold: sev_from_wire(c.severity_threshold)?,
        max_kind: kind_from_wire(c.max_kind)?.unwrap_or(UpdateKind::Minor),
        max_proposals: if c.max_proposals == 0 {
            DEFAULT_PROPOSALS
        } else {
            c.max_proposals
        },
        grouping: Grouping::parse(&c.grouping)
            .ok_or_else(|| bad("grouping is not none, ecosystem, kind or all".into()))?,
        groups: c
            .groups
            .iter()
            .map(|g| {
                Ok(Group {
                    name: g.name.clone(),
                    ecosystem: eco_from(&g.ecosystem)?,
                    patterns: g.patterns.clone(),
                    kinds: kinds_from(&g.kinds)?,
                })
            })
            .collect::<Result<_, Status>>()?,
        ignore: c
            .ignore
            .iter()
            .map(|i| {
                Ok(Ignore {
                    ecosystem: eco_from(&i.ecosystem)?,
                    name: i.name.clone(),
                    kinds: kinds_from(&i.kinds)?,
                    advisories: i.advisories.clone(),
                    until: i.until.clone(),
                    reason: i.reason.clone(),
                })
            })
            .collect::<Result<_, Status>>()?,
    };
    RepoConfig::parse(draft.render().as_bytes()).map_err(bad)
}

impl DepsHub {
    /// The scheduled workflow for `cfg`, or empty when there is no schedule or no public URL.
    fn workflow_for(&self, ctx: &CallCtx, cfg: &RepoConfig) -> Result<String, Status> {
        match (&cfg.schedule, &self.server_url) {
            (Some(cron), Some(url)) => {
                workflow::render(ctx.repo.as_str(), url, cron).map_err(Status::failed_precondition)
            }
            _ => Ok(String::new()),
        }
    }

    pub async fn get_config(
        &self,
        ctx: &CallCtx,
        req: wire::GetDepsConfigRequest,
    ) -> Result<wire::GetDepsConfigResponse, Status> {
        let entry = self.entry(ctx).await?;
        let ids = self.codec(ctx, &entry);
        let at = if req.rev.is_empty() {
            self.trunk(&entry).await?
        } else {
            Some(self.rev(&ids, &entry, &req.rev).await?)
        };
        let loaded = self.repo_config(&entry, at).await?;
        let workflow = self.workflow_for(ctx, &loaded.config).unwrap_or_default();
        Ok(wire::GetDepsConfigResponse {
            config: Some(config_wire(&loaded.config)),
            source: loaded.source.to_owned(),
            text: loaded.text,
            error: loaded.error,
            workflow,
            supported: Ecosystem::ALL
                .iter()
                .map(|e| e.osv_name().to_owned())
                .collect(),
        })
    }

    pub async fn set_config(
        &self,
        ctx: &CallCtx,
        req: wire::SetDepsConfigRequest,
    ) -> Result<wire::SetDepsConfigResponse, Status> {
        let cfg = config_from_wire(
            req.config
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing config"))?,
        )?;
        let text = cfg.render();
        let workflow = if cfg.schedule.is_some() {
            if self.server_url.is_none() {
                return Err(Status::failed_precondition(
                    "a schedule needs this server's public_url, which is not set",
                ));
            }
            self.workflow_for(ctx, &cfg)?
        } else {
            String::new()
        };
        let entry = self.entry(ctx).await?;
        let ids = self.codec(ctx, &entry);
        let base = self
            .trunk(&entry)
            .await?
            .ok_or_else(|| Status::failed_precondition("the repository has no commits"))?;
        let root = self.view.root_of(&entry, base).await?;
        let read = |path: &'static str| {
            let view = &self.view;
            let entry = &entry;
            async move {
                match view.read(entry, root, path, 64 * 1024).await {
                    Ok(b) => Ok(b.map(|b| String::from_utf8_lossy(&b).into_owned())),
                    Err(s) if s.code() == tonic::Code::FailedPrecondition => Ok(None),
                    Err(s) => Err(s),
                }
            }
        };
        let have_config = read(repo_config::PATH).await?;
        let have_flow = read(workflow::PATH).await?;
        let mut files: BTreeMap<String, String> = BTreeMap::new();
        let mut delete: Vec<String> = Vec::new();
        if have_config.as_deref() != Some(text.as_str()) {
            files.insert(repo_config::PATH.to_owned(), text.clone());
        }
        if !workflow.is_empty() {
            if have_flow.as_deref() != Some(workflow.as_str()) {
                files.insert(workflow::PATH.to_owned(), workflow.clone());
            }
        } else if have_flow
            .as_deref()
            .is_some_and(|f| f.starts_with(GENERATED))
        {
            delete.push(workflow::PATH.to_owned());
        }
        let mut out = wire::SetDepsConfigResponse {
            text: text.clone(),
            workflow: workflow.clone(),
            ..wire::SetDepsConfigResponse::default()
        };
        if (files.is_empty() && delete.is_empty()) || req.dry_run {
            return Ok(out);
        }
        let hash =
            blake3::hash(format!("{text}\n--\n{workflow}").as_bytes()).to_hex()[..8].to_owned();
        let ref_name = format!("{REF_PREFIX}config-{hash}");
        out.r#ref = ref_name.clone();
        let message = "deps: configure dependency updates\n\nWrites .asc/deps.toml and the scheduled workflow that goes with it.\n";
        match self
            .commit_files(ctx, &entry, &ids, base, &ref_name, message, &files, &delete)
            .await
        {
            Ok((cs, change)) => {
                out.changeset = cs;
                out.change = change.clone();
                let event = NewEvent::new("deps.config.proposed", actor_of(&ctx.principal.id))
                    .repo(ctx.repo.as_str())
                    .target(&ref_name)
                    .detail("ref", ref_name.as_str())
                    .detail("change", hex(&change));
                crate::audit::store::record_best_effort(self.registry.pool(), &event).await;
            }
            // The same settings were proposed before.
            Err(s) if matches!(s.code(), tonic::Code::Aborted | tonic::Code::AlreadyExists) => {}
            Err(s) => return Err(s),
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_cfg() -> wire::DepsConfig {
        wire::DepsConfig {
            version: 1,
            ecosystems: vec!["cargo".into(), "npm".into()],
            schedule: "0 4 * * 1".into(),
            security_only: true,
            severity_threshold: wire::DepSeverity::High as i32,
            max_kind: wire::DepUpdateKind::Major as i32,
            max_proposals: 3,
            grouping: "ecosystem".into(),
            groups: vec![wire::DepGroup {
                name: "lint".into(),
                ecosystem: "npm".into(),
                patterns: vec!["eslint*".into()],
                kinds: vec![wire::DepUpdateKind::Patch as i32],
            }],
            ignore: vec![wire::DepIgnore {
                ecosystem: "npm".into(),
                name: "left-pad".into(),
                kinds: vec![wire::DepUpdateKind::Major as i32],
                advisories: vec!["GHSA-x".into()],
                until: "2030-01-01".into(),
                reason: "pinned".into(),
            }],
        }
    }

    #[test]
    fn settings_round_trip_through_the_wire_and_the_strict_reader() {
        let c = config_from_wire(&wire_cfg()).unwrap();
        assert_eq!(c.ecosystems, [Ecosystem::CratesIo, Ecosystem::Npm]);
        assert_eq!(c.max_proposals, 3);
        let back = config_wire(&c);
        assert_eq!(back.ecosystems, ["crates.io", "npm"]);
        assert_eq!(back.schedule, "0 4 * * 1");
        assert_eq!(config_from_wire(&back).unwrap(), c);
        // An empty request is the defaults.
        let d = config_from_wire(&wire::DepsConfig::default()).unwrap();
        assert_eq!(d, RepoConfig::default());
    }

    #[test]
    fn invalid_settings_are_refused_with_the_reason() {
        let with = |f: &dyn Fn(&mut wire::DepsConfig)| {
            let mut c = wire_cfg();
            f(&mut c);
            config_from_wire(&c).unwrap_err().message().to_owned()
        };
        assert!(with(&|c| c.version = 2).contains("version"));
        assert!(with(&|c| c.ecosystems = vec!["cobol".into()]).contains("cobol"));
        assert!(with(&|c| c.schedule = "* * * * *".into()).contains("once an hour"));
        assert!(with(&|c| c.max_proposals = 51).contains("max_proposals"));
        assert!(with(&|c| c.grouping = "weekly".into()).contains("grouping"));
        assert!(with(&|c| c.groups[0].name = "A B".into()).contains("group"));
        assert!(with(&|c| c.ignore[0].until = "2030-02-30".into()).contains("until"));
        assert!(with(&|c| c.severity_threshold = 99).contains("severity"));
        assert!(with(&|c| c.ignore[0].reason = "line\nbreak".into()).contains("reason"));
        // A value that would break out of the file is refused, not written.
        let e = with(&|c| c.ignore[0].name = "x\"\n[[group]]".into());
        assert!(!e.is_empty());
        let e = with(&|c| c.ignore[0].reason = "x\u{0}".into());
        assert!(!e.is_empty());
    }
}
