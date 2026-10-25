//! The server write tools of `/mcp`: `commit`, `submit`, `rebase`, `restack` (SPEC 22.3.3; PLAN
//! M14).
//!
//! Every write runs the same handlers as the wire, so the SPEC 8.3 pipeline (protections, path
//! rules, hooks, quotas, push events) binds it exactly as it binds a push. This module adds the
//! MCP-only rules on top:
//!
//! * The deployment installed a [`WriteGate`] (`[mcp] write`), else `NOT_AVAILABLE`.
//! * The principal can read the repository (else the same `NOT_FOUND` as a missing one) and the
//!   authorizer lets it write (else `PERMISSION_DENIED`).
//! * `update_ref` of `commit` and `rebase` is empty or `refs/for/*`. No tool moves a branch
//!   (M-D8); `land` is the only one that moves a protected branch and it is not here.
//! * `dry_run` of `rebase` and `restack` is true unless the caller says `false`.
//! * Identity is the principal: `author`, `committer`, `sign`, `mode` and the committer fields
//!   are not arguments, and a call that carries one is refused. A `rebase` cannot name another
//!   repository as `head_repo`.
//! * The per-call credential is minted for `Access::Write` on this one repository.
//!
//! Confirmation (`submit` always, `commit` and `rebase` with `refs/for/*`) is the handler's, and
//! it also turns a bad `update_ref` into -32602 before any of this runs. The check here repeats
//! it, so a caller of the backend that skips the handler is refused too.

use asc_mcp::backend::{CallContext, Reply};
use asc_mcp::catalogue::{ToolDef, Toolset};
use asc_mcp::errors::{ToolError, code};
use asc_mcp::rpc_backend::DynCodec;
use asc_rpc::auth::Access;
use asc_rpc::json;
use asc_rpc::server::RepoName;
use futures::StreamExt as _;
use futures::stream;
use prost_reflect::{DynamicMessage, Kind, MessageDescriptor, ReflectMessage as _, Value as PbValue};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use tonic::Request;
use tonic::client::Grpc;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::{Ascii, MetadataValue};

use super::backend::{
    CALL_TIMEOUT, INFLIGHT_RETRY_MS, MAX_MESSAGE, REPO_NOT_FOUND, ServeBackend, clean, encode_msg, map_status, set_repo,
};
use crate::limits::{Caller, Class, class_of};

/// Fields that are never arguments of a write: identity is the principal, and the update mode is
/// the server's.
const FORBIDDEN: [&str; 7] =
    ["author", "committer", "sign", "committer_name", "committer_email", "committer_time", "mode"];
/// Most changes one `submit` carries.
const MAX_CHANGES: usize = 64;
/// Most reviewers one `submit` names, and the longest name.
const MAX_REVIEWERS: usize = 32;
const MAX_REVIEWER_LEN: usize = 128;

/// Whether `def` is one of the write tools this module serves.
#[must_use]
pub(super) fn is_write_tool(def: &ToolDef) -> bool {
    def.toolset == Toolset::Write && matches!(def.name, "commit" | "submit" | "rebase" | "restack")
}

/// The `update_ref` rule of `commit` and `rebase`: empty, or `refs/for/<branch>` with no empty or
/// dot component and no control character. Every other value, `refs/heads/*` and `master`
/// included, is refused.
///
/// # Errors
/// `INVALID_ARGUMENT` for any other value.
pub fn check_update_ref(update_ref: &str) -> Result<(), ToolError> {
    let ok = update_ref.is_empty()
        || update_ref.strip_prefix("refs/for/").is_some_and(|rest| {
            !update_ref.contains(|c: char| c.is_control() || c == '\\' || c == ' ')
                && rest.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
        });
    if ok {
        Ok(())
    } else {
        Err(ToolError::new("INVALID_ARGUMENT", "update_ref must be empty or refs/for/<branch>; branches are never written"))
    }
}

