//! Gate history, statistics and maintainer overrides (`review.proto`, "gate statistics and
//! overrides").
//!
//! The host reports each evaluation of a version (`RecordGateDecision`); asc keeps one row per
//! `(change, version)` with the first result, the latest result and when a held version cleared.
//! The statistics (checked, passed first time, held/cleared/open, median time to clear, a count per
//! day and per rule) are computed from those rows over a window. An override is an exception a
//! maintainer grants for one rule of one change, with a reason; it is audited with the person and
//! the host relay and chained (`proof` = SHA-256 over the previous proof and the row).

use std::collections::BTreeMap;

use asc_rpc::server::{CallCtx, wire};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use sqlx::Row;
use tonic::Status;

use super::{
    Reviews, WriteError, after_token, change_of, next_id, now, page_limit, parse_after,
    retry_conflict, sql, store,
};

/// Evaluations a statistics call reads, and rules one decision holds.
pub const MAX_ROWS: i64 = 20_000;
pub const MAX_RULES: usize = 64;
pub const MAX_RULE: usize = 120;
pub const MAX_REASON: usize = 500;
const DAY: i64 = 86_400;

fn u32_of(n: i64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `YYYY-MM-DD` of a Unix time.
fn day_of(secs: i64) -> String {
    crate::agents::control::day_of(secs)
}

fn proof_of(
    prev: &[u8],
    id: i64,
    change: &[u8],
    rule: &str,
    reason: &str,
    by: &str,
    at: i64,
) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(b"asc-override-v1\n");
    h.update(prev);
    for part in [
        id.to_string().as_bytes(),
        change,
        rule.as_bytes(),
        reason.as_bytes(),
        by.as_bytes(),
        at.to_string().as_bytes(),
    ] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part);
    }
    h.finalize().to_vec()
}

fn override_of(r: &sqlx::any::AnyRow) -> Result<wire::GateOverride, Status> {
    Ok(wire::GateOverride {
        id: u64::try_from(r.try_get::<i64, _>("id").map_err(sql)?).unwrap_or(0),
        change: Bytes::from(r.try_get::<Vec<u8>, _>("change").map_err(sql)?),
        rule: r.try_get("rule").map_err(sql)?,
        reason: r.try_get("reason").map_err(sql)?,
        granted_by: r.try_get("granted_by").map_err(sql)?,
        granted_at: r.try_get("granted_at").map_err(sql)?,
        revoked_by: r.try_get("revoked_by").map_err(sql)?,
        revoked_at: r.try_get("revoked_at").map_err(sql)?,
        proof: Bytes::from(r.try_get::<Vec<u8>, _>("proof").map_err(sql)?),
        prev: Bytes::from(r.try_get::<Vec<u8>, _>("prev").map_err(sql)?),
    })
}

const OVERRIDE_COLS: &str =
    "id, change, rule, reason, granted_by, granted_at, revoked_by, revoked_at, prev, proof";

