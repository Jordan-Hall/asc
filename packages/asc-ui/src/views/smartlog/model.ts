// Smartlog view model (SPEC §24.3.2, §24.4.5): pure rows, badges and the reason every disabled
// button states. No DOM here, so the rules are unit-tested without a browser.
import type { MoveTarget } from "../../ops/mutations";
import { applyPreview } from "../../ops/preview";
import type { StackChange, Stacks } from "../../state/model";

/** Facts the `Stacks` model does not carry; the host supplies them from the workspace extras. */
export interface ChangeExtras {
  readonly divergent?: boolean;
  /** Name of the worktree that has this change checked out. */
  readonly worktree?: string;
  /** Change version (1 = never amended). */
  readonly version?: number;
}

export interface UncommittedFile {
  readonly path: string;
  readonly status: "M" | "A" | "D" | "R" | "?";
}

/** A draft that lives on another machine (`Cloud.GetWorkspace`). */
export interface CloudDraft {
  readonly id: string;
  readonly subject: string;
  readonly machine: string;
}

export interface SmartlogInput {
  readonly stacks: Stacks;
  readonly extras: Readonly<Record<string, ChangeExtras>>;
  readonly uncommitted: readonly UncommittedFile[];
  /** Hidden change ids (B05). */
  readonly hidden: ReadonlySet<string>;
  readonly cloudDrafts: readonly CloudDraft[];
  readonly showCloud: boolean;
  readonly showHidden: boolean;
  /** Checked uncommitted paths. */
  readonly checked: ReadonlySet<string>;
}

export type ActionId =
  | "checkout"
  | "restack"
  | "uncommit"
  | "unamend"
  | "abandon"
  | "hide"
  | "unhide"
  | "create"
  | "modify";

export interface ActionState {
  readonly id: ActionId;
  readonly label: string;
  readonly enabled: boolean;
  /** Set exactly when `enabled` is false: shown as the tooltip. */
  readonly reason?: string;
}

export type BadgeKind =
  | "pr"
  | "checks"
  | "restack"
  | "divergent"
  | "frozen"
  | "worktree"
  | "landed"
  | "hidden"
  | "version";

export interface Badge {
  readonly kind: BadgeKind;
  readonly text: string;
}

export interface ChangeRow {
  readonly kind: "change";
  readonly id: string;
  readonly subject: string;
  readonly depth: number;
  readonly current: boolean;
  readonly branches: readonly string[];
  readonly badges: readonly Badge[];
  readonly hidden: boolean;
  readonly frozen: boolean;
  readonly landed: boolean;
  readonly actions: readonly ActionState[];
}

export interface UncommittedRow {
  readonly kind: "uncommitted";
  readonly id: "@uncommitted";
  readonly files: readonly (UncommittedFile & { readonly checked: boolean })[];
  readonly actions: readonly ActionState[];
}

export interface CloudRow {
  readonly kind: "cloud";
  readonly id: string;
  readonly subject: string;
  readonly machine: string;
}

export type SmartlogRow = UncommittedRow | ChangeRow | CloudRow;

export const UNCOMMITTED_ID = "@uncommitted";
export const REASON_DIRTY = "Uncommitted changes: create or stash them first";
export const REASON_FROZEN = "Frozen";
export const REASON_LANDED = "Landed";

const gate = (id: ActionId, label: string, reason: string | undefined): ActionState =>
  reason === undefined
    ? { id, label, enabled: true }
    : { id, label, enabled: false, reason };

const isLanded = (c: StackChange): boolean => c.state === "landed" || c.state === "merged";

export function changeActions(
  c: StackChange,
  extras: ChangeExtras | undefined,
  hidden: boolean,
  dirty: boolean,
): readonly ActionState[] {
  const landed = isLanded(c);
  const locked = c.frozen ? REASON_FROZEN : landed ? REASON_LANDED : undefined;
  const notCurrent = c.current ? undefined : "Check out this change first";
  let checkout: string | undefined;
  if (c.current) checkout = "Already checked out";
  else if (landed) checkout = REASON_LANDED;
  else if (dirty) checkout = REASON_DIRTY;
  else if (extras?.worktree !== undefined) checkout = `Checked out in worktree ${extras.worktree}`;
  const unamend =
    locked ?? notCurrent ?? ((extras?.version ?? 1) > 1 ? undefined : "No earlier version to restore");
  const uncommit =
    locked ?? notCurrent ?? (c.empty ? "Nothing to uncommit: the change is empty" : undefined);
  return [
    gate("checkout", "Check out", checkout),
    gate("restack", "Restack", locked ?? (c.needsRestack ? undefined : "Nothing to restack")),
    gate("uncommit", "Uncommit", uncommit),
    gate("unamend", "Unamend", unamend),
    gate("abandon", "Abandon", locked),
    hidden ? gate("unhide", "Unhide", undefined) : gate("hide", "Hide", landed ? REASON_LANDED : undefined),
  ];
}

