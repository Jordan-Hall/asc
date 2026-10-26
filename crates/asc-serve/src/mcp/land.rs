//! The server `land` and `classify` tools of `/mcp` (SPEC 22.3.2 and 22.3.3; PLAN M32).
//!
//! [`LandBackend`] wraps a [`ServeBackend`] and adds two things:
//!
//! * `classify` is the generic read path (`Repo.Classify`); this wrapper only bounds its answer
//!   to 300 files and to the token budget of the call, and says how many files were left out.
//! * `land` calls `UpdateRefs` with `UPDATE_MODE_LAND` and nothing else, so branch protections,
//!   required approvals, relaxed approvals, path rules, hooks and quotas bind it exactly as they
//!   bind `asc push --land`. A refusal comes back as the pipeline's own code. The tool adds only
//!   rules that tighten:
//!   * the deployment installed a [`WriteGate`] (`[mcp] write`), else `NOT_AVAILABLE`;
//!   * the principal can read the repository (else the `NOT_FOUND` of a missing one) and the
//!     authorizer lets it write (else `PERMISSION_DENIED`);
//!   * `dry_run` is true unless the caller sends the boolean `false`, and a dry run writes
//!     nothing (it asks `Repo.Merge` with `dry_run` for conflicts);
//!   * the branch must be one the repository accepts land requests on ([`LandPolicy`]), checked
//!     before any ref is read, so a refusal names no object;
//!   * identity, hooks and the update mode are not arguments;
//!   * the per-call credential is minted for `Access::Write` on this repository only.
//!
//! Confirmation is always required: the catalogue row says `confirm = "always"`, and the handler
//! asks before this code runs.

use std::sync::Arc;

use async_trait::async_trait;
use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::errors::{ToolError, code};
use asc_mcp::rpc_backend::DynCodec;
use asc_rpc::auth::Access;
use asc_rpc::server::{RepoName, status_rpc_error};
use futures::StreamExt as _;
use futures::stream;
use prost_reflect::{DynamicMessage, MessageDescriptor, ReflectMessage as _, Value as PbValue};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use tonic::Request;
use tonic::client::Grpc;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::{Ascii, MetadataValue};

use super::backend::{
    CALL_TIMEOUT, INFLIGHT_RETRY_MS, MAX_MESSAGE, REPO_NOT_FOUND, ServeBackend, clean, encode_msg,
    map_status, set_repo,
};
use crate::limits::{Caller, Class, class_of};

/// Files `classify` lists, at most.
pub const MAX_CLASSIFY_FILES: usize = 300;
/// Most changes one `land` names.
pub const MAX_CHANGES: usize = 100;
/// Longest revision or branch argument.
const MAX_ARG: usize = 256;
/// `UPDATE_MODE_LAND` on the wire.
const UPDATE_MODE_LAND: i32 = 2;

/// Whether a repository accepts land requests on a branch (`repos.land_branches`, SPEC 8.5).
#[async_trait]
pub trait LandPolicy: Send + Sync + 'static {
    /// `true` only when `refname` (a full `refs/heads/<name>`) accepts LAND on `repo`. A missing
    /// repository answers `false`.
    async fn accepts_land(&self, repo: &str, refname: &str) -> bool;
}

/// The policy of a running server: the repository registry and the same check the pipeline runs.
pub struct RegistryLandPolicy(pub Arc<crate::registry::Registry>);

#[async_trait]
impl LandPolicy for RegistryLandPolicy {
    async fn accepts_land(&self, repo: &str, refname: &str) -> bool {
        let Ok(name) = RepoName::parse(repo) else {
            return false;
        };
        let Ok(entry) = self.0.open(&name).await else {
            return false;
        };
        crate::land::check_branch(self.0.pool(), &entry, refname)
            .await
            .is_ok()
    }
}

/// A [`ServeBackend`] that also serves `land` and bounds `classify`.
pub struct LandBackend {
    inner: ServeBackend,
    policy: Arc<dyn LandPolicy>,
}

impl std::fmt::Debug for LandBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LandBackend")
    }
}

impl LandBackend {
    /// `land` runs only when `inner` has a write gate ([`ServeBackend::with_writes`]).
    #[must_use]
    pub fn new(inner: ServeBackend, policy: Arc<dyn LandPolicy>) -> Self {
        Self { inner, policy }
    }
}

