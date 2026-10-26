//! `--json` for the stack verbs that print nothing typed of their own: `create`, `modify`,
//! `reword`, `abort` and `stack rename` (SPEC §11.2, §24.2; PLAN P45).
//!
//! Like `cli::json_verbs`, the verb keeps its one implementation. With `--json` the registration
//! macro hands the argv here: [`run`] runs the same verb as a child of this binary without
//! `--json` (stdout dropped, stderr passed through), reads the operation log before and after,
//! and prints one `OpResult` as the only line on stdout. `op` is the newest operation the verb
//! logged, `refs` the net move of each ref over all the operations it logged, and `status` is
//! `ok` when the verb logged an operation (or, for [`Done::Always`], when it succeeded) and
//! `nothing` otherwise. The exit code is the verb's.
//!
//! A verb that fails prints no result: the host reads the exit code and stderr. The one
//! exception is exit 1 with a paused restack (`asc-stack-op.json`): that prints a `paused`
//! result whose `hints` say how to go on. The conflicting changes are not listed.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::process::{ExitCode, Stdio};

use serde_json::Value;

use super::sl::fatal;
use crate::cli::common::{Ctx, Result};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::stack::json::{OpResult, RefMove, Status, line};
use crate::stack::opstate;

/// Most operations one run reads back from the log.
const MAX_OPS: usize = 200;
/// Most ref moves one result lists.
const MAX_REFS: usize = 1000;

/// When a successful run counts as `ok`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Done {
    /// Only when the verb logged an operation.
    IfLogged,
    /// Whenever the verb succeeded: its effect is not a ref move (`abort`).
    Always,
}

/// `argv` (`[verb, args…]`) without its `--json`, or `None` when it has none before a `--`.
pub(crate) fn strip(argv: &[OsString]) -> Option<Vec<OsString>> {
    let end = argv
        .iter()
        .skip(1)
        .position(|a| a == "--")
        .map_or(argv.len(), |i| i + 1);
    let at = argv[..end].iter().skip(1).position(|a| a == "--json")? + 1;
    let mut plain = argv.to_vec();
    plain.remove(at);
    Some(plain)
}

/// One logged operation: its id and the refs it moved.
struct Logged {
    op: String,
    refs: Vec<RefMove>,
}

/// Runs `asc <lead> <argv…>` in the workspace directory; `Some(stdout)` when it exits 0.
async fn capture(ctx: &Ctx, args: Vec<OsString>) -> Option<Vec<u8>> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = ctx.child(exe).ok()?;
    cmd.current_dir(&ctx.cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let out = tokio::task::spawn_blocking(move || cmd.output())
        .await
        .ok()?
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// The operation log, newest first (empty when it cannot be read).
async fn op_log(ctx: &Ctx) -> Vec<Logged> {
    let limit = MAX_OPS.to_string();
    let args = ["op", "log", "--json", "-n", &limit]
        .map(OsString::from)
        .to_vec();
    let Some(out) = capture(ctx, args).await else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out);
    let Some(v) = text
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
    else {
        return Vec::new();
    };
    let rows = v
        .get("ops")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    rows.iter()
        .filter_map(|r| {
            let op = r.get("op")?.as_str()?.to_owned();
            let refs = r
                .get("refs")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .filter_map(|m| {
                    let text = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_owned);
                    Some(RefMove {
                        name: text("name")?,
                        old: text("old"),
                        new: text("new"),
                    })
                })
                .collect();
            Some(Logged { op, refs })
        })
        .collect()
}

/// The net move of each ref over `fresh` (newest first): the oldest `old` and the newest `new`;
/// a ref that ended where it began is left out. At most [`MAX_REFS`] moves; the flag says the
/// list was cut.
fn net_refs(fresh: &[Logged]) -> (Vec<RefMove>, bool) {
    let mut by_name: BTreeMap<&str, (Option<&str>, Option<&str>)> = BTreeMap::new();
    for row in fresh {
        for m in &row.refs {
            by_name
                .entry(&m.name)
                .and_modify(|e| e.0 = m.old.as_deref())
                .or_insert((m.old.as_deref(), m.new.as_deref()));
        }
    }
    let mut moves: Vec<RefMove> = by_name
        .into_iter()
        .filter(|(_, (old, new))| old != new)
        .map(|(name, (old, new))| RefMove {
            name: name.to_owned(),
            old: old.map(str::to_owned),
            new: new.map(str::to_owned),
        })
        .collect();
    let cut = moves.len() > MAX_REFS;
    moves.truncate(MAX_REFS);
    (moves, cut)
}

/// The result of a run that exited 0, from the operations it logged (`fresh`, newest first).
fn result_of(fresh: &[Logged], done: Done) -> OpResult {
    let logged = !fresh.is_empty();
    let mut r = OpResult::new(if logged || done == Done::Always {
        Status::Ok
    } else {
        Status::Nothing
    });
    r.op = fresh.first().map(|l| l.op.clone());
    let (refs, cut) = net_refs(fresh);
    r.refs = refs;
    if cut {
        r.hints
            .push(format!("more than {MAX_REFS} refs moved; the list is cut"));
    }
    r
}