impl Reviews {
    /// `Review.RecordGateDecision`.
    pub async fn record_gate_decision(
        &self,
        ctx: &CallCtx,
        req: wire::RecordGateDecisionRequest,
    ) -> Result<wire::RecordGateDecisionResponse, Status> {
        let change = change_of(&req.change)?;
        if req.rules.len() > MAX_RULES
            || req
                .rules
                .iter()
                .any(|r| r.rule.is_empty() || r.rule.len() > MAX_RULE)
        {
            return Err(Status::invalid_argument(
                "at most 64 rules of 1 to 120 bytes",
            ));
        }
        let entry = self.open(ctx).await?;
        let ns = entry.ns;
        let rules: Vec<(String, bool)> =
            req.rules.iter().map(|r| (r.rule.clone(), r.held)).collect();
        let passed = !rules.iter().any(|r| r.1);
        let json = serde_json::to_string(&rules).unwrap_or_else(|_| "[]".to_owned());
        let at = now();
        let mut tx = self.registry.pool().begin().await.map_err(sql)?;
        self.registry.guard(&mut tx, &entry).await?;
        if !store::change_known(&mut tx, ns, change).await? {
            return Err(Status::not_found("unknown change"));
        }
        let latest = store::latest_version(&mut tx, ns, change).await?;
        let version = if req.version == 0 {
            latest
        } else {
            req.version
        };
        if version == 0 || version > latest {
            return Err(Status::invalid_argument("unknown version"));
        }
        let row = sqlx::query("SELECT first_passed, passed, cleared_at FROM gate_decisions WHERE ns = $1 AND change = $2 AND version = $3")
            .bind(ns.as_bytes().to_vec())
            .bind(change.as_bytes().to_vec())
            .bind(i64::from(version))
            .fetch_optional(&mut *tx)
            .await
            .map_err(sql)?;
        let (first_time, cleared) = match row {
            None => {
                sqlx::query("INSERT INTO gate_decisions (ns, change, version, first_at, at, first_passed, passed, cleared_at, rules_json) VALUES ($1, $2, $3, $4, $4, $5, $5, 0, $6)")
                    .bind(ns.as_bytes().to_vec())
                    .bind(change.as_bytes().to_vec())
                    .bind(i64::from(version))
                    .bind(at)
                    .bind(i64::from(passed))
                    .bind(json)
                    .execute(&mut *tx)
                    .await
                    .map_err(sql)?;
                (passed, false)
            }
            Some(r) => {
                let (first, was): (i64, i64) = (
                    r.try_get("first_passed").map_err(sql)?,
                    r.try_get("passed").map_err(sql)?,
                );
                let cleared_at: i64 = r.try_get("cleared_at").map_err(sql)?;
                let clears = was == 0 && passed;
                let new_cleared = if clears {
                    at
                } else if passed {
                    cleared_at
                } else {
                    0
                };
                sqlx::query("UPDATE gate_decisions SET at = $4, passed = $5, cleared_at = $6, rules_json = $7 WHERE ns = $1 AND change = $2 AND version = $3")
                    .bind(ns.as_bytes().to_vec())
                    .bind(change.as_bytes().to_vec())
                    .bind(i64::from(version))
                    .bind(at)
                    .bind(i64::from(passed))
                    .bind(new_cleared)
                    .bind(json)
                    .execute(&mut *tx)
                    .await
                    .map_err(sql)?;
                (first != 0, clears)
            }
        };
        tx.commit().await.map_err(sql)?;
        Ok(wire::RecordGateDecisionResponse {
            passed,
            first_time,
            cleared,
        })
    }