#[async_trait]
impl McpBackend for LandBackend {
    async fn call(&self, tool: &str, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        match tool {
            "land" => self.land(args, ctx).await,
            "classify" => match self.inner.call(tool, args, ctx).await? {
                Reply::Messages(s) => {
                    let budget = ctx.budget_bytes;
                    Ok(Reply::Messages(
                        s.map(move |r| r.map(|v| cap_classify(v, budget))).boxed(),
                    ))
                }
                other => Ok(other),
            },
            _ => self.inner.call(tool, args, ctx).await,
        }
    }

    async fn worktrees(&self) -> Result<Vec<std::path::PathBuf>, ToolError> {
        self.inner.worktrees().await
    }
}

/// Bounds a `ClassifyResponse` to [`MAX_CLASSIFY_FILES`] files and to `budget_bytes` of JSON, and
/// records the files left out in `omitted_files`.
#[must_use]
pub fn cap_classify(mut v: Value, budget_bytes: u64) -> Value {
    let Some(files) = v.get_mut("files").and_then(Value::as_array_mut) else {
        return v;
    };
    let total = files.len();
    let mut used = 0u64;
    let mut keep = 0usize;
    for f in files.iter().take(MAX_CLASSIFY_FILES) {
        used = used.saturating_add(f.to_string().len() as u64 + 1);
        if used > budget_bytes {
            break;
        }
        keep += 1;
    }
    files.truncate(keep);
    if let Some(o) = v.as_object_mut() {
        o.insert("omitted_files".into(), json!(total - keep));
    }
    v
}

/// What `land` was asked to land, after validation.
#[derive(Debug, PartialEq, Eq)]
pub struct LandArgs {
    /// Revisions to resolve; the last is the tip.
    pub revs: Vec<String>,
    /// Full `refs/heads/<name>`.
    pub branch: String,
    /// A preview only.
    pub dry_run: bool,
}

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new("INVALID_ARGUMENT", msg)
}

fn plain(s: &str, what: &str) -> Result<(), ToolError> {
    if s.is_empty()
        || s.len() > MAX_ARG
        || s.starts_with('-')
        || s.contains(|c: char| c.is_control())
    {
        return Err(bad(format!(
            "{what} is empty, too long or has control characters"
        )));
    }
    Ok(())
}

/// A branch name as a full ref. Only `refs/heads/*` is ever a land target.
fn branch_ref(branch: &str) -> Result<String, ToolError> {
    plain(branch, "branch")?;
    let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    if short.starts_with("refs/")
        || short.is_empty()
        || short.starts_with('/')
        || short.ends_with('/')
        || short.ends_with(".lock")
        || short.contains("..")
        || short.contains("//")
        || short.contains("@{")
        || short.contains([' ', '~', '^', ':', '?', '*', '[', '\\'])
        || short.split('/').any(|seg| seg.starts_with('.'))
    {
        return Err(bad("branch is not a branch name"));
    }
    Ok(format!("refs/heads/{short}"))
}

