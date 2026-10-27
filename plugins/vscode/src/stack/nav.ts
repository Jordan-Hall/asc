// Stack navigation (SPEC 24.3.2): `up`, `down`, `top` and `bottom` through the allowlisted
// `Bridge.Run`. At a fork the verb exits 2 with `OpResult.candidates` and moves nothing; the person picks
// one, the plugin moves there with `switch --detach <change>` (the move `up` itself makes) and the verb
// runs again for the steps that are left, so the allowlist needs no `--to` flag. Editor-free;
// `registerStackNav` is the only `vscode` user.
import type * as vscodeTypes from "vscode";
import { OpStatus } from "@asc/asc-client/cli";
import { changeFull, decodeOpResult } from "../opresult";
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

/** Reads `candidates` from the encoded `OpResult`. A change that is not a plain id is dropped. */
export function parseCandidates(bytes: Uint8Array | null): Candidate[] {
  const r = decodeOpResult(bytes);
  if (r === null) return [];
  const out: Candidate[] = [];
  for (const c of r.candidates) {
    const change = changeFull(c.change);
    if (change === null || !validChange(change)) continue;
    out.push({ change, subject: new TextDecoder().decode(c.subject) });
  }
  return out;
}

/** Most forks one command walks through before it stops. */
export const MAX_FORKS = 16;

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
      if (
        steps !== undefined &&
        (!stepped || !Number.isInteger(steps) || steps < 1 || steps > NAV_MAX_STEPS)
      ) {
        this.ui.warn("ASC: the step count must be a whole number from 1 to 100.");
        return { ok: false, reason: "invalid" };
      }
      if (!(await this.r.gate())) return { ok: false, reason: "disabled" };
      // `down` and `bottom` never fork, so one run does them. `up N` goes one step at a time, because a
      // fork reports the children of the node it stopped at and not how far the walk had come; `top`
      // goes on from the chosen child with a new run.
      if (!forks(verb)) {
        const args = steps === undefined || steps === 1 ? [] : [String(steps)];
        return this.r.outcome(verb, await this.r.run(root, verb, args));
      }
      let left = verb === "up" ? (steps ?? 1) : 1;
      let forked = 0;
      for (;;) {
        const res = await this.r.run(root, verb, []);
        if (res.exit !== 2) {
          const last = this.r.outcome(verb, res);
          if (!last.ok) return last;
          if (decodeOpResult(res.opResult)?.status === OpStatus.NOTHING) {
            return { ok: true, nothing: true };
          }
          if (verb === "top" || --left < 1) return last;
          continue;
        }
        if (++forked > MAX_FORKS) break;
        const cands = parseCandidates(res.opResult);
        if (cands.length === 0) return this.r.outcome(verb, res);
        const to = await this.ui.pickCandidate(cands);
        // Only a change the verb itself offered can be passed back.
        if (to === undefined || !cands.some((c) => c.change === to)) {
          return { ok: false, reason: "cancelled" };
        }
        const moved = await this.r.run(root, "switch", ["--detach", to]);
        if (moved.exit !== 0) return this.r.outcome("switch", moved);
        if (verb === "up" && --left < 1) return { ok: true };
      }
      this.ui.warn("ASC: the stack forks too often to follow; stopped.");
      return { ok: false, reason: "ambiguous" };
    });
  }
}

function forks(verb: NavVerb): boolean {
  return verb === "up" || verb === "top";
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
      const hit = await vscode.window.showQuickPick(items, {
        placeHolder: "This stack forks. Choose a branch.",
      });
      return hit?.description;
    },
  });
  const subs: { dispose(): unknown }[] = [];
  const pick = async (a: unknown): Promise<string | undefined> => {
    if (typeof a === "string" && roots.includes(a)) return a;
    if (roots.length === 1) return roots[0];
    return vscode.window.showQuickPick([...roots], { placeHolder: "Choose an ASC root" });
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
