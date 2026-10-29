//! `Releases`: the record of a tagged release with notes, provenance and asset checksums
//! (docs/BUILDOUT.md section 26).
//!
//! * **Notes** are written from the changesets in `from..to`, newest first: the category comes from a
//!   `Release-Note: added|changed|fixed|skip` trailer, else from a leading word of the subject
//!   (`feat`/`add` is added, `fix`/`bug` is fixed), else "changed". At most [`MAX_NOTES`] changesets are read.
//! * **Provenance** is computed when a release is created and kept with it: the jobs of the run that built
//!   it (from `check_runs`/`check_jobs`) and the signature on the release commit (`change_signatures`).
//! * **Assets** are metadata only: name, size, SHA-256 and the https address the bytes are served from.
//! * A **draft** is seen and published by writers only.

pub mod rpc;

use std::collections::HashMap;

use asc_core::id::ChangesetId;
use asc_engine::graph::LogQuery;
use asc_rpc::server::{CallCtx, wire};
use futures::TryStreamExt as _;
use sqlx::Row;
use tonic::Status;

use crate::idmode::IdCodec;
use crate::registry::RepoEntry;
use crate::review::{Reviews, acting, handle_of, now, sql, user_id_col};

/// Changesets read for notes and counts.
pub const MAX_NOTES: usize = 500;
/// Largest notes text.
pub const MAX_NOTES_BYTES: usize = 64 * 1024;
/// Assets of one release.
pub const MAX_ASSETS: usize = 100;
/// Releases listed per page.
const DEFAULT_PAGE: usize = 30;
const MAX_PAGE: usize = 100;
const MAX_SUBJECT: usize = 200;

const STABLE: i64 = wire::ReleaseChannel::Stable as i64;
const PRE: i64 = wire::ReleaseChannel::PreRelease as i64;

fn bad(m: impl Into<String>) -> Status {
    Status::invalid_argument(m.into())
}

/// A tag the server accepts: 1 to 128 characters of letters, digits and `._+-/`, not starting with `-` or `/`.
pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 128
        && !tag.starts_with(['-', '/', '.'])
        && !tag.ends_with(['/', '.'])
        && !tag.contains("//")
        && !tag.contains("..")
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b'/'))
}

/// The category of a message: a trailer, then the subject's leading word; `None` leaves the change out.
pub fn category(message: &str) -> Option<&'static str> {
    for line in message.lines().rev().take(8) {
        if let Some(v) = line.strip_prefix("Release-Note:") {
            return match v.trim().to_ascii_lowercase().as_str() {
                "added" | "add" => Some("added"),
                "fixed" | "fix" => Some("fixed"),
                "changed" | "change" => Some("changed"),
                "skip" | "none" => None,
                _ => Some("changed"),
            };
        }
    }
    let subject = message
        .lines()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let word: String = subject
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    Some(match word.as_str() {
        "feat" | "feature" | "add" | "adds" | "added" => "added",
        "fix" | "fixes" | "fixed" | "bug" | "bugfix" => "fixed",
        _ => "changed",
    })
}

/// The next tag after `latest`: `v1.2.3` is `v1.2.4`, `2.0.9` is `2.0.10`; anything else has none.
pub fn next_tag(latest: &str) -> String {
    let (prefix, rest) = match latest.strip_prefix('v') {
        Some(r) => ("v", r),
        None => ("", latest),
    };
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return String::new();
    }
    match parts[2].parse::<u64>() {
        Ok(patch) if patch < u64::MAX => format!("{prefix}{}.{}.{}", parts[0], parts[1], patch + 1),
        _ => String::new(),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn markdown(notes: &[wire::ReleaseNote]) -> String {
    let mut out = String::new();
    for (key, title) in [
        ("added", "Added"),
        ("changed", "Changed"),
        ("fixed", "Fixed"),
    ] {
        let items: Vec<&wire::ReleaseNote> = notes.iter().filter(|n| n.category == key).collect();
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("### {title}\n\n"));
        for n in items {
            let id = if n.change.is_empty() {
                String::new()
            } else {
                format!(", change {}", &hex(&n.change)[..8])
            };
            out.push_str(&format!("- {} ({}{id})\n", n.subject, n.author));
        }
        out.push('\n');
    }
    out
}