/// Validates the arguments of `land`. Exactly one of `changes` and `tip`; `dry_run` is true unless
/// it is the boolean `false`.
///
/// # Errors
/// `INVALID_ARGUMENT`.
pub fn parse_args(obj: &Map<String, Value>) -> Result<LandArgs, ToolError> {
    for k in obj.keys() {
        if !matches!(
            k.as_str(),
            "repo" | "tip" | "changes" | "branch" | "dry_run" | "max_tokens"
        ) {
            return Err(bad(format!("{k} is not an argument of land")));
        }
    }
    let revs = match (obj.get("tip"), obj.get("changes")) {
        (Some(Value::String(t)), None | Some(Value::Null)) => vec![t.clone()],
        (None | Some(Value::Null), Some(Value::Array(a))) => {
            if a.is_empty() || a.len() > MAX_CHANGES {
                return Err(bad(format!("changes takes 1 to {MAX_CHANGES} change ids")));
            }
            a.iter()
                .map(|c| {
                    c.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| bad("changes must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        _ => return Err(bad("land needs exactly one of changes and tip")),
    };
    for r in &revs {
        plain(r, "a revision")?;
    }
    let branch = match obj.get("branch") {
        Some(Value::String(b)) => branch_ref(b)?,
        _ => return Err(bad("branch is required")),
    };
    let dry_run = match obj.get("dry_run") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad("dry_run must be a boolean")),
    };
    Ok(LandArgs {
        revs,
        branch,
        dry_run,
    })
}

fn path_text(p: &[u8]) -> String {
    clean(&String::from_utf8_lossy(p))
}

/// `map_status`, plus the conflict records of a `LAND_CONFLICT` as data.
fn map_land_status(s: &tonic::Status) -> ToolError {
    let mut t = map_status(s);
    if let Some(e) = status_rpc_error(s) {
        t.conflicts = e
            .info()
            .conflicts
            .iter()
            .map(|c| clean(&c.path.to_string()))
            .collect();
    }
    t
}

struct Wire<'a> {
    b: &'a ServeBackend,
    token: &'a str,
    cancel: &'a CancellationToken,
    principal: &'a str,
}

impl Wire<'_> {
    /// One in-process unary call of `Repo.<method>` on the write credential, charged to its class.
    async fn repo(
        &self,
        method: &str,
        build: impl FnOnce(&mut DynamicMessage),
        repo: &str,
    ) -> Result<DynamicMessage, ToolError> {
        let m = self
            .b
            .pool
            .get_service_by_name("asc.v1.Repo")
            .and_then(|s| s.methods().find(|m| m.name() == method))
            .ok_or_else(|| {
                ToolError::not_available(format!("Repo.{method} is not served by this endpoint"))
            })?;
        let path = format!("/{}/{}", m.parent_service().full_name(), m.name());
        let class = class_of(&path).unwrap_or(Class::Expensive);
        let _permit = self
            .b
            .admission
            .admit(Caller::principal(self.principal), class, 1)
            .map_err(|s| map_status(&s))?;
        let mut msg = DynamicMessage::new(m.input());
        build(&mut msg);
        set_repo(&mut msg, repo);
        self.unary(&path, msg, m.output()).await
    }

    async fn unary(
        &self,
        path: &str,
        msg: DynamicMessage,
        out: MessageDescriptor,
    ) -> Result<DynamicMessage, ToolError> {
        let mut req = Request::new(msg);
        req.set_timeout(CALL_TIMEOUT);
        let meta = req.metadata_mut();
        if let Ok(mut v) = format!("Bearer {}", self.token).parse::<MetadataValue<Ascii>>() {
            v.set_sensitive(true);
            meta.insert("authorization", v);
        }
        if let Ok(v) =
            format!("asc/{} mcp", env!("CARGO_PKG_VERSION")).parse::<MetadataValue<Ascii>>()
        {
            meta.insert("x-asc-client", v);
        }
        // Every call has an op id of its own: a model's retry is a new call, never a replay.
        if let Ok(v) = uuid::Uuid::new_v4()
            .to_string()
            .parse::<MetadataValue<Ascii>>()
        {
            meta.insert("x-asc-op-id", v);
        }
        let uri: PathAndQuery = path
            .parse()
            .map_err(|_| ToolError::internal("bad method path"))?;
        let mut grpc = Grpc::new(self.b.routes.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE);
        let run = async {
            grpc.ready()
                .await
                .map_err(|e| ToolError::not_available(clean(&e.to_string())))?;
            let r = grpc
                .unary(req, uri, DynCodec::new(out))
                .await
                .map_err(|s| map_land_status(&s))?;
            Ok::<_, ToolError>(r.into_inner())
        };
        tokio::select! {
            r = run => r,
            () = self.cancel.cancelled() => Err(ToolError::internal("the call was cancelled")),
        }
    }
}

fn strings(v: Vec<String>) -> PbValue {
    PbValue::List(v.into_iter().map(PbValue::String).collect())
}

fn byte_list(m: &DynamicMessage, field: &str) -> Vec<Vec<u8>> {
    m.get_field_by_name(field)
        .and_then(|v| v.as_list().map(<[PbValue]>::to_vec))
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_bytes().map(|b| b.to_vec()))
        .collect()
}

impl LandBackend {
    async fn land(&self, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        let b = &self.inner;
        let not_served = || ToolError::not_available("land is not served by this endpoint");
        let Some(writes) = b.writes.as_ref() else {
            return Err(not_served());
        };
        let Some(principal) = ctx.principal.as_deref() else {
            return Err(ToolError::new(
                "UNAUTHENTICATED",
                "authentication is required",
            ));
        };
        let obj = args.as_object().cloned().unwrap_or_default();
        let repo_arg = obj
            .get("repo")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("repo (owner/name) is required"))?
            .to_owned();
        let repo = RepoName::parse(&repo_arg).map_err(|s| bad(clean(s.message())))?;
        let land = parse_args(&obj)?;