/// The operations logged after `before` (the newest id seen earlier), newest first.
fn fresh_since(after: Vec<Logged>, before: Option<&str>) -> Vec<Logged> {
    match before {
        None => after,
        Some(id) => after.into_iter().take_while(|l| l.op != id).collect(),
    }
}

/// Runs the verb `argv` (`[verb, args…]`, `--json` already removed) after `lead` (the words
/// of its parent verb, like `stack`) and prints its `OpResult`.
///
/// # Errors
/// The verb cannot be started or the result cannot be printed.
pub(crate) async fn run(
    ctx: &Ctx,
    lead: &[&str],
    argv: Vec<OsString>,
    done: Done,
) -> Result<ExitCode> {
    let exe = std::env::current_exe().map_err(|e| fatal(format!("cannot find asc itself: {e}")))?;
    let before = op_log(ctx).await.first().map(|l| l.op.clone());

    let mut cmd = ctx.child(exe)?;
    cmd.current_dir(&ctx.cwd)
        .args(lead)
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let status = tokio::task::spawn_blocking(move || cmd.status())
        .await
        .map_err(|e| fatal(format!("the verb did not finish: {e}")))?
        .map_err(|e| fatal(format!("cannot start the verb: {e}")))?;
    let code = status.code().map_or(128, |c| c.clamp(0, 255) as u8);

    let result = if code == 0 {
        result_of(&fresh_since(op_log(ctx).await, before.as_deref()), done)
    } else if code == 1 && paused(ctx) {
        let mut r = OpResult::new(Status::Paused);
        r.hints.push(
            "a restack stopped on a conflict: resolve it and run asc continue, or run asc abort"
                .to_owned(),
        );
        r
    } else {
        return Ok(ExitCode::from(code));
    };
    let mut out = Output::open(ctx, "stack", PagerWant::Never)?;
    out.write_all(line(&result).map_err(fatal)?.as_bytes())?;
    out.finish()?;
    Ok(ExitCode::from(code))
}

/// Whether a restack is paused in this workspace.
fn paused(ctx: &Ctx) -> bool {
    ctx.ws()
        .is_ok_and(|ws| matches!(opstate::load(ws.projection.git_dir()), Ok(Some(_))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn mv(name: &str, old: Option<&str>, new: Option<&str>) -> RefMove {
        RefMove {
            name: name.into(),
            old: old.map(Into::into),
            new: new.map(Into::into),
        }
    }

    fn logged(op: &str, refs: Vec<RefMove>) -> Logged {
        Logged {
            op: op.into(),
            refs,
        }
    }

    #[test]
    fn strip_drops_the_first_json_before_a_double_dash() {
        let got = strip(&argv(&["create", "--json", "-m", "x", "--", "--json"])).unwrap();
        assert_eq!(got, argv(&["create", "-m", "x", "--", "--json"]));
        assert!(strip(&argv(&["create", "-m", "x", "--", "--json"])).is_none());
        assert!(strip(&argv(&["abort"])).is_none());
    }

    #[test]
    fn refs_net_out_over_the_operations_of_a_run() {
        // Newest first: the branch moved a -> b, then b -> c; HEAD went and came back.
        let fresh = vec![
            logged(
                "o2",
                vec![
                    mv("refs/heads/f", Some("bb"), Some("cc")),
                    mv("HEAD", Some("aa"), Some("aa")),
                ],
            ),
            logged(
                "o1",
                vec![
                    mv("refs/heads/f", Some("aa"), Some("bb")),
                    mv("HEAD", Some("aa"), Some("aa")),
                ],
            ),
        ];
        let (refs, cut) = net_refs(&fresh);
        assert!(!cut);
        assert_eq!(refs, vec![mv("refs/heads/f", Some("aa"), Some("cc"))]);
    }

    #[test]
    fn status_follows_the_log_unless_the_verb_always_counts() {
        let none = result_of(&[], Done::IfLogged);
        assert_eq!((none.status, none.op), (Status::Nothing, None));
        assert_eq!(result_of(&[], Done::Always).status, Status::Ok);
        let one = result_of(&[logged("o1", vec![])], Done::IfLogged);
        assert_eq!((one.status, one.op.as_deref()), (Status::Ok, Some("o1")));
    }

    #[test]
    fn only_operations_after_the_earlier_newest_are_fresh() {
        let after = vec![
            logged("o3", vec![]),
            logged("o2", vec![]),
            logged("o1", vec![]),
        ];
        let fresh = fresh_since(after, Some("o2"));
        assert_eq!(
            fresh.iter().map(|l| l.op.as_str()).collect::<Vec<_>>(),
            ["o3"]
        );
        assert!(fresh_since(vec![logged("o1", vec![])], Some("o1")).is_empty());
        assert_eq!(fresh_since(vec![logged("o1", vec![])], None).len(), 1);
    }
}
