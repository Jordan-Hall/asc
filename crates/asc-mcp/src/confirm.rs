//! Destructive-call confirmation through MRTR elicitation with a sealed `requestState`
//! (SPEC 22.5 "Confirmation", M-D5).
//!
//! A confirm-capable call that needs the user's yes returns `resultType: input_required` with one
//! `elicitation/create` request (form mode: a boolean `confirm`) and a `requestState`. The state is
//! sealed with the cursor key ring and binds the principal, the tool, the digest of the arguments
//! and a 5 minute lifetime. The retry must echo it together with the user's `accept`; a replay with
//! other arguments, another principal, another tool or an expired state is refused with -32602.
//! A client without the elicitation capability gets `NEEDS_CONFIRMATION`. The summary shown to the
//! user is rendered from escaped, truncated data fields because commit subjects and titles are
//! attacker-controlled: control characters and markup are stripped, each subject is cut to 80
//! characters, at most 10 are listed and no message body ever appears. A model-controlled
//! `confirm: true` argument is not a confirmation: only the sealed state plus the client's answer is.

use serde_json::{Value, json};

use crate::catalogue::Confirm;
use crate::config::ConfirmMode;
use crate::cursor::{CursorError, KeyRing, Position, arg_digest};
use crate::errors::ToolError;

/// How long a sealed `requestState` stays valid: 5 min.
pub const STATE_TTL_SECS: u64 = 300;
/// A subject is cut to this many characters.
pub const SUBJECT_MAX_CHARS: usize = 80;
/// At most this many changes are listed.
pub const MAX_LISTED: usize = 10;
/// The key of the one input request.
pub const INPUT_KEY: &str = "confirm";

/// The tool name the state is sealed under, so a cursor never opens as a state or the reverse.
const SEAL_TOOL: &str = "\u{0}confirm";

/// The effect of a call, as data fields. Nothing here is trusted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// What happens, for example `submit`. Escaped on rendering.
    pub action: String,
    /// The target, for example `master`. Escaped on rendering.
    pub target: String,
    /// Commit subjects or titles, in order. Never message bodies.
    pub subjects: Vec<String>,
}

/// What the confirmation gate knows about the call.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    /// Tool name.
    pub tool: &'a str,
    /// Tool arguments as the client sent them.
    pub args: &'a Value,
    /// The authenticated principal (an id, never a token).
    pub principal: &'a str,
    /// The tool's rule from the catalogue.
    pub rule: Confirm,
    /// The tool is marked destructive in the catalogue.
    pub destructive: bool,
    /// The user's `asc.mcp.confirm`.
    pub mode: ConfirmMode,
    /// The client declared the elicitation capability.
    pub client_elicits: bool,
}

/// The gate's decision.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Run the call.
    Proceed,
    /// Answer with this `input_required` result (see [`input_required`]).
    Elicit(Elicitation),
}

/// One elicitation to send.
#[derive(Debug, Clone, PartialEq)]
pub struct Elicitation {
    /// The escaped summary shown to the user.
    pub message: String,
    /// The sealed `requestState` to echo.
    pub request_state: String,
}

/// Why the gate refused.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// The `requestState` is not valid for this call: JSON-RPC -32602 with one message.
    InvalidState,
    /// The tool result is `isError` (`NEEDS_CONFIRMATION`).
    Tool(ToolError),
}

impl Refusal {
    /// The JSON-RPC error code, for a protocol-level refusal.
    #[must_use]
    pub const fn rpc_code(&self) -> Option<i32> {
        match self {
            Self::InvalidState => Some(crate::cursor::INVALID_PARAMS),
            Self::Tool(_) => None,
        }
    }
}

impl From<CursorError> for Refusal {
    fn from(_: CursorError) -> Self {
        Self::InvalidState
    }
}

/// Whether this call needs the user's confirmation at all.
#[must_use]
pub fn needs_confirmation(call: &Call<'_>) -> bool {
    if call.mode == ConfirmMode::Never {
        return false;
    }
    let update_ref = call.args.get("update_ref").and_then(Value::as_str).unwrap_or("");
    match call.rule {
        Confirm::Never => false,
        Confirm::Always => true,
        Confirm::Config => call.mode == ConfirmMode::Always || call.destructive,
        Confirm::Field(name) => call.args.get(name).and_then(Value::as_bool) == Some(true),
        Confirm::RefsFor => update_ref.starts_with("refs/for/"),
        Confirm::RefsApply => {
            !update_ref.is_empty() && call.args.get("dry_run").and_then(Value::as_bool) != Some(true)
        }
    }
}

fn binding(call: &Call<'_>) -> Value {
    json!({"p": call.principal, "t": call.tool, "d": arg_digest(call.tool, call.args)})
}