        let slot = b.inflight.acquire(principal).ok_or_else(|| {
            ToolError::rate_limited(
                "too many calls in flight for this principal",
                INFLIGHT_RETRY_MS,
            )
        })?;
        if !b.gate.can_read(principal, repo.as_str()).await {
            return Err(ToolError::new(code::NOT_FOUND, REPO_NOT_FOUND));
        }
        if !writes.can_write(principal, repo.as_str()).await {
            return Err(ToolError::new(
                "PERMISSION_DENIED",
                "write access is required",
            ));
        }
        // Before any ref or object is read: a branch that takes no land requests is refused alone.
        if !self.policy.accepts_land(repo.as_str(), &land.branch).await {
            return Err(ToolError::new(
                "LAND_BRANCH_REFUSED",
                "this branch does not accept land requests",
            ));
        }

        let grant = b.scope.mint(principal, repo.as_str(), Access::Write);
        let wire = Wire {
            b,
            token: grant.token.as_str(),
            cancel: &ctx.cancel,
            principal,
        };
        let out = run(&wire, repo.as_str(), &land).await?;
        drop(grant);
        drop(slot);
        Ok(Reply::Messages(
            stream::once(async move { Ok(out) }).boxed(),
        ))
    }
}

async fn run(wire: &Wire<'_>, repo: &str, land: &LandArgs) -> Result<Value, ToolError> {
    let mut specs = land.revs.clone();
    specs.push(land.branch.clone());
    let resolved = wire
        .repo(
            "Resolve",
            |m| {
                m.set_field_by_name("revspecs", strings(specs.clone()));
            },
            repo,
        )
        .await?;
    let ids = byte_list(&resolved, "ids");
    if ids.len() != land.revs.len() + 1 {
        return Err(ToolError::internal(
            "the server resolved a different number of revisions",
        ));
    }
    let (tips, branch_tip) = ids.split_at(land.revs.len());
    if let Some(i) = tips.iter().position(Vec::is_empty) {
        return Err(ToolError::new(
            code::NOT_FOUND,
            format!("revision not found: {}", clean(&land.revs[i])),
        ));
    }
    if branch_tip[0].is_empty() {
        return Err(ToolError::new(code::NOT_FOUND, "branch not found"));
    }
    let tip = tips[tips.len() - 1].clone();
    // Several changes land as one stack: each must lie under the last one.
    for (i, id) in tips[..tips.len() - 1].iter().enumerate() {
        let r = wire
            .repo(
                "IsAncestor",
                |m| {
                    m.set_field_by_name("ancestor", PbValue::Bytes(id.clone().into()));
                    m.set_field_by_name("descendant", PbValue::Bytes(tip.clone().into()));
                },
                repo,
            )
            .await?;
        if r.get_field_by_name("result").and_then(|v| v.as_bool()) != Some(true) {
            return Err(bad(format!(
                "{} is not under the last change; land one stack at a time",
                clean(&land.revs[i])
            )));
        }
    }

    if land.dry_run {
        let r = wire
            .repo(
                "Merge",
                |m| {
                    m.set_field_by_name("base", PbValue::String(land.branch.clone()));
                    m.set_field_by_name("head", PbValue::String(hex(&tip)));
                    m.set_field_by_name("dry_run", PbValue::Bool(true));
                },
                repo,
            )
            .await?;
        let conflicts: Vec<String> = byte_list(&r, "conflicts")
            .iter()
            .map(|p| path_text(p))
            .collect();
        let protected: Vec<String> = byte_list(&r, "changed_protected")
            .iter()
            .map(|p| path_text(p))
            .collect();
        return Ok(json!({
            "results": [],
            "dry_run": true,
            "would_land": conflicts.is_empty(),
            "conflicts": conflicts,
            "changed_protected": protected,
        }));
    }

    let resp = wire
        .repo(
            "UpdateRefs",
            |m| {
                let Some(f) = m.descriptor().get_field_by_name("updates") else {
                    return;
                };
                let prost_reflect::Kind::Message(ud) = f.kind() else {
                    return;
                };
                let Some(tf) = ud.get_field_by_name("new") else {
                    return;
                };
                let prost_reflect::Kind::Message(td) = tf.kind() else {
                    return;
                };
                let mut target = DynamicMessage::new(td);
                target.set_field_by_name("changeset", PbValue::Bytes(tip.clone().into()));
                let mut update = DynamicMessage::new(ud);
                update.set_field_by_name("name", PbValue::String(land.branch.clone()));
                // The land itself rebases onto the current tip and retries its own CAS.
                update.set_field_by_name("any", PbValue::Bool(true));
                update.set_field_by_name("new", PbValue::Message(target));
                m.set_field_by_name("updates", PbValue::List(vec![PbValue::Message(update)]));
                m.set_field_by_name("mode", PbValue::EnumNumber(UPDATE_MODE_LAND));
            },
            repo,
        )
        .await?;
    let mut v = encode_msg(&resp)?;
    if let Some(o) = v.as_object_mut() {
        o.insert("dry_run".into(), Value::Bool(false));
        o.insert("would_land".into(), Value::Bool(true));
    }
    Ok(v)
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn dry_run_defaults_to_true_and_only_false_turns_it_off() {
        let a = parse_args(&obj(
            json!({"repo": "a/b", "tip": "abc", "branch": "master"}),
        ))
        .unwrap();
        assert!(a.dry_run);
        assert_eq!(a.branch, "refs/heads/master");
        let a = parse_args(&obj(
            json!({"tip": "abc", "branch": "master", "dry_run": false}),
        ))
        .unwrap();
        assert!(!a.dry_run);
        for bad in [json!("false"), json!(0), json!([])] {
            assert!(
                parse_args(&obj(
                    json!({"tip": "abc", "branch": "master", "dry_run": bad})
                ))
                .is_err()
            );
        }
        assert!(
            parse_args(&obj(
                json!({"tip": "abc", "branch": "master", "dry_run": null})
            ))
            .unwrap()
            .dry_run
        );
    }

    #[test]
    fn exactly_one_of_tip_and_changes_and_no_identity_arguments() {
        let ok = |v: Value| parse_args(&obj(v)).is_ok();
        assert!(ok(json!({"changes": ["kkkk"], "branch": "master"})));
        assert!(!ok(json!({"branch": "master"})));
        assert!(!ok(
            json!({"tip": "a", "changes": ["b"], "branch": "master"})
        ));
        assert!(!ok(json!({"changes": [], "branch": "master"})));
        assert!(!ok(json!({"tip": "a"})));
        assert!(!ok(json!({"tip": "a", "branch": "master", "mode": 2})));
        assert!(!ok(
            json!({"tip": "a", "branch": "master", "committer": "x"})
        ));
        assert!(!ok(json!({"tip": "--upload-pack=x", "branch": "master"})));
    }

    #[test]
    fn only_branches_are_land_targets() {
        assert_eq!(
            branch_ref("refs/heads/release/1").unwrap(),
            "refs/heads/release/1"
        );
        for bad in [
            "refs/tags/v1",
            "refs/for/master",
            "refs/users/1/heads/x",
            "a..b",
            "a/.hidden",
            "x.lock",
            "",
            "a b",
            "a//b",
            "/a",
            "a/",
        ] {
            assert!(branch_ref(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn classify_is_cut_at_300_files_and_at_the_budget() {
        let files: Vec<Value> = (0..350)
            .map(|i| json!({"path": format!("f{i}"), "class": "LOGIC"}))
            .collect();
        let v = cap_classify(json!({"files": files, "overall": "LOGIC"}), u64::MAX);
        assert_eq!(v["files"].as_array().unwrap().len(), 300);
        assert_eq!(v["omitted_files"], json!(50));
        assert_eq!(v["overall"], json!("LOGIC"));
        let one = json!({"path": "f0", "class": "LOGIC"}).to_string().len() as u64 + 1;
        let v = cap_classify(
            json!({"files": (0..10).map(|i| json!({"path": format!("f{i}"), "class": "LOGIC"})).collect::<Vec<_>>()}),
            one * 3,
        );
        assert_eq!(v["files"].as_array().unwrap().len(), 3);
        assert_eq!(v["omitted_files"], json!(7));
    }
}