    /// `Review.GetGateStats`.
    pub async fn get_gate_stats(
        &self,
        ctx: &CallCtx,
        req: wire::GetGateStatsRequest,
    ) -> Result<wire::GetGateStatsResponse, Status> {
        let days = match req.days {
            0 => 30,
            n => i64::from(n.min(365)),
        };
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let end = now();
        let since = end - days * DAY;
        let rows = sqlx::query(
            "SELECT first_at, first_passed, passed, cleared_at, rules_json FROM gate_decisions \
             WHERE ns = $1 AND first_at >= $2 ORDER BY first_at DESC LIMIT $3",
        )
        .bind(ns.clone())
        .bind(since)
        .bind(MAX_ROWS + 1)
        .fetch_all(self.registry.pool())
        .await
        .map_err(sql)?;
        let truncated = rows.len() as i64 > MAX_ROWS;
        let mut out = wire::GetGateStatsResponse {
            truncated,
            ..Default::default()
        };
        let mut per_day: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        for i in 0..=days {
            per_day.insert(day_of(end - (days - i) * DAY), (0, 0));
        }
        let mut per_rule: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        let mut clear_times: Vec<i64> = Vec::new();
        for r in rows.iter().take(usize::try_from(MAX_ROWS).unwrap_or(0)) {
            let (first_at, first_passed, passed, cleared_at): (i64, i64, i64, i64) = (
                r.try_get("first_at").map_err(sql)?,
                r.try_get("first_passed").map_err(sql)?,
                r.try_get("passed").map_err(sql)?,
                r.try_get("cleared_at").map_err(sql)?,
            );
            out.checked += 1;
            let ever_held = first_passed == 0;
            let day = per_day.entry(day_of(first_at)).or_default();
            day.0 += 1;
            if ever_held {
                day.1 += 1;
                out.held += 1;
                if passed == 0 {
                    out.held_open += 1;
                } else {
                    out.held_cleared += 1;
                    clear_times.push((cleared_at - first_at).max(0));
                }
            } else {
                out.passed_first_time += 1;
            }
            let rules: Vec<(String, bool)> =
                serde_json::from_str(&r.try_get::<String, _>("rules_json").map_err(sql)?)
                    .unwrap_or_default();
            for (rule, held) in rules {
                let c = per_rule.entry(rule).or_default();
                c.0 += 1;
                if held {
                    c.1 += 1;
                }
            }
        }
        clear_times.sort_unstable();
        out.median_clear_seconds = clear_times
            .get(clear_times.len() / 2)
            .map_or(0, |s| u32_of(*s));
        out.per_day = per_day
            .into_iter()
            .map(|(day, (checked, held))| wire::GateDay { day, checked, held })
            .collect();
        let mut rules: Vec<(String, (u32, u32))> = per_rule.into_iter().collect();
        rules.sort_by(|a, b| {
            b.1.1
                .cmp(&a.1.1)
                .then_with(|| b.1.0.cmp(&a.1.0))
                .then_with(|| a.0.cmp(&b.0))
        });
        out.per_rule = rules
            .into_iter()
            .map(|(rule, (checked, held))| wire::GateRuleCount {
                rule,
                checked,
                held,
            })
            .collect();
        let active: i64 = sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM gate_overrides WHERE ns = $1 AND revoked_at = 0",
        )
        .bind(ns)
        .fetch_one(self.registry.pool())
        .await
        .map_err(sql)?;
        out.overrides_active = u32_of(active);
        Ok(out)
    }

    /// `Review.GrantOverride`. `by` is the verified maintainer.
    pub async fn grant_override(
        &self,
        ctx: &CallCtx,
        by: &str,
        req: wire::GrantOverrideRequest,
    ) -> Result<wire::GrantOverrideResponse, Status> {
        let change = change_of(&req.change)?;
        let (rule, reason) = (req.rule.trim().to_owned(), req.reason.trim().to_owned());
        if rule.is_empty()
            || rule.len() > MAX_RULE
            || reason.is_empty()
            || reason.len() > MAX_REASON
            || reason.contains('\0')
            || rule.contains('\0')
        {
            return Err(Status::invalid_argument(
                "an override needs a rule (up to 120 bytes) and a reason (up to 500 bytes)",
            ));
        }
        let entry = self.open(ctx).await?;
        let ns = entry.ns;
        let id = retry_conflict(|| async {
            let mut tx = self.registry.pool().begin().await?;
            self.registry.guard(&mut tx, &entry).await?;
            if !store::change_known(&mut tx, ns, change).await? {
                return Err(WriteError::Status(Status::not_found("unknown change")));
            }
            let prev: Option<Vec<u8>> = sqlx::query_scalar("SELECT proof FROM gate_overrides WHERE ns = $1 ORDER BY id DESC LIMIT 1")
                .bind(ns.as_bytes().to_vec())
                .fetch_optional(&mut *tx)
                .await?;
            let prev = prev.unwrap_or_else(|| vec![0u8; 32]);
            let id = next_id(&mut tx, "gate_overrides", ns.as_bytes()).await?;
            let at = now();
            let proof = proof_of(&prev, id, change.as_bytes(), &rule, &reason, by, at);
            sqlx::query("INSERT INTO gate_overrides (ns, id, change, rule, reason, granted_by, granted_at, prev, proof) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
                .bind(ns.as_bytes().to_vec())
                .bind(id)
                .bind(change.as_bytes().to_vec())
                .bind(rule.clone())
                .bind(reason.clone())
                .bind(by.to_owned())
                .bind(at)
                .bind(prev)
                .bind(proof)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(id)
        })
        .await?;
        super::acting::record(
            self.registry.pool(),
            super::acting::event(ctx, "review.override.granted", &id.to_string())
                .detail("rule", rule)
                .detail("reason", reason),
        )
        .await;
        Ok(wire::GrantOverrideResponse {
            r#override: Some(self.override_row(&entry, id).await?),
        })
    }

    async fn override_row(
        &self,
        entry: &crate::registry::RepoEntry,
        id: i64,
    ) -> Result<wire::GateOverride, Status> {
        let q = format!("SELECT {OVERRIDE_COLS} FROM gate_overrides WHERE ns = $1 AND id = $2");
        let r = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(entry.ns.as_bytes().to_vec())
            .bind(id)
            .fetch_optional(self.registry.pool())
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::not_found("no such override"))?;
        override_of(&r)
    }

    /// `Review.RevokeOverride`.
    pub async fn revoke_override(
        &self,
        ctx: &CallCtx,
        by: &str,
        req: wire::RevokeOverrideRequest,
    ) -> Result<wire::RevokeOverrideResponse, Status> {
        let id = i64::try_from(req.id)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Status::invalid_argument("invalid id"))?;
        let entry = self.open(ctx).await?;
        self.override_row(&entry, id).await?;
        sqlx::query("UPDATE gate_overrides SET revoked_by = $3, revoked_at = $4 WHERE ns = $1 AND id = $2 AND revoked_at = 0")
            .bind(entry.ns.as_bytes().to_vec())
            .bind(id)
            .bind(by.to_owned())
            .bind(now())
            .execute(self.registry.pool())
            .await
            .map_err(sql)?;
        super::acting::record(
            self.registry.pool(),
            super::acting::event(ctx, "review.override.revoked", &id.to_string()),
        )
        .await;
        Ok(wire::RevokeOverrideResponse {
            r#override: Some(self.override_row(&entry, id).await?),
        })
    }

    /// `Review.ListOverrides`.
    pub async fn list_overrides(
        &self,
        ctx: &CallCtx,
        req: wire::ListOverridesRequest,
    ) -> Result<wire::ListOverridesResponse, Status> {
        let entry = self.open(ctx).await?;
        let limit = page_limit(req.limit, 50, 200);
        let scope = format!("Review.ListOverrides|{}", ctx.repo);
        let before = parse_after(&scope, &ctx.principal.id, &req.page_token)?;
        let mut sql_text = format!("SELECT {OVERRIDE_COLS} FROM gate_overrides WHERE ns = $1");
        let mut n = 1;
        if before > 0 {
            n += 1;
            sql_text.push_str(&format!(" AND id < ${n}"));
        }
        if !req.change.is_empty() {
            n += 1;
            sql_text.push_str(&format!(" AND change = ${n}"));
        }
        if req.active_only {
            sql_text.push_str(" AND revoked_at = 0");
        }
        sql_text.push_str(&format!(" ORDER BY id DESC LIMIT {}", limit + 1));
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql_text)).bind(entry.ns.as_bytes().to_vec());
        if before > 0 {
            q = q.bind(before);
        }
        if !req.change.is_empty() {
            q = q.bind(change_of(&req.change)?.as_bytes().to_vec());
        }
        let rows = q.fetch_all(self.registry.pool()).await.map_err(sql)?;
        let mut overrides = rows
            .iter()
            .map(override_of)
            .collect::<Result<Vec<_>, _>>()?;
        let next = if overrides.len() > limit {
            overrides.truncate(limit);
            overrides
                .last()
                .map(|o| after_token(&scope, &ctx.principal.id, o.id))
                .unwrap_or_default()
        } else {
            String::new()
        };
        Ok(wire::ListOverridesResponse {
            overrides,
            next_page_token: next,
        })
    }
}
