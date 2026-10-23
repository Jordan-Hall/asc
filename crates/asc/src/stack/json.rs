//! The `--json` result types of the stack verbs (SPEC §20.7). Every object carries
//! `"version": 1`, ids are lowercase hex SHA-1s or change ids, field names are `snake_case`
//! and a result is always an object (a list is wrapped), following the asc JSON profile.
//! Verbs build these and print them with [`line`]; the exit code comes from [`Status`].

use std::process::ExitCode;

use serde::Serialize;

/// The schema version every result carries.
pub const VERSION: u32 = 1;

/// The `+0530` form of a zone offset in minutes (clamped to git's range of +-99:59).
pub fn tz(minutes: i32) -> String {
    let m = minutes.clamp(-(99 * 60 + 59), 99 * 60 + 59);
    let sign = if m < 0 { '-' } else { '+' };
    let a = m.unsigned_abs();
    format!("{sign}{:02}{:02}", a / 60, a % 60)
}

/// Serialises a result as one JSON line, newline included.
pub fn line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    Ok(s)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: i64,
    pub tz: String,
}

// ---- asc sl --json (Workspace.Stacks) -----------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Sl {
    pub version: u32,
    pub trunks: Vec<Trunk>,
    pub stacks: Vec<StackOut>,
}

impl Sl {
    pub fn new(trunks: Vec<Trunk>, stacks: Vec<StackOut>) -> Self {
        Self {
            version: VERSION,
            trunks,
            stacks,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Trunk {
    pub name: String,
    pub tip: String,
    pub remote: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StackOut {
    pub trunk: String,
    pub base: String,
    pub behind: u32,
    /// Topological, bottom to top.
    pub changes: Vec<ChangeOut>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeState {
    Draft,
    Landed,
    Obsolete,
    Divergent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChangeOut {
    pub change: String,
    pub short: String,
    pub commit: String,
    pub asc: String,
    pub parent_change: Option<String>,
    pub branches: Vec<String>,
    pub subject: String,
    pub author: Signature,
    pub state: ChangeState,
    pub needs_restack: bool,
    pub frozen: bool,
    pub empty: bool,
    pub current: bool,
    pub worktree: Option<String>,
    /// Present only with `--remote`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<Review>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewState {
    Draft,
    Open,
    Merged,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Checks {
    Pending,
    Success,
    Failure,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Queue {
    None,
    Queued,
    Landing,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Review {
    pub number: u64,
    pub url: String,
    pub state: ReviewState,
    pub approvals: u32,
    pub checks: Checks,
    pub queue: Queue,
    pub version: u32,
    pub reviewed_version: u32,
}

// ---- every mutating verb -----------------------------------------------------------------

/// The printed outcome of a verb, and with it the exit code (SPEC §20.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Nothing,
    Paused,
    Ambiguous,
}

impl Status {
    /// 0 done or nothing to do, 1 paused on a conflict, 2 ambiguous. The codes do not change
    /// under `--json`.
    pub fn exit_code(self) -> ExitCode {
        match self {
            Self::Ok | Self::Nothing => ExitCode::SUCCESS,
            Self::Paused => ExitCode::from(1),
            Self::Ambiguous => ExitCode::from(2),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    CheckedOutElsewhere,
    Frozen,
    Conflict,
    Merge,
    Immutable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Rewritten {
    pub change: String,
    pub old: String,
    pub new: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RefMove {
    pub name: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub change: String,
    pub reason: SkipReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Conflict {
    pub change: String,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub change: String,
    pub subject: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewRow {
    pub change: String,
    pub number: u64,
    pub url: String,
    pub created: bool,
    pub version: u32,
}

/// The result of every mutating verb.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OpResult {
    pub version: u32,
    pub op: Option<String>,
    pub status: Status,
    pub rewritten: Vec<Rewritten>,
    pub refs: Vec<RefMove>,
    pub skipped: Vec<Skipped>,
    pub conflicts: Vec<Conflict>,
    pub candidates: Vec<Candidate>,
    pub reviews: Vec<ReviewRow>,
    pub hints: Vec<String>,
}

impl OpResult {
    /// An empty result with this status.
    pub fn new(status: Status) -> Self {
        Self {
            version: VERSION,
            op: None,
            status,
            rewritten: Vec::new(),
            refs: Vec::new(),
            skipped: Vec::new(),
            conflicts: Vec::new(),
            candidates: Vec::new(),
            reviews: Vec::new(),
            hints: Vec::new(),
        }
    }

    /// Exit 2: nothing moved, `candidates` lists the choices.
    pub fn ambiguous(candidates: Vec<Candidate>) -> Self {
        Self {
            candidates,
            ..Self::new(Status::Ambiguous)
        }
    }

    /// Exit 1: stopped on conflicts.
    pub fn paused(conflicts: Vec<Conflict>) -> Self {
        Self {
            conflicts,
            ..Self::new(Status::Paused)
        }
    }
}

// ---- asc op log / asc evolog ---------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OpLog {
    pub version: u32,
    pub ops: Vec<OpRow>,
}

impl OpLog {
    pub fn new(ops: Vec<OpRow>) -> Self {
        Self {
            version: VERSION,
            ops,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OpRow {
    pub op: String,
    pub seq: u64,
    pub at: i64,
    pub ws: String,
    /// Already redacted (SPEC §20.3).
    pub argv: Vec<String>,
    pub refs: Vec<RefMove>,
    pub snapshot: Option<String>,
    pub undone: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Evolog {
    pub version: u32,
    pub versions: Vec<VersionRow>,
}

impl Evolog {
    pub fn new(versions: Vec<VersionRow>) -> Self {
        Self {
            version: VERSION,
            versions,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VersionRow {
    pub version: u32,
    pub commit: String,
    /// `amend`, `rebase`, ... (the `RewriteOp` name, lowercase).
    pub op: String,
    pub at: i64,
    pub actor: String,
    pub preds: Vec<String>,
    pub published: bool,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn parse<T: Serialize>(v: &T) -> Value {
        serde_json::from_str(line(v).unwrap().trim_end()).unwrap()
    }

    #[test]
    fn tz_formats_like_git() {
        assert_eq!(tz(0), "+0000");
        assert_eq!(tz(330), "+0530");
        assert_eq!(tz(-480), "-0800");
        assert_eq!(tz(i32::MAX), "+9959");
        assert_eq!(tz(i32::MIN), "-9959");
    }

    #[test]
    fn op_result_has_every_field_and_the_version() {
        let v = parse(&OpResult::new(Status::Nothing));
        assert_eq!(
            v,
            json!({"version": 1, "op": null, "status": "nothing", "rewritten": [], "refs": [],
                   "skipped": [], "conflicts": [], "candidates": [], "reviews": [], "hints": []})
        );
        let v = parse(&OpResult::ambiguous(vec![Candidate {
            change: "c".into(),
            subject: "s".into(),
        }]));
        assert_eq!(v["status"], "ambiguous");
        assert_eq!(v["candidates"][0]["subject"], "s");
    }

    #[test]
    fn skip_reasons_are_kebab_case() {
        let s = Skipped {
            change: "c".into(),
            reason: SkipReason::CheckedOutElsewhere,
        };
        assert_eq!(parse(&s)["reason"], "checked-out-elsewhere");
    }

    #[test]
    fn exit_codes_follow_the_status() {
        assert_eq!(Status::Ok.exit_code(), ExitCode::SUCCESS);
        assert_eq!(Status::Nothing.exit_code(), ExitCode::SUCCESS);
        assert_eq!(Status::Paused.exit_code(), ExitCode::from(1));
        assert_eq!(Status::Ambiguous.exit_code(), ExitCode::from(2));
    }

    #[test]
    fn lists_are_wrapped_in_objects() {
        assert_eq!(parse(&OpLog::new(vec![])), json!({"version": 1, "ops": []}));
        assert_eq!(
            parse(&Evolog::new(vec![])),
            json!({"version": 1, "versions": []})
        );
    }

    #[test]
    fn sl_shape() {
        let change = ChangeOut {
            change: "k".repeat(32),
            short: "k".repeat(12),
            commit: "a".repeat(40),
            asc: "b".repeat(64),
            parent_change: None,
            branches: vec!["jordan/parser".into()],
            subject: "x".into(),
            author: Signature {
                name: "n".into(),
                email: "e".into(),
                time: 0,
                tz: tz(0),
            },
            state: ChangeState::Draft,
            needs_restack: false,
            frozen: false,
            empty: false,
            current: true,
            worktree: None,
            review: None,
        };
        let sl = Sl::new(
            vec![Trunk {
                name: "master".into(),
                tip: "c".repeat(40),
                remote: Some("origin".into()),
            }],
            vec![StackOut {
                trunk: "master".into(),
                base: "c".repeat(40),
                behind: 3,
                changes: vec![change],
            }],
        );
        let v = parse(&sl);
        assert_eq!(v["version"], 1);
        assert_eq!(v["stacks"][0]["changes"][0]["state"], "draft");
        assert_eq!(v["stacks"][0]["changes"][0]["parent_change"], Value::Null);
        assert!(v["stacks"][0]["changes"][0].get("review").is_none());
        assert_eq!(v["stacks"][0]["changes"][0]["author"]["tz"], "+0000");
    }

    #[test]
    fn review_enums_are_lowercase() {
        let r = Review {
            number: 42,
            url: "u".into(),
            state: ReviewState::Merged,
            approvals: 1,
            checks: Checks::None,
            queue: Queue::Landing,
            version: 3,
            reviewed_version: 2,
        };
        let v = parse(&r);
        assert_eq!(v["state"], "merged");
        assert_eq!(v["checks"], "none");
        assert_eq!(v["queue"], "landing");
    }
}
