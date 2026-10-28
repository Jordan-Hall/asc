# ASC server side of the review feature gaps (rows needs-asc-backend / needs-both)

Source: D:/dev/asc-hub docs/asc/feature-gaps/review.md at 9285890ac2 (row id = line number in that file).
Branch feat/asc-review (worktree D:/tmp/feat-asc-review), based on master 09452f9.

## Status: stopped before any RPC family (coordinator order: UI first, RPC families in Phase 3)

Rows done: none. Rows not done: all 49 data rows listed below (rows 60 to 313). No proto or server code was changed.

## Findings for Phase 3 (from reading the code)

- Protos are in proto/asc/v1 (not crates/asc-rpc/proto). Migrations are in crates/asc-serve/migrations/{sqlite,postgres}, last is 0561; use a distinct range (0570+) to avoid clashing with the other worktrees.
- Claim-aware repo calls: Env::context (crates/asc-serve/src/review/mod.rs) authorises the credential only. Add an `acting` variant: when the principal is the host service and a claim is present, classify(subject = repo) must be Person(claim) with claim.allows(access), act as claim.user, keep the host as relay (audit on_behalf); Refused means PERMISSION_DENIED; no claim means unchanged. CallCtx.admin can carry the claim. Review.AddComment needs this, since it calls require_handle and the host service has no handle (this is why hub comments cannot reach Review today).
- MergeQueue is proto only: no server implementation exists (BUILDOUT section 7, `asc-serve/src/mergequeue/`); landing goes through the forge queue (changes/request_merge.rs). ETAs and the live stream need a queue store plus Enqueue/List/Get/Watch (or a host-reported entry RPC).
- Approvals live in the hub (ReviewState.approvals, reviewed_version via SetReviewState); per-version approval and carry-over need an asc table plus a RecordApproval RPC and a per-file unchanged set from interdiff.
- AgentRun row already stores `task` (agent_runs.task); only the wire message lacks it. Agent loop hooks: agents/runtime.rs `converse` (before each tool call: pause, permission, plan), tools.rs Outcome::Ask pattern gives WAITING_INPUT handling. Permission requests can be modelled as configured `ask_before` command prefixes plus a `request_permission` tool; sandbox allow-list is in asc-ci (not editable here).
- Policy dir and crates/asc-serve/migrations are outside the FILES list; gate stats and overrides were planned under review/ as Review RPCs.
- Build: wsl script at scratchpad w.sh (CARGO_TARGET_DIR=/mnt/e/cgt/shared); `check -p asc-serve -p asc-rpc --lib` passes on the base (2m22s).

## Rows not done
- row 18: Board | done | ui-only-missing
- row 60: Review | Intent, 'Where to look' pointers (file:line), Size, Files, Risk summary block | needs-both
- row 67: Review | Threads visible to asc clients: hub comments are not written to the asc Review service (ListThreads/AddComment | needs-both
- row 69: Review | Suggested change block in a comment, 'Apply as v5' (creates a version, undoable op, resets approvals), 'Ask co | needs-both
- row 75: Review | Approval carry-over: approval on an unchanged diff survives a new version and says so | needs-both
- row 76: Review | Landing card: queue position, estimated start, 'Add changes 1 and 2 to queue', dry-run line | needs-both
- row 80: Review | 'How it was made': run id, task, tool calls, versions, files read, 'never touched infra/', link to run | needs-both
- row 81: Review | Conflict with main: stays open, other changes unblocked, markers, 'Resolve in editor', 'Show both sides', 'Ask | needs-both
- row 83: Review | Applied-as-v5 toast with Undo (8 s, role=status) | needs-both
- row 91: ReviewVersions | 'Approved by Priya' marker on the version she approved | needs-both
- row 95: ReviewVersions | Compare header: '+2 -1 in 1 of 3 files, Priya's approval still applies to the other 2' | needs-both
- row 98: ReviewVersions | Land panel summary: merge order, signed-by list, queue position and ETA, undo window 30 min | needs-both
- row 111: NewChange | 'Draft from the diff' (AI writes What/Why/How/Risk from the diff) | needs-both
- row 112: NewChange | 'Where to look' pointers editor | needs-both
- row 113: NewChange | Reviewer suggestions from ownership and review history, out-of-office note, agent reviewers labelled | needs-both
- row 114: NewChange | Options: open as draft, keep as part of stack, land when approved and green, ask agent to babysit checks | needs-both
- row 119: NewChange | Blocked-by-secret-scan banner before open; nothing-to-compare; opened/draft toasts | needs-both
- row 126: Checks | Headline stats: pass rate vs previous period, failure groups open, flaky above threshold, median duration + sl | needs-both
- row 150: ChangeChecks | Compared with main: main green, 'these 2 failures are new', slowest test 41 s (was 12 s) | needs-both
- row 159: Agents | Stats: Landed this week (31% of changes), Spend this month ($142 of $400 cap) | needs-both
- row 163: Agents | Run row progress '3 of 4 steps' with bar | needs-asc-backend
- row 165: Agents | 'Needs you' panel: permission request (Allow once / Deny) and plan approval (Review plan), applied in place | needs-both
- row 166: Agents | 'Agents on this repository' card (scope, status, changes landed in 30 days, Manage access) | needs-both
- row 178: AgentRun | Task quote ('Task by Maya Okafor: ...') on the run page | needs-asc-backend
- row 180: AgentRun | Pause (holds the run at the next tool call) | needs-both
- row 182: AgentRun | Plan block with step states and 'You approved at 10:02' | needs-both
- row 183: AgentRun | Permission request banner: Allow once / Deny and continue / Answer in own words; records who and why | needs-both
- row 189: AgentRun | Budget 'Edit limits' on a live run | needs-both
- row 191: AgentRun | Checkpoints list with Roll back (logged) | needs-both
- row 192: AgentRun | Audit note and 'Download the audit log' (90 days, linked from each change) | needs-both
- row 193: AgentRun | Failed state 'hit its model token limit' with 'Raise the limit and continue' and Review the 3 changes | needs-both
- row 202: AgentNew | 'Ask me to approve the plan first' switch | needs-both
- row 204: AgentNew | Estimate 'About 10 min and $1.60' from similar runs | needs-asc-backend
- row 205: AgentNew | Recent tasks with Reuse | needs-both
- row 206: AgentNew | Limits above the policy cap are lowered with a message, not rejected | needs-both
- row 215: AgentAccess | Spend this month per token ($58 of $120) as a monthly budget | needs-both
- row 216: AgentAccess | Approvals log (permission and plan requests with decision and decider) | needs-both
- row 218: AgentAccess | Spend this month panel: cap, daily spend chart, per-agent totals, 'people using AI keys' | needs-both
- row 234: Workflows | Capacity '3 running of 6 allowed' and shared agent budget line | needs-both
- row 271: Policy | Stat cards: Changes checked, Passed first time, Held by the gate (cleared/open), Median time to clear | needs-both
- row 274: Policy | 'Checked and held, per day' chart with table equivalent | needs-both
- row 275: Policy | Rules in force with 'N held of M' per rule | needs-both
- row 276: Policy | Maintainer overrides (signed, logged exception, reason) | needs-asc-backend
- row 294: Ops | Preview lines '4 changes go back', 'approvals on 2 unchanged diffs are kept' | needs-both
- row 306: Queue | Row detail: 'unit running on merged trunk, about 3 min' and 'starts about 6 min' ETAs | needs-both
- row 313: Queue | Live update: list slides when an item lands, Checking pulses | needs-both