/// `dry_run` is true unless the caller sent `false`.
fn default_dry_run(obj: &mut Map<String, Value>) {
    if !matches!(obj.get("dry_run"), Some(Value::Bool(false))) {
        obj.insert("dry_run".into(), Value::Bool(true));
    }
}

/// Runs one server write tool.
pub(super) async fn call(b: &ServeBackend, def: &ToolDef, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
    let name = def.name;
    let not_served = || ToolError::not_available(format!("{name} is not served by this endpoint"));
    if !matches!(name, "commit" | "submit" | "rebase" | "restack") {
        return Err(not_served());
    }
    let Some(writes) = b.writes.as_ref() else { return Err(not_served()) };
    let Some(principal) = ctx.principal.as_deref() else {
        return Err(ToolError::new("UNAUTHENTICATED", "authentication is required"));
    };
    let mut obj = args.as_object().cloned().unwrap_or_default();
    let repo_arg = obj
        .get("repo")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new("INVALID_ARGUMENT", "repo (owner/name) is required"))?
        .to_owned();
    let repo = RepoName::parse(&repo_arg).map_err(|s| ToolError::new("INVALID_ARGUMENT", clean(s.message())))?;

    // Arguments first: a refused value costs the repository nothing and reveals nothing.
    if matches!(name, "commit" | "rebase") {
        match obj.get("update_ref") {
            None | Some(Value::Null) => {}
            Some(Value::String(r)) => check_update_ref(r)?,
            Some(_) => return Err(ToolError::new("INVALID_ARGUMENT", "update_ref must be a string")),
        }
    }
    if let Some(k) = FORBIDDEN.iter().find(|k| obj.contains_key(**k)) {
        return Err(ToolError::new("INVALID_ARGUMENT", format!("{k} is not an argument of {name}")));
    }
    if matches!(name, "rebase" | "restack") {
        default_dry_run(&mut obj);
    }

    let slot = b
        .inflight
        .acquire(principal)
        .ok_or_else(|| ToolError::rate_limited("too many calls in flight for this principal", INFLIGHT_RETRY_MS))?;
    if !b.gate.can_read(principal, repo.as_str()).await {
        return Err(ToolError::new(code::NOT_FOUND, REPO_NOT_FOUND));
    }
    if !writes.can_write(principal, repo.as_str()).await {
        return Err(ToolError::new("PERMISSION_DENIED", "write access is required"));
    }
    let grant = b.scope.mint(principal, repo.as_str(), Access::Write);
    let wire = Wire { b, token: grant.token.as_str(), cancel: &ctx.cancel };

    let out = if name == "submit" {
        let permit = b.admission.admit(Caller::principal(principal), Class::Expensive, 1).map_err(|s| map_status(&s))?;
        let v = submit(&wire, repo.as_str(), &obj).await?;
        drop(permit);
        v
    } else {
        let method = b.method(def.rpc).ok_or_else(not_served)?;
        let path = format!("/{}/{}", method.parent_service().full_name(), method.name());
        let class = class_of(&path).unwrap_or(Class::Expensive);
        let permit = b.admission.admit(Caller::principal(principal), class, 1).map_err(|s| map_status(&s))?;
        // `head_repo` may only name this repository; it is set below, never trusted.
        let head_repo = obj.remove("head_repo");
        if let Some(h) = &head_repo
            && h.get("name").and_then(Value::as_str).is_some_and(|n| n != repo.as_str())
        {
            return Err(ToolError::new("INVALID_ARGUMENT", "head_repo must be this repository"));
        }
        let mut msg = ServeBackend::build(def, &method, repo.as_str(), &Value::Object(obj), ctx)?;
        if head_repo.is_some() {
            set_named_repo(&mut msg, "head_repo", repo.as_str());
        }
        let resp = wire.unary(&path, msg, method.output()).await?;
        drop(permit);
        encode_msg(&resp)?
    };
    // The slot and the credential live until the work is done.
    drop(grant);
    drop(slot);
    Ok(Reply::Messages(stream::once(async move { Ok(out) }).boxed()))
}