fn valid_url(u: &str) -> bool {
    u.is_empty()
        || (u.len() <= 2048
            && u.starts_with("https://")
            && u.len() > "https://".len()
            && !u.chars().any(|c| c.is_control() || c.is_whitespace()))
}

fn valid_asset(a: &wire::ReleaseAsset) -> Result<(), Status> {
    if a.name.is_empty()
        || a.name.len() > 255
        || a.name.contains(['/', '\\'])
        || a.name.chars().any(char::is_control)
        || a.name == "."
        || a.name == ".."
    {
        return Err(bad("an asset name is a file name of at most 255 bytes"));
    }
    if a.sha256.len() != 64
        || !a
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(bad("sha256 is 64 lowercase hex characters"));
    }
    if !valid_url(&a.url) {
        return Err(bad("an asset url is an https address"));
    }
    if a.content_type.len() > 128 || a.content_type.chars().any(char::is_control) {
        return Err(bad("content_type is at most 128 bytes"));
    }
    Ok(())
}

impl Reviews {
    async fn release_row(
        &self,
        entry: &RepoEntry,
        row: &sqlx::any::AnyRow,
    ) -> Result<wire::Release, Status> {
        let ns = entry.ns.as_bytes().to_vec();
        let id: i64 = row.try_get("id").map_err(sql)?;
        let assets = sqlx::query(
            "SELECT name, size, sha256, url, content_type FROM release_assets \
             WHERE ns = $1 AND release_id = $2 ORDER BY name",
        )
        .bind(ns)
        .bind(id)
        .fetch_all(self.registry.pool())
        .await
        .map_err(sql)?;
        let mut out = Vec::with_capacity(assets.len());
        for a in &assets {
            out.push(wire::ReleaseAsset {
                name: a.try_get("name").map_err(sql)?,
                size: u64::try_from(a.try_get::<i64, _>("size").map_err(sql)?).unwrap_or(0),
                sha256: a.try_get("sha256").map_err(sql)?,
                url: a.try_get("url").map_err(sql)?,
                content_type: a.try_get("content_type").map_err(sql)?,
            });
        }
        let n = |c: &str| -> Result<u32, Status> {
            Ok(u32::try_from(row.try_get::<i64, _>(c).map_err(sql)?).unwrap_or(0))
        };
        let published: i64 = row.try_get("published").map_err(sql)?;
        Ok(wire::Release {
            id: u64::try_from(id).unwrap_or(0),
            tag: row.try_get("tag").map_err(sql)?,
            title: row.try_get("title").map_err(sql)?,
            notes: row.try_get("notes").map_err(sql)?,
            channel: i32::try_from(row.try_get::<i64, _>("channel").map_err(sql)?).unwrap_or(0),
            draft: published == 0,
            target: row.try_get::<Vec<u8>, _>("target").map_err(sql)?.into(),
            previous_tag: row.try_get("previous_tag").map_err(sql)?,
            changes: n("changes")?,
            created: row.try_get("created").map_err(sql)?,
            published,
            created_by: row.try_get("created_by").map_err(sql)?,
            created_by_id: u64::try_from(row.try_get::<i64, _>("created_by_id").map_err(sql)?)
                .unwrap_or(0),
            provenance: Some(wire::ReleaseProvenance {
                build_source: row.try_get("build_source").map_err(sql)?,
                build_run: row.try_get("build_run").map_err(sql)?,
                build_url: row.try_get("build_url").map_err(sql)?,
                built_from: row.try_get("built_from").map_err(sql)?,
                checks_passed: n("checks_passed")?,
                checks_total: n("checks_total")?,
                signed_by: row.try_get("signed_by").map_err(sql)?,
                signing_key: row.try_get("signing_key").map_err(sql)?,
                signature_valid: row.try_get::<i64, _>("signature_ok").map_err(sql)? != 0,
            }),
            assets: out,
        })
    }

