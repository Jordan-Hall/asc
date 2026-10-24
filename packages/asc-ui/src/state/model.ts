// The `Stacks` model: a plain, immutable view of `Workspace.Stacks` (proto `WsStacksResponse`).
// Ids are strings (change ids in their text form) so the model is serialisable and comparable.

export interface ReviewInfo {
  readonly forge: string;
  readonly number: number;
  readonly url: string;
  readonly state: string;
  readonly draft: boolean;
  readonly approvals: number;
  readonly checks: string;
}

export interface StackChange {
  readonly id: string;
  /** Parent change id, always set by the proto adapter and the appliers; absent when the parent is the trunk. */
  readonly parent?: string;
  readonly subject: string;
  readonly branches: readonly string[];
  readonly state: string;
  readonly needsRestack: boolean;
  readonly frozen: boolean;
  readonly empty: boolean;
  readonly current: boolean;
  readonly review?: ReviewInfo;
}

export interface Stack {
  readonly trunk: string;
  /** Trunk tip the stack is based on (hex). */
  readonly base: string;
  readonly behind: number;
  /** Ordered base to tip. The first entry's `parent` is set when the stack forks off another stack. */
  readonly changes: readonly StackChange[];
}

export interface TrunkInfo {
  readonly name: string;
  readonly tip: string;
  readonly remote: string;
}

export interface Stacks {
  readonly version: number;
  readonly trunks: readonly TrunkInfo[];
  readonly stacks: readonly Stack[];
}

export const EMPTY_STACKS: Stacks = { version: 0, trunks: [], stacks: [] };

export interface Located {
  readonly stackIndex: number;
  readonly index: number;
  readonly change: StackChange;
}

export function locate(model: Stacks, id: string): Located | undefined {
  for (let stackIndex = 0; stackIndex < model.stacks.length; stackIndex++) {
    const stack = model.stacks[stackIndex];
    const index = stack?.changes.findIndex((c) => c.id === id) ?? -1;
    const change = stack?.changes[index];
    if (change) return { stackIndex, index, change };
  }
  return undefined;
}

/** True when `id` is `ancestor` or sits above it, through parent links and stack forks. */
export function isDescendantOrSelf(model: Stacks, id: string, ancestor: string): boolean {
  let cursor: string | undefined = id;
  for (let hops = 0; cursor !== undefined && hops <= 100_000; hops++) {
    if (cursor === ancestor) return true;
    cursor = parentOf(model, cursor);
  }
  return false;
}

export function parentOf(model: Stacks, id: string): string | undefined {
  return locate(model, id)?.change.parent;
}