fn set_named_repo(msg: &mut DynamicMessage, field: &str, name: &str) {
    let Some(f) = msg.descriptor().get_field_by_name(field) else { return };
    let Kind::Message(d) = f.kind() else { return };
    let mut tmp = DynamicMessage::new(d);
    tmp.set_field_by_name("name", PbValue::String(name.to_owned()));
    tmp.set_field_by_name("id_mode", PbValue::EnumNumber(asc_rpc::IdMode::Git.to_wire()));
    msg.set_field_by_name(field, PbValue::Message(tmp));
}

/// One authenticated in-process unary call on the write credential.
struct Wire<'a> {
    b: &'a ServeBackend,
    token: &'a str,
    cancel: &'a CancellationToken,
}

impl Wire<'_> {
    async fn unary(&self, path: &str, msg: DynamicMessage, out: MessageDescriptor) -> Result<DynamicMessage, ToolError> {
        let mut req = Request::new(msg);
        req.set_timeout(CALL_TIMEOUT);
        let meta = req.metadata_mut();
        if let Ok(mut v) = format!("Bearer {}", self.token).parse::<MetadataValue<Ascii>>() {
            v.set_sensitive(true);
            meta.insert("authorization", v);
        }
        if let Ok(v) = format!("asc/{} mcp", env!("CARGO_PKG_VERSION")).parse::<MetadataValue<Ascii>>() {
            meta.insert("x-asc-client", v);
        }
        // A mutating call needs an op id. A fresh one per tool call: a model's retry is a new call
        // and must not replay a cached answer.
        if let Ok(v) = uuid::Uuid::new_v4().to_string().parse::<MetadataValue<Ascii>>() {
            meta.insert("x-asc-op-id", v);
        }
        let uri: PathAndQuery = path.parse().map_err(|_| ToolError::internal("bad method path"))?;
        let mut grpc = Grpc::new(self.b.routes.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE);
        let run = async {
            grpc.ready().await.map_err(|e| ToolError::not_available(clean(&e.to_string())))?;
            let r = grpc.unary(req, uri, DynCodec::new(out)).await.map_err(|s| map_status(&s))?;
            Ok::<_, ToolError>(r.into_inner())
        };
        tokio::select! {
            r = run => r,
            () = self.cancel.cancelled() => Err(ToolError::internal("the call was cancelled")),
        }
    }

    /// A call by service and method name with a JSON request; `repo` is set from the gate.
    async fn named(&self, svc: &str, method: &str, req: &Value, repo: &str) -> Result<Value, ToolError> {
        let m = self
            .b
            .pool
            .get_service_by_name(&format!("asc.v1.{svc}"))
            .and_then(|s| s.methods().find(|m| m.name() == method))
            .ok_or_else(|| ToolError::not_available(format!("{svc}.{method} is not served by this endpoint")))?;
        let mut msg =
            json::decode(&m.input(), req).map_err(|e| ToolError::new("INVALID_ARGUMENT", format!("{method}: {e}")))?;
        set_repo(&mut msg, repo);
        let path = format!("/{}/{}", m.parent_service().full_name(), m.name());
        let resp = self.unary(&path, msg, m.output()).await?;
        encode_msg(&resp)
    }
}

fn strings(args: &Map<String, Value>, key: &str, required: bool) -> Result<Vec<String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) if !required => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().map(str::to_owned).ok_or_else(|| ToolError::new("INVALID_ARGUMENT", format!("{key} must be strings"))))
            .collect(),
        _ => Err(ToolError::new("INVALID_ARGUMENT", format!("{key} must be an array of strings"))),
    }
}

