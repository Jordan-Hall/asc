// Stack navigation (SPEC 24.3.2): `up`, `down`, `top` and `bottom` through the allowlisted
// `Bridge.Run`. At a fork the verb exits 2 with `OpResult.candidates`; the person picks one and the
// verb runs again with `--to=<change>`. Editor-free; `registerStackNav` is the only `vscode` user.
// TODO(C09): the bridge allowlist for `up`/`down` has no `--to` flag yet, so the second run is
// refused there until the allowlist adds `--to` (`rev`) to up, down, top and bottom.
import type * as vscodeTypes from "vscode";
import type { CommandResult, CommandsApi, CommandsUi } from "../scm/commands";
import { VerbRunner } from "../scm/statusbar";

export type NavVerb = "up" | "down" | "top" | "bottom";

/** Allowlist limit for the step count of `up` and `down`. */
export const NAV_MAX_STEPS = 100;

export interface Candidate {
  change: string;
  subject: string;
}

export interface NavUi extends CommandsUi {
  /** A quick pick of the fork candidates; resolves the chosen change, or undefined when dismissed. */
  pickCandidate(candidates: readonly Candidate[]): Promise<string | undefined>;
}

/** A change id from `asc` output never reads as a flag and holds no whitespace or control character. */
export function validChange(s: string): boolean {
  if (s === "" || s.length > 256 || s.startsWith("-")) return false;
  for (const ch of s) {
    const c = ch.charCodeAt(0);
    if (c < 0x21 || c === 0x7f) return false;
  }
  return true;
}

/**
 * Reads `candidates` from the encoded `OpResult`. TODO(M02): `cli.proto` is not in the descriptor set,
 * so the bytes are read as the JSON the verb printed. Anything malformed gives `[]`.
 */
export function parseCandidates(bytes: Uint8Array | null): Candidate[] {
  if (bytes === null || bytes.byteLength === 0) return [];
  let v: unknown;
  try {
    v = JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    return [];
  }
  const list = (v as { candidates?: unknown } | null)?.candidates;
  if (!Array.isArray(list)) return [];
  const out: Candidate[] = [];
  for (const e of list) {
    if (typeof e !== "object" || e === null) continue;
    const o = e as Record<string, unknown>;
    if (typeof o.change !== "string" || !validChange(o.change)) continue;
    out.push({ change: o.change, subject: typeof o.subject === "string" ? o.subject : "" });
  }
  return out;
}

export class StackNav {
  private readonly r: VerbRunner;

  constructor(
    api: CommandsApi,
    private readonly ui: NavUi,
  ) {
    this.r = new VerbRunner(api, ui);
  }

  /** Moves `steps` (`up` and `down` only; 1 to 100) along the stack, asking at a fork. */
  go(root: string, verb: NavVerb, steps?: number): Promise<CommandResult> {
    return this.r.serial(root, async () => {
      const stepped = verb === "up" || verb === "down";
      if (steps !== undefined && (!stepped || !Number.isInteger(steps) || steps < 1 || steps > NAV_MAX_STEPS)) {
        this.ui.warn("asc: the step count must be a whole number from 1 to 100.");
        return { ok: false, reason: "invalid" };
      }
      if (!(await this.r.gate())) return { ok: false, reason: "disabled" };
      const base = steps === undefined || steps === 1 ? [] : [String(steps)];
      const first = await this.r.run(root, verb, base);
      if (first.exit !== 2) return this.r.outcome(verb, first);
      const cands = parseCandidates(first.opResult);
      if (cands.length === 0) return this.r.outcome(verb, first);
      const to = await this.ui.pickCandidate(cands);
      // Only a change the verb itself offered can be passed back.
      if (to === undefined || !cands.some((c) => c.change === to)) return { ok: false, reason: "cancelled" };
      return this.r.outcome(verb, await this.r.run(root, verb, [...base, `--to=${to}`]));
    });
  }
}

/** Registers `asc.stackUp`, `asc.stackDown`, `asc.stackTop`, `asc.stackBottom`. Keybindings live in `package.json`. */
export function registerStackNav(
  vscode: typeof vscodeTypes,
  api: CommandsApi,
  ui: CommandsUi,
  roots: readonly string[],
  after: (root: string) => void,
): { dispose(): void } {
  const nav = new StackNav(api, {
    ...ui,
    async pickCandidate(cands) {
      const items = cands.map((c) => ({ label: c.subject || c.change, description: c.change }));
      const hit = await vscode.window.showQuickPick(items, { placeHolder: "This stack forks. Choose a branch." });
      return hit?.description;
    },
  });
  const subs: { dispose(): unknown }[] = [];
  const pick = async (a: unknown): Promise<string | undefined> => {
    if (typeof a === "string" && roots.includes(a)) return a;
    if (roots.length === 1) return roots[0];
    return vscode.window.showQuickPick([...roots], { placeHolder: "Choose an asc root" });
  };
  const verbs: [string, NavVerb][] = [
    ["asc.stackUp", "up"],
    ["asc.stackDown", "down"],
    ["asc.stackTop", "top"],
    ["asc.stackBottom", "bottom"],
  ];
  for (const [id, verb] of verbs) {
    subs.push(
      vscode.commands.registerCommand(id, async (a?: unknown) => {
        const root = await pick(a);
        if (!root) return;
        await nav.go(root, verb);
        after(root);
      }),
    );
  }
  return {
    dispose() {
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