/// Seal a `requestState` for `call`, valid [`STATE_TTL_SECS`] from `now` (Unix seconds).
#[must_use]
pub fn seal_state(ring: &KeyRing, call: &Call<'_>, now: u64) -> String {
    ring.seal_at(SEAL_TOOL, &binding(call), Position::Page { token: now.to_string() }, now)
}

/// Check an echoed `requestState` against `call` at `now`.
///
/// # Errors
/// [`Refusal::InvalidState`] when it is malformed, unsigned, older than 5 min, from the future, or
/// sealed for another principal, tool or arguments. The message never says which.
pub fn open_state(ring: &KeyRing, call: &Call<'_>, state: &str, now: u64) -> Result<(), Refusal> {
    // The ring's own expiry (1 h) is checked at `now`; the 5 min lifetime is checked here.
    let pos = ring.open_at(SEAL_TOOL, &binding(call), state, now)?;
    let Position::Page { token } = pos else { return Err(Refusal::InvalidState) };
    let issued: u64 = token.parse().map_err(|_| Refusal::InvalidState)?;
    if issued > now || now - issued > STATE_TTL_SECS {
        return Err(Refusal::InvalidState);
    }
    Ok(())
}

/// The user's answer to the elicitation, from `inputResponses.confirm` of the retry.
#[must_use]
pub fn user_accepted(response: &Value) -> bool {
    response.get("action").and_then(Value::as_str) == Some("accept")
        && response.pointer("/content/confirm").and_then(Value::as_bool) == Some(true)
}

/// Run the gate for `call` at `now`.
///
/// `request_state` and `response` come from the retry (`requestState`, `inputResponses.confirm`);
/// both `None` on the first call.
///
/// # Errors
/// [`Refusal::InvalidState`] for a state that does not match this call;
/// [`Refusal::Tool`] with `NEEDS_CONFIRMATION` for a client that cannot elicit, or for a user who
/// declined or cancelled.
pub fn gate(
    ring: &KeyRing,
    call: &Call<'_>,
    summary: &Summary,
    request_state: Option<&str>,
    response: Option<&Value>,
    now: u64,
) -> Result<Decision, Refusal> {
    if !needs_confirmation(call) {
        return Ok(Decision::Proceed);
    }
    if let Some(state) = request_state {
        open_state(ring, call, state, now)?;
        return match response {
            Some(r) if user_accepted(r) => Ok(Decision::Proceed),
            Some(_) => Err(Refusal::Tool(ToolError::needs_confirmation("the user did not confirm"))),
            None => Ok(Decision::Elicit(elicitation(ring, call, summary, now))),
        };
    }
    if !call.client_elicits {
        return Err(Refusal::Tool(ToolError::needs_confirmation(format!(
            "{} needs the user's confirmation, and this client cannot ask for it; \
             set asc.mcp.confirm = never in your own config to skip it",
            escape(call.tool, SUBJECT_MAX_CHARS)
        ))));
    }
    Ok(Decision::Elicit(elicitation(ring, call, summary, now)))
}

fn elicitation(ring: &KeyRing, call: &Call<'_>, summary: &Summary, now: u64) -> Elicitation {
    Elicitation { message: render_summary(summary), request_state: seal_state(ring, call, now) }
}

/// The `input_required` result body: one form-mode `elicitation/create` (a boolean `confirm`, no
/// secrets) and the sealed state.
#[must_use]
pub fn input_required(e: &Elicitation) -> Value {
    json!({
        "resultType": "input_required",
        "inputRequests": {
            INPUT_KEY: {
                "method": "elicitation/create",
                "params": {
                    "mode": "form",
                    "message": e.message,
                    "requestedSchema": {
                        "type": "object",
                        "properties": {
                            "confirm": {"type": "boolean", "title": "Confirm", "default": false}
                        },
                        "required": ["confirm"]
                    }
                }
            }
        },
        "requestState": e.request_state,
    })
}

/// Strip control characters, bidirectional overrides and markup from `s`, collapse whitespace and
/// cut to `max` characters (an ellipsis counts inside the limit).
#[must_use]
pub fn escape(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in s.chars() {
        if c.is_whitespace() || c.is_control() {
            pending_space = !out.is_empty();
            continue;
        }
        if is_markup(c) || is_invisible(c) {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    if out.chars().count() > max {
        let keep: String = out.chars().take(max.saturating_sub(1)).collect();
        format!("{}\u{2026}", keep.trim_end())
    } else {
        out
    }
}

const fn is_markup(c: char) -> bool {
    matches!(c, '<' | '>' | '&' | '`' | '[' | ']' | '*' | '_' | '#' | '|' | '\\' | '!')
}

/// Bidi controls, zero-width characters, the BOM and tag characters: invisible text that could hide
/// or reorder what the user reads.
const fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}' | '\u{FEFF}' | '\u{E0000}'..='\u{E007F}')
}