/// `submit`: one `UpdateRefs` of `refs/for/<trunk>/asc-<change>` per change (SPEC 20.5 flow 1),
/// with the push options `asc.draft` and `asc.reviewers`. The ref names and heads come from the
/// server's own answer for the changes, never from the arguments; a change the caller cannot see
/// is not found.
async fn submit(wire: &Wire<'_>, repo: &str, args: &Map<String, Value>) -> Result<Value, ToolError> {
    let bad = |m: &str| ToolError::new("INVALID_ARGUMENT", m);
    let changes = strings(args, "changes", true)?;
    if changes.is_empty() || changes.len() > MAX_CHANGES {
        return Err(bad("changes must hold between 1 and 64 change ids"));
    }
    let draft = args.get("draft").and_then(Value::as_bool).unwrap_or(true);
    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
    let reviewers = strings(args, "reviewers", false)?;
    if reviewers.len() > MAX_REVIEWERS
        || reviewers.iter().any(|r| {
            r.is_empty()
                || r.len() > MAX_REVIEWER_LEN
                || r.contains(|c: char| c.is_control() || c == ',' || c.is_whitespace())
        })
    {
        return Err(bad("reviewers must be up to 32 plain user names"));
    }

    let resolved = wire.named("Changes", "ResolveChanges", &json!({ "changes": changes }), repo).await?;
    let infos = resolved.get("changes").and_then(Value::as_array).cloned().unwrap_or_default();
    if infos.len() != changes.len() {
        return Err(ToolError::new(code::NOT_FOUND, "a change was not found"));
    }
    let mut heads = Vec::with_capacity(infos.len());
    for i in &infos {
        if i.get("visible").and_then(Value::as_array).is_some_and(|v| v.len() > 1) {
            return Err(ToolError::new("DIVERGENT", "a change is divergent; resolve it before submitting"));
        }
        match (i.get("change").and_then(Value::as_str), i.get("head").and_then(Value::as_str)) {
            (Some(c), Some(h)) if !c.is_empty() && !h.is_empty() => heads.push((c.to_owned(), h.to_owned())),
            _ => return Err(ToolError::new(code::NOT_FOUND, "a change was not found")),
        }
    }
    let stack = wire.named("Changes", "Stack", &json!({ "change": changes[0] }), repo).await?;
    let trunk = stack.get("trunk").and_then(Value::as_str).unwrap_or_default();
    if trunk.contains(|c: char| c.is_control() || c == ' ' || c == '\\') || trunk.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return Err(ToolError::new("FAILED_PRECONDITION", "the repository has no trunk to submit against"));
    }
    let updates: Vec<Value> = heads
        .iter()
        .map(|(c, h)| json!({ "name": format!("refs/for/{trunk}/asc-{c}"), "any": true, "new": { "changeset": h } }))
        .collect();
    let mut options = Vec::new();
    if draft {
        options.push("asc.draft".to_owned());
    }
    if !reviewers.is_empty() {
        options.push(format!("asc.reviewers={}", reviewers.join(",")));
    }
    let mode = if force { "UPDATE_MODE_FORCE" } else { "UPDATE_MODE_FAST_FORWARD" };
    wire.named("Repo", "UpdateRefs", &json!({ "updates": updates, "mode": mode, "push_options": options }), repo).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_ref_is_empty_or_under_refs_for() {
        for ok in ["", "refs/for/master", "refs/for/master/topic"] {
            assert!(check_update_ref(ok).is_ok(), "{ok}");
        }
        for bad in [
            "master",
            "refs/heads/master",
            "refs/heads/x",
            "refs/for/",
            "refs/for",
            "refs/for/../heads/master",
            "refs/for//x",
            "refs/for/./x",
            "refs/for/x\n",
            "refs/for/x y",
            "refs/for\\x",
            "REFS/for/x",
            "refs/tags/v1",
            " refs/for/x",
        ] {
            assert!(check_update_ref(bad).is_err(), "{bad:?}");
        }
    }
}