    async fn release_by_tag(
        &self,
        entry: &RepoEntry,
        tag: &str,
        drafts: bool,
    ) -> Result<wire::Release, Status> {
        if !valid_tag(tag) {
            return Err(bad("invalid tag"));
        }
        let row = sqlx::query("SELECT * FROM releases WHERE ns = $1 AND tag = $2")
            .bind(entry.ns.as_bytes().to_vec())
            .bind(tag.to_owned())
            .fetch_optional(self.registry.pool())
            .await
            .map_err(sql)?;
        match row {
            // A draft is not there for anyone who may not write.
            Some(r) if drafts || r.try_get::<i64, _>("published").map_err(sql)? != 0 => {
                self.release_row(entry, &r).await
            }
            _ => Err(Status::not_found("no such release")),
        }
    }

    /// The newest published release: (tag, target changeset).
    async fn latest_release(&self, entry: &RepoEntry) -> Result<Option<(String, Vec<u8>)>, Status> {
        let row = sqlx::query(
            "SELECT tag, target FROM releases WHERE ns = $1 AND published > 0 \
             ORDER BY published DESC, id DESC LIMIT 1",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .fetch_optional(self.registry.pool())
        .await
        .map_err(sql)?;
        row.map(|r| {
            Ok((
                r.try_get("tag").map_err(sql)?,
                r.try_get("target").map_err(sql)?,
            ))
        })
        .transpose()
    }

    /// The changesets of `exclude..include`, newest first, at most [`MAX_NOTES`]; whether more exist.
    async fn release_range(
        &self,
        entry: &RepoEntry,
        include: ChangesetId,
        exclude: Option<ChangesetId>,
    ) -> Result<(Vec<ChangesetId>, bool), Status> {
        let graph = crate::policy::graph_of(&self.registry, entry);
        let mut list: Vec<ChangesetId> = graph
            .log(LogQuery {
                include: vec![include],
                exclude: exclude.into_iter().collect(),
                limit: Some(MAX_NOTES + 1),
                ..LogQuery::default()
            })
            .try_collect()
            .await
            .map_err(|e| crate::update::internal("history", &e))?;
        let more = list.len() > MAX_NOTES;
        list.truncate(MAX_NOTES);
        Ok((list, more))
    }

    async fn resolve_rev(
        &self,
        ids: &IdCodec,
        entry: &RepoEntry,
        spec: &str,
        fallback: &str,
    ) -> Result<ChangesetId, Status> {
        if spec.len() > 1024 || spec.chars().any(char::is_control) {
            return Err(bad("invalid revision"));
        }
        let spec = if spec.is_empty() { fallback } else { spec };
        ids.resolve(spec, &entry.default_branch, |e| {
            crate::update::internal("history", &e)
        })
        .await?
        .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))
    }

    async fn notes_of(
        &self,
        ctx: &CallCtx,
        entry: &RepoEntry,
        list: &[ChangesetId],
    ) -> Result<Vec<wire::ReleaseNote>, Status> {
        let pool = self.registry.pool();
        let ns = entry.ns.as_bytes().to_vec();
        // The change of each landed changeset: the land recorded old -> new, the change index knows the old.
        let mut old_of: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        let mut change_of: HashMap<Vec<u8>, (Vec<u8>, String)> = HashMap::new();
        for part in list.chunks(200) {
            let marks = (0..part.len())
                .map(|i| format!("${}", i + 2))
                .collect::<Vec<_>>()
                .join(", ");
            let q = format!(
                "SELECT old_cs, new_cs FROM land_map_ref WHERE ns = $1 AND new_cs IN ({marks})"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(ns.clone());
            for c in part {
                query = query.bind(c.as_bytes().to_vec());
            }
            for r in query.fetch_all(pool).await.map_err(sql)? {
                old_of.insert(
                    r.try_get("new_cs").map_err(sql)?,
                    r.try_get("old_cs").map_err(sql)?,
                );
            }
            let marks2 = (0..part.len() * 2)
                .map(|i| format!("${}", i + 2))
                .collect::<Vec<_>>()
                .join(", ");
            // A change is found by the changeset of any version; the first version's actor is its author.
            let q = format!(
                "SELECT v.cs AS cs, f.change AS change, f.actor AS actor FROM change_versions v \
                 JOIN change_versions f ON f.ns = v.ns AND f.change = v.change AND f.v = 1 \
                 WHERE v.ns = $1 AND v.cs IN ({marks2})"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(ns.clone());
            for c in part {
                query = query.bind(c.as_bytes().to_vec());
            }
            for c in part {
                let key = old_of.get(c.as_bytes().as_slice()).cloned();
                query = query.bind(key.unwrap_or_else(|| c.as_bytes().to_vec()));
            }
            for r in query.fetch_all(pool).await.map_err(sql)? {
                change_of.insert(
                    r.try_get("cs").map_err(sql)?,
                    (
                        r.try_get("change").map_err(sql)?,
                        r.try_get("actor").map_err(sql)?,
                    ),
                );
            }
        }
        let runs = self.agent_runs_of(ctx).await?;
        let engine = entry.backend.engine();
        let mut out = Vec::new();
        for cs in list {
            let body = engine
                .get_changeset(*cs)
                .await
                .map_err(|e| crate::update::internal("changeset", &e))?;
            let message = String::from_utf8_lossy(&body.message).into_owned();
            let Some(category) = category(&message) else {
                continue;
            };
            let key = old_of.get(cs.as_bytes().as_slice()).cloned();
            let found = change_of
                .get(cs.as_bytes().as_slice())
                .or_else(|| key.as_ref().and_then(|k| change_of.get(k)));
            let (change, actor) = found.cloned().unwrap_or_default();
            let author = if found.is_some() {
                crate::stats::author_of(&runs, &change, &actor)
            } else {
                crate::stats::Author {
                    agent: false,
                    name: String::from_utf8_lossy(body.author.name()).into_owned(),
                    run_by: String::new(),
                    user_id: 0,
                }
            };
            out.push(wire::ReleaseNote {
                change: change.into(),
                changeset: bytes::Bytes::copy_from_slice(cs.as_bytes()),
                subject: crate::notify::one_line(
                    message.lines().next().unwrap_or_default(),
                    MAX_SUBJECT,
                ),
                author: author.name,
                by_agent: author.agent,
                category: category.to_owned(),
            });
        }
        Ok(out)
    }

    /// `Releases.DraftNotes`.
    pub async fn release_notes(
        &self,
        ctx: &CallCtx,
        req: wire::DraftReleaseNotesRequest,
    ) -> Result<wire::DraftReleaseNotesResponse, Status> {
        let entry = self.open(ctx).await?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let to = self
            .resolve_rev(&ids, &entry, &req.to, &entry.default_branch)
            .await?;
        let (from, from_tag) = if req.from.is_empty() {
            match self.latest_release(&entry).await? {
                Some((tag, target)) => (
                    <[u8; 32]>::try_from(target.as_slice())
                        .ok()
                        .map(ChangesetId::from_bytes),
                    tag,
                ),
                None => (None, String::new()),
            }
        } else {
            (
                Some(self.resolve_rev(&ids, &entry, &req.from, "").await?),
                req.from.clone(),
            )
        };
        let (list, truncated) = self.release_range(&entry, to, from).await?;
        let notes = self.notes_of(ctx, &entry, &list).await?;
        Ok(wire::DraftReleaseNotesResponse {
            markdown: markdown(&notes),
            total: u32::try_from(notes.len()).unwrap_or(u32::MAX),
            notes,
            from_tag,
            truncated,
        })
    }

    /// `Releases.Next`.
    pub async fn release_next(
        &self,
        ctx: &CallCtx,
        _req: wire::NextReleaseRequest,
    ) -> Result<wire::NextReleaseResponse, Status> {
        let entry = self.open(ctx).await?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let latest = self.latest_release(&entry).await?;
        let to = self
            .resolve_rev(&ids, &entry, "", &entry.default_branch)
            .await?;
        let from = latest
            .as_ref()
            .and_then(|(_, t)| <[u8; 32]>::try_from(t.as_slice()).ok())
            .map(ChangesetId::from_bytes);
        let (list, truncated) = self.release_range(&entry, to, from).await?;
        let draft: Option<String> = sqlx::query_scalar(
            "SELECT tag FROM releases WHERE ns = $1 AND published = 0 ORDER BY id DESC LIMIT 1",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .fetch_optional(self.registry.pool())
        .await
        .map_err(sql)?;
        let latest_tag = latest.map(|(t, _)| t).unwrap_or_default();
        Ok(wire::NextReleaseResponse {
            proposed_tag: next_tag(&latest_tag),
            latest_tag,
            changes_since: u32::try_from(list.len()).unwrap_or(u32::MAX),
            draft_tag: draft.unwrap_or_default(),
            truncated,
        })
    }

    /// `Releases.Create`.
    pub async fn release_create(
        &self,
        ctx: &CallCtx,
        req: wire::CreateReleaseRequest,
    ) -> Result<wire::CreateReleaseResponse, Status> {
        if !valid_tag(&req.tag) {
            return Err(bad(
                "a tag is 1 to 128 characters of letters, digits and ._+-/",
            ));
        }
        if req.title.len() > 200 || req.title.chars().any(char::is_control) {
            return Err(bad("a title is one line of at most 200 bytes"));
        }
        if req.notes.len() > MAX_NOTES_BYTES {
            return Err(bad("notes are at most 64 KiB"));
        }
        let channel = match wire::ReleaseChannel::try_from(req.channel) {
            Ok(wire::ReleaseChannel::Unspecified | wire::ReleaseChannel::Stable) => STABLE,
            Ok(wire::ReleaseChannel::PreRelease) => PRE,
            _ => {
                return Err(bad(
                    "channel is stable or pre-release; use draft for a draft",
                ));
            }
        };
        if req.assets.len() > MAX_ASSETS {
            return Err(bad("at most 100 assets"));
        }
        let mut names = std::collections::HashSet::new();
        for a in &req.assets {
            valid_asset(a)?;
            if !names.insert(a.name.clone()) {
                return Err(bad("an asset name appears twice"));
            }
        }
        if req.build_source.is_empty() != req.build_run.is_empty()
            || req.build_source.len() > 128
            || req.build_run.len() > 256
        {
            return Err(bad("a build run needs its source and its id"));
        }
        let entry = self.open(ctx).await?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let target_spec = if req.target.is_empty() {
            entry.default_branch.clone()
        } else {
            req.target.clone()
        };
        let target = self
            .resolve_rev(&ids, &entry, &target_spec, &entry.default_branch)
            .await?;
        let pool = self.registry.pool();
        let ns = entry.ns.as_bytes().to_vec();
        // Provenance: the jobs of the building run, and the signature on the release commit.
        let (mut build_url, mut passed, mut total) = (String::new(), 0i64, 0i64);
        if !req.build_run.is_empty() {
            let run = sqlx::query(
                "SELECT id, url FROM check_runs WHERE ns = $1 AND source = $2 AND external_id = $3",
            )
            .bind(ns.clone())
            .bind(req.build_source.clone())
            .bind(req.build_run.clone())
            .fetch_optional(pool)
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::not_found("no such build run reported"))?;
            build_url = run.try_get("url").map_err(sql)?;
            let id: i64 = run.try_get("id").map_err(sql)?;
            let jobs: Vec<i64> =
                sqlx::query_scalar("SELECT status FROM check_jobs WHERE ns = $1 AND run = $2")
                    .bind(ns.clone())
                    .bind(id)
                    .fetch_all(pool)
                    .await
                    .map_err(sql)?;
            let ok = wire::CiStatus::Success as i64;
            let skipped = wire::CiStatus::Skipped as i64;
            passed = i64::try_from(jobs.iter().filter(|s| **s == ok).count()).unwrap_or(0);
            total = i64::try_from(jobs.iter().filter(|s| **s != skipped).count()).unwrap_or(0);
        }
        let (mut signed_by, mut signing_key, mut signature_ok) =
            (String::new(), String::new(), 0i64);
        if let Some(r) = sqlx::query(
            "SELECT s.key_id AS key_id, s.status AS status, k.owner AS owner, k.fingerprint AS fingerprint \
             FROM change_signatures s LEFT JOIN signing_keys k ON k.id = s.key_id \
             WHERE s.repo_id = $1 AND s.changeset = $2",
        )
        .bind(entry.key.0)
        .bind(target.as_bytes().to_vec())
        .fetch_optional(pool)
        .await
        .map_err(sql)?
        {
            let key_id: String = r.try_get("key_id").map_err(sql)?;
            if key_id == crate::signing::SERVER_KEY_ID {
                signed_by = "server".into();
            } else {
                signed_by = r.try_get::<Option<String>, _>("owner").map_err(sql)?.unwrap_or_default();
                signing_key = r
                    .try_get::<Option<String>, _>("fingerprint")
                    .map_err(sql)?
                    .unwrap_or_default();
            }
            signature_ok = i64::from(
                r.try_get::<i64, _>("status").map_err(sql)? == wire::VerificationStatus::Valid as i64,
            );
        }
        let previous = self.latest_release(&entry).await?;
        let from = previous
            .as_ref()
            .and_then(|(_, t)| <[u8; 32]>::try_from(t.as_slice()).ok())
            .map(ChangesetId::from_bytes);
        let (list, _) = self.release_range(&entry, target, from).await?;
        let who = handle_of(&ctx.principal.id).unwrap_or_else(|| ctx.principal.id.clone());
        let at = now();
        let title = if req.title.is_empty() {
            req.tag.clone()
        } else {
            req.title.clone()
        };
        let mut tx = pool.begin().await.map_err(sql)?;
        self.registry.guard(&mut tx, &entry).await?;
        let taken: Option<i64> =
            sqlx::query_scalar("SELECT id FROM releases WHERE ns = $1 AND tag = $2")
                .bind(ns.clone())
                .bind(req.tag.clone())
                .fetch_optional(&mut *tx)
                .await
                .map_err(sql)?;
        if taken.is_some() {
            return Err(Status::already_exists("a release with this tag exists"));
        }
        let id = crate::review::next_id(&mut tx, "releases", &ns)
            .await
            .map_err(sql)?;
        sqlx::query(
            "INSERT INTO releases (ns, id, tag, title, notes, channel, target, previous_tag, changes, created, published, \
             created_by, created_by_id, build_source, build_run, build_url, built_from, checks_passed, checks_total, \
             signed_by, signing_key, signature_ok) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22)",
        )
        .bind(ns.clone())
        .bind(id)
        .bind(req.tag.clone())
        .bind(title)
        .bind(req.notes.clone())
        .bind(channel)
        .bind(target.as_bytes().to_vec())
        .bind(previous.map(|(t, _)| t).unwrap_or_default())
        .bind(i64::try_from(list.len()).unwrap_or(0))
        .bind(at)
        .bind(if req.draft { 0 } else { at })
        .bind(who)
        .bind(user_id_col(&ctx.principal.id))
        .bind(req.build_source.clone())
        .bind(req.build_run.clone())
        .bind(build_url)
        .bind(target_spec)
        .bind(passed)
        .bind(total)
        .bind(signed_by)
        .bind(signing_key)
        .bind(signature_ok)
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
        for a in &req.assets {
            sqlx::query(
                "INSERT INTO release_assets (ns, release_id, name, size, sha256, url, content_type) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(ns.clone())
            .bind(id)
            .bind(a.name.clone())
            .bind(i64::try_from(a.size).unwrap_or(i64::MAX))
            .bind(a.sha256.clone())
            .bind(a.url.clone())
            .bind(a.content_type.clone())
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        }
        tx.commit().await.map_err(sql)?;
        acting::record(
            pool,
            acting::event(ctx, "release.created", &req.tag)
                .detail("tag", req.tag.clone())
                .detail("draft", req.draft.to_string())
                .detail("assets", req.assets.len().to_string()),
        )
        .await;
        let release = self.release_by_tag(&entry, &req.tag, true).await?;
        Ok(wire::CreateReleaseResponse {
            release: Some(release),
        })
    }

    /// `Releases.Publish`.
    pub async fn release_publish(
        &self,
        ctx: &CallCtx,
        req: wire::PublishReleaseRequest,
    ) -> Result<wire::PublishReleaseResponse, Status> {
        let entry = self.open(ctx).await?;
        let current = self.release_by_tag(&entry, &req.tag, true).await?;
        if !current.draft {
            return Err(Status::failed_precondition(
                "the release is published already",
            ));
        }
        let done = sqlx::query(
            "UPDATE releases SET published = $3 WHERE ns = $1 AND tag = $2 AND published = 0",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(req.tag.clone())
        .bind(now().max(1))
        .execute(self.registry.pool())
        .await
        .map_err(sql)?;
        if done.rows_affected() != 1 {
            return Err(Status::failed_precondition(
                "the release is published already",
            ));
        }
        acting::record(
            self.registry.pool(),
            acting::event(ctx, "release.published", &req.tag).detail("tag", req.tag.clone()),
        )
        .await;
        Ok(wire::PublishReleaseResponse {
            release: Some(self.release_by_tag(&entry, &req.tag, true).await?),
        })
    }

    /// `Releases.Get`.
    pub async fn release_get(
        &self,
        ctx: &CallCtx,
        req: wire::GetReleaseRequest,
        drafts: bool,
    ) -> Result<wire::GetReleaseResponse, Status> {
        let entry = self.open(ctx).await?;
        Ok(wire::GetReleaseResponse {
            release: Some(self.release_by_tag(&entry, &req.tag, drafts).await?),
        })
    }

    /// `Releases.List`, newest first.
    pub async fn release_list(
        &self,
        ctx: &CallCtx,
        req: wire::ListReleasesRequest,
        drafts: bool,
    ) -> Result<wire::ListReleasesResponse, Status> {
        let limit = crate::review::page_limit(req.limit, DEFAULT_PAGE, MAX_PAGE);
        let scope = format!("Releases.List|{}", ctx.repo);
        let after = crate::review::parse_after(&scope, &ctx.principal.id, &req.page_token)?;
        let want =
            wire::ReleaseChannel::try_from(req.channel).map_err(|_| bad("unknown channel"))?;
        if want == wire::ReleaseChannel::Draft && !drafts {
            return Ok(wire::ListReleasesResponse::default());
        }
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let pool = self.registry.pool();
        let visible = if drafts { "" } else { " AND published > 0" };
        let count = |cond: &'static str| {
            let q = format!(
                "SELECT CAST(COUNT(*) AS BIGINT) FROM releases WHERE ns = $1{visible} AND {cond}"
            );
            let ns = ns.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q))
                    .bind(ns)
                    .fetch_one(pool)
                    .await
                    .map_err(sql)
            }
        };
        let (stable, pre, draft) = (
            count("published > 0 AND channel = 1").await?,
            count("published > 0 AND channel = 2").await?,
            if drafts {
                count("published = 0").await?
            } else {
                0
            },
        );
        let cond = match want {
            wire::ReleaseChannel::Stable => " AND published > 0 AND channel = 1",
            wire::ReleaseChannel::PreRelease => " AND published > 0 AND channel = 2",
            wire::ReleaseChannel::Draft => " AND published = 0",
            wire::ReleaseChannel::Unspecified => "",
        };
        let q = format!(
            "SELECT * FROM releases WHERE ns = $1{visible}{cond} AND id < $2 ORDER BY id DESC LIMIT $3"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(ns)
            .bind(if after == 0 { i64::MAX } else { after })
            .bind(i64::try_from(limit + 1).unwrap_or(i64::MAX))
            .fetch_all(pool)
            .await
            .map_err(sql)?;
        let more = rows.len() > limit;
        let mut releases = Vec::new();
        for r in rows.iter().take(limit) {
            releases.push(self.release_row(&entry, r).await?);
        }
        let next_page_token = match (more, releases.last()) {
            (true, Some(last)) => crate::review::after_token(&scope, &ctx.principal.id, last.id),
            _ => String::new(),
        };
        let n = |x: i64| u32::try_from(x).unwrap_or(0);
        Ok(wire::ListReleasesResponse {
            releases,
            next_page_token,
            stable: n(stable),
            pre_release: n(pre),
            draft: n(draft),
        })
    }
}

#[cfg(test)]
mod tests;