export function uncommittedActions(stacks: Stacks, checkedCount: number): readonly ActionState[] {
  const none = checkedCount === 0 ? "Select at least one file" : undefined;
  const target = stacks.stacks.flatMap((s) => s.changes).find((c) => c.current);
  let modify = none;
  if (modify === undefined) {
    if (target === undefined) modify = "Check out a change to modify";
    else if (target.frozen) modify = REASON_FROZEN;
    else if (isLanded(target)) modify = REASON_LANDED;
  }
  return [gate("create", "Create change", none), gate("modify", "Modify change", modify)];
}

function changeBadges(c: StackChange, extras: ChangeExtras | undefined, hidden: boolean): Badge[] {
  const out: Badge[] = [];
  const r = c.review;
  if (r) {
    out.push({ kind: "pr", text: `#${r.number}${r.draft ? " draft" : ""} ${r.state}`.trim() });
    if (r.checks) out.push({ kind: "checks", text: `checks ${r.checks}` });
  }
  if (isLanded(c)) out.push({ kind: "landed", text: "landed" });
  if (c.needsRestack) out.push({ kind: "restack", text: "needs restack" });
  if (extras?.divergent) out.push({ kind: "divergent", text: "divergent" });
  if (c.frozen) out.push({ kind: "frozen", text: "frozen" });
  if (extras?.worktree !== undefined) out.push({ kind: "worktree", text: extras.worktree });
  const version = extras?.version ?? 1;
  if (version > 1) out.push({ kind: "version", text: `v${version}` });
  if (hidden) out.push({ kind: "hidden", text: "hidden" });
  return out;
}

/** Rows top to bottom: the uncommitted node, each stack tip first, then cloud drafts. */
export function buildRows(input: SmartlogInput): SmartlogRow[] {
  const rows: SmartlogRow[] = [];
  const dirty = input.uncommitted.length > 0;
  if (dirty) {
    rows.push({
      kind: "uncommitted",
      id: UNCOMMITTED_ID,
      files: input.uncommitted.map((f) => ({ ...f, checked: input.checked.has(f.path) })),
      actions: uncommittedActions(input.stacks, input.checked.size),
    });
  }
  for (const stack of input.stacks.stacks) {
    const forks = stack.changes[0]?.parent !== undefined;
    for (const c of stack.changes.toReversed()) {
      const hidden = input.hidden.has(c.id);
      if (hidden && !input.showHidden) continue;
      const extras = input.extras[c.id];
      rows.push({
        kind: "change",
        id: c.id,
        subject: c.subject,
        depth: forks ? 1 : 0,
        current: c.current,
        branches: c.branches,
        badges: changeBadges(c, extras, hidden),
        hidden,
        frozen: c.frozen,
        landed: isLanded(c),
        actions: changeActions(c, extras, hidden, dirty),
      });
    }
  }
  if (input.showCloud) {
    for (const d of input.cloudDrafts) {
      rows.push({ kind: "cloud", id: d.id, subject: d.subject, machine: d.machine });
    }
  }
  return rows;
}

/** Why `drag` cannot be dropped on `onto`, or undefined when the move can apply. */
export function dropBlockedReason(
  stacks: Stacks,
  drag: string,
  onto: MoveTarget,
): string | undefined {
  const r = applyPreview(stacks, { kind: "move", change: drag, onto });
  return r.ok ? undefined : r.reason;
}

export type MovePrediction =
  | { readonly ok: true; readonly conflicts: number }
  | { readonly ok: false; readonly error: string };

export function predictionText(p: MovePrediction | "pending"): string {
  if (p === "pending") return "checking conflicts";
  if (!p.ok) return `cannot predict: ${p.error}`;
  if (p.conflicts === 0) return "no conflicts";
  return `${p.conflicts} predicted conflict${p.conflicts === 1 ? "" : "s"}`;
}

/** A `Bridge.Run` call: the verb and its flags then positionals. The bridge validates the schema
 * and inserts `--end-of-options` itself (SPEC §24.4.5.1), so none is added here. */
export interface RunCall {
  readonly verb: string;
  readonly args: readonly string[];
}

/** Verbs the smartlog runs. */
export type VerbCall =
  | { readonly verb: "switch" | "restack" | "abandon" | "hide" | "unhide"; readonly change: string }
  | { readonly verb: "uncommit" | "unamend" }
  | { readonly verb: "create"; readonly files: readonly string[] }
  | { readonly verb: "modify"; readonly into?: string; readonly files: readonly string[] }
  | {
      readonly verb: "move";
      readonly change: string;
      readonly onto: MoveTarget;
      readonly dryRun: boolean;
    };

export function runCall(call: VerbCall): RunCall {
  switch (call.verb) {
    case "switch":
    case "restack":
    case "abandon":
    case "hide":
    case "unhide":
      return { verb: call.verb, args: [call.change] };
    case "uncommit":
    case "unamend":
      return { verb: call.verb, args: [] };
    case "create":
      return { verb: "create", args: [...call.files] };
    case "modify":
      return {
        verb: "modify",
        args: [...(call.into === undefined ? [] : ["--into", call.into]), ...call.files],
      };
    case "move":
      return {
        verb: "move",
        args: [
          "--source",
          call.change,
          "--onto",
          call.onto.kind === "change" ? call.onto.id : call.onto.name,
          ...(call.dryRun ? ["-n"] : []),
        ],
      };
  }
}