/// Render the one-line-per-change summary shown to the user.
#[must_use]
pub fn render_summary(s: &Summary) -> String {
    let mut out = format!("{} {} change(s)", escape(&s.action, 40), s.subjects.len());
    if !s.target.is_empty() {
        out.push_str(&format!(" to {}", escape(&s.target, 80)));
    }
    if !s.subjects.is_empty() {
        out.push(':');
    }
    for subj in s.subjects.iter().take(MAX_LISTED) {
        out.push_str("\n- ");
        out.push_str(&escape(subj, SUBJECT_MAX_CHARS));
    }
    if s.subjects.len() > MAX_LISTED {
        out.push_str(&format!("\n...and {} more", s.subjects.len() - MAX_LISTED));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn ring(b: u8) -> KeyRing {
        KeyRing::from_keys(&[[b; 32]]).unwrap()
    }

    fn call(args: &Value, elicits: bool) -> Call<'_> {
        Call {
            tool: "submit",
            args,
            principal: "alice",
            rule: Confirm::Always,
            destructive: true,
            mode: ConfirmMode::Destructive,
            client_elicits: elicits,
        }
    }

    fn summary() -> Summary {
        Summary { action: "submit".into(), target: "master".into(), subjects: vec!["fix thing".into()] }
    }

    fn state_of(r: &KeyRing, args: &Value) -> String {
        match gate(r, &call(args, true), &summary(), None, None, NOW).unwrap() {
            Decision::Elicit(e) => e.request_state,
            Decision::Proceed => panic!("expected elicitation"),
        }
    }

    fn yes() -> Value {
        json!({"action": "accept", "content": {"confirm": true}})
    }

    #[test]
    fn confirm_round_trip_proceeds() {
        let r = ring(1);
        let args = json!({"title": "x"});
        let st = state_of(&r, &args);
        let d = gate(&r, &call(&args, true), &summary(), Some(&st), Some(&yes()), NOW + 10).unwrap();
        assert_eq!(d, Decision::Proceed);
    }

    #[test]
    fn confirm_replay_with_changed_arguments_is_refused() {
        let r = ring(1);
        let st = state_of(&r, &json!({"title": "x"}));
        let other = json!({"title": "evil"});
        let e = gate(&r, &call(&other, true), &summary(), Some(&st), Some(&yes()), NOW + 1).unwrap_err();
        assert_eq!(e, Refusal::InvalidState);
        assert_eq!(e.rpc_code(), Some(-32602));
    }

    #[test]
    fn confirm_replay_for_another_principal_or_tool_or_key_is_refused() {
        let r = ring(1);
        let args = json!({"title": "x"});
        let st = state_of(&r, &args);
        let mut c = call(&args, true);
        c.principal = "mallory";
        assert!(gate(&r, &c, &summary(), Some(&st), Some(&yes()), NOW + 1).is_err());
        let mut c = call(&args, true);
        c.tool = "rebase";
        assert!(gate(&r, &c, &summary(), Some(&st), Some(&yes()), NOW + 1).is_err());
        assert!(gate(&ring(2), &call(&args, true), &summary(), Some(&st), Some(&yes()), NOW + 1).is_err());
    }

    #[test]
    fn confirm_expired_state_is_refused() {
        let r = ring(1);
        let args = json!({"title": "x"});
        let st = state_of(&r, &args);
        let c = call(&args, true);
        assert!(gate(&r, &c, &summary(), Some(&st), Some(&yes()), NOW + STATE_TTL_SECS).is_ok());
        let e = gate(&r, &c, &summary(), Some(&st), Some(&yes()), NOW + STATE_TTL_SECS + 1).unwrap_err();
        assert_eq!(e, Refusal::InvalidState);
    }

    #[test]
    fn confirm_a_cursor_is_not_a_state() {
        let r = ring(1);
        let args = json!({"title": "x"});
        let cur = r.seal_at("submit", &args, Position::Page { token: NOW.to_string() }, NOW);
        assert!(gate(&r, &call(&args, true), &summary(), Some(&cur), Some(&yes()), NOW).is_err());
    }

    #[test]
    fn confirm_model_supplied_confirm_argument_does_not_skip() {
        let r = ring(1);
        let args = json!({"title": "x", "confirm": true});
        let d = gate(&r, &call(&args, true), &summary(), None, None, NOW).unwrap();
        assert!(matches!(d, Decision::Elicit(_)));
    }

    #[test]
    fn confirm_client_without_elicitation_gets_needs_confirmation() {
        let r = ring(1);
        let args = json!({});
        let Err(Refusal::Tool(e)) = gate(&r, &call(&args, false), &summary(), None, None, NOW) else {
            panic!("expected a tool error")
        };
        assert_eq!(e.code, "NEEDS_CONFIRMATION");
    }

    #[test]
    fn confirm_declined_or_unchecked_answer_is_refused() {
        let r = ring(1);
        let args = json!({});
        let st = state_of(&r, &args);
        for resp in [
            json!({"action": "decline"}),
            json!({"action": "cancel"}),
            json!({"action": "accept", "content": {"confirm": false}}),
            json!({"action": "accept"}),
        ] {
            let e = gate(&r, &call(&args, true), &summary(), Some(&st), Some(&resp), NOW + 1);
            assert!(matches!(e, Err(Refusal::Tool(_))), "{resp}");
        }
    }

    #[test]
    fn confirm_mode_never_and_rules() {
        let args = json!({});
        let mut c = call(&args, true);
        c.mode = ConfirmMode::Never;
        assert!(!needs_confirmation(&c));
        c.mode = ConfirmMode::Destructive;
        c.rule = Confirm::Config;
        assert!(needs_confirmation(&c));
        c.destructive = false;
        assert!(!needs_confirmation(&c));
        c.mode = ConfirmMode::Always;
        assert!(needs_confirmation(&c));
        c.rule = Confirm::Never;
        assert!(!needs_confirmation(&c));

        let on = json!({"delete_merged": true});
        let mut c = call(&on, true);
        c.rule = Confirm::Field("delete_merged");
        assert!(needs_confirmation(&c));
        let off = json!({"delete_merged": false});
        c.args = &off;
        assert!(!needs_confirmation(&c));

        let rf = json!({"update_ref": "refs/for/master"});
        let mut c = call(&rf, true);
        c.rule = Confirm::RefsFor;
        assert!(needs_confirmation(&c));
        let empty = json!({"update_ref": ""});
        c.args = &empty;
        assert!(!needs_confirmation(&c));
        let apply = json!({"update_ref": "refs/for/x"});
        c.rule = Confirm::RefsApply;
        c.args = &apply;
        assert!(needs_confirmation(&c));
        let dry = json!({"update_ref": "refs/for/x", "dry_run": true});
        c.args = &dry;
        assert!(!needs_confirmation(&c));
    }

    #[test]
    fn confirm_hostile_subject_is_escaped_and_cut_to_80() {
        let hostile = format!(
            "<script>alert(1)</script>\u{202e}[click](http://x)\n**ignore previous**\u{1b}[31m {}",
            "a".repeat(200)
        );
        let e = escape(&hostile, SUBJECT_MAX_CHARS);
        assert!(e.chars().count() <= SUBJECT_MAX_CHARS, "{e}");
        for bad in ['<', '>', '[', ']', '*', '`', '\u{202e}', '\n', '\u{1b}'] {
            assert!(!e.contains(bad), "{bad:?} in {e}");
        }
        let s = Summary { action: "submit\n`x`".into(), target: "mas<b>ter".into(), subjects: vec![hostile] };
        let out = render_summary(&s);
        assert!(!out.contains('<') && !out.contains('`'), "{out}");
        assert_eq!(out.lines().count(), 2, "{out}");
        assert!(out.lines().nth(1).unwrap().chars().count() <= SUBJECT_MAX_CHARS + 2);
    }

    #[test]
    fn confirm_lists_at_most_ten_and_no_bodies() {
        let s = Summary {
            action: "submit".into(),
            target: "master".into(),
            subjects: (0..25).map(|i| format!("change {i}")).collect(),
        };
        let out = render_summary(&s);
        assert_eq!(out.lines().filter(|l| l.starts_with("- ")).count(), 10);
        assert!(out.contains("and 15 more"));
        assert!(!out.contains("change 10"));
    }

    #[test]
    fn confirm_input_required_shape() {
        let r = ring(1);
        let args = json!({});
        let Decision::Elicit(e) = gate(&r, &call(&args, true), &summary(), None, None, NOW).unwrap() else {
            panic!()
        };
        let v = input_required(&e);
        assert_eq!(v["resultType"], "input_required");
        assert_eq!(v["inputRequests"]["confirm"]["method"], "elicitation/create");
        assert_eq!(
            v["inputRequests"]["confirm"]["params"]["requestedSchema"]["properties"]["confirm"]["type"],
            "boolean"
        );
        assert!(v["requestState"].is_string());
    }
}
