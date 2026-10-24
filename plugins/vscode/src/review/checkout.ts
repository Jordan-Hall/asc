// Stack checkout from the inbox or the review panel (SPEC 24.4.7): runs the allowlisted
// `stack get <pr|change|branch> [--worktree <path>]` through `Bridge.Run`. Typed argv only, no shell,
// nothing secret in it. A plain git remote exits 128 ("needs an asc server"); that is surfaced.
import type { RunOutcome } from "../scm/commands";

export interface CheckoutApi {
  run(root: string, verb: string, args: string[], signal?: AbortSignal): Promise<RunOutcome>;
  canMutate(): Promise<boolean>;
}

export interface CheckoutUi {
  info(message: string): void;
  error(message: string, lines: string[]): void;
}

export type CheckoutTarget =
  | { kind: "pr"; number: number }
  | { kind: "change"; id: string }
  | { kind: "branch"; name: string };

export interface CheckoutOptions {
  /** Opens the stack in a new plain worktree at this path. */
  worktree?: string;
}

export const NEEDS_SERVER = "needs an asc server";

/** Turns a target into one positional argument; null when it is not safe to pass. */
export function targetArg(t: CheckoutTarget): string | null {
  switch (t.kind) {
    case "pr":
      return Number.isSafeInteger(t.number) && t.number > 0 ? String(t.number) : null;
    case "change":
      return /^[k-z]{4,32}$|^[0-9a-f]{4,64}$/.test(t.id) ? t.id : null;
    case "branch":
      // No leading dash (flag injection), no control characters, bounded length.
      return t.name.length > 0 &&
        t.name.length <= 255 &&
        !t.name.startsWith("-") &&
        ![...t.name].some((ch) => ch.charCodeAt(0) < 0x20 || ch.charCodeAt(0) === 0x7f)
        ? t.name
        : null;
  }
}

export function checkoutArgs(t: CheckoutTarget, o: CheckoutOptions = {}): string[] | null {
  const a = targetArg(t);
  if (a === null) return null;
  if (o.worktree === undefined) return [a];
  if (o.worktree === "" || o.worktree.startsWith("-") || o.worktree.includes("\u0000")) return null;
  return ["--worktree", o.worktree, a];
}

export class StackCheckout {
  constructor(
    private readonly api: CheckoutApi,
    private readonly ui: CheckoutUi,
  ) {}

  /** Resolves true when the stack was fetched. */
  async run(
    root: string,
    t: CheckoutTarget,
    o: CheckoutOptions = {},
    signal?: AbortSignal,
  ): Promise<boolean> {
    const args = checkoutArgs(t, o);
    if (args === null) {
      this.ui.error("That is not a valid stack to check out", []);
      return false;
    }
    if (!(await this.api.canMutate())) {
      this.ui.error("Checking out a stack needs a newer asc; update asc", []);
      return false;
    }
    const r = await this.api.run(root, "stack get", args, signal);
    if (r.exit === 0) {
      this.ui.info(
        o.worktree === undefined ? "Stack checked out" : `Stack checked out in ${o.worktree}`,
      );
      return true;
    }
    const tail = r.stderr.slice(-20);
    const needs = tail.some((l) => l.includes(NEEDS_SERVER));
    this.ui.error(
      needs
        ? "Stack checkout needs an asc server (not available on a plain git remote)"
        : "Stack checkout failed",
      tail,
    );
    return false;
  }
}
