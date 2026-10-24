// The optimistic operations queue (SPEC §24.1 IP-D10, §24.4.12). One queue per workspace root.
// Lifecycle: queued -> running -> optimistic (the verb finished, waiting for `Watch`) -> done.
// The predicted model is the confirmed model plus the previews of every live op, in order.
import type { Stacks } from "../state/model";
import { describeMutation, type Mutation } from "./mutations";
import { applyPreview } from "./preview";

/** Caps of SPEC §24.4.12. */
export const MAX_QUEUED = 50;
export const MAX_RUNNING = 1;
export const MAX_FINISHED = 20;
/** Optimistic ops wait for `Watch`; if it never comes the oldest are retired at this bound. */
export const MAX_OPTIMISTIC = 50;

export const QUEUE_FULL_MESSAGE = "Wait for the running operations";

export type OpStatus = "queued" | "running" | "optimistic" | "done" | "failed" | "cancelled";

export interface Op {
  readonly id: number;
  readonly mutation: Mutation;
  readonly label: string;
  readonly status: OpStatus;
  /** Why the op failed or was cancelled. */
  readonly reason?: string;
}

/** What the host-side runner reports for one op: a `Bridge.Run` result reduced to ok or a message. */
export type RunOutcome = { readonly ok: true } | { readonly ok: false; readonly error: string };
export type OpRunner = (mutation: Mutation, signal: AbortSignal) => Promise<RunOutcome>;

export type EnqueueResult =
  | { readonly ok: true; readonly op: Op }
  | { readonly ok: false; readonly reason: string };

export interface QueueSnapshot {
  readonly root: string;
  readonly queued: readonly Op[];
  readonly running: Op | undefined;
  readonly optimistic: readonly Op[];
  /** Newest first, at most `MAX_FINISHED`. */
  readonly finished: readonly Op[];
}

export interface OpsQueueOptions {
  readonly root: string;
  readonly run: OpRunner;
  readonly confirmed?: Stacks;
  readonly onChange?: (snapshot: QueueSnapshot, predicted: Stacks) => void;
}

const EMPTY: Stacks = { version: 0, trunks: [], stacks: [] };

export class OpsQueue {
  readonly root: string;
  #run: OpRunner;
  #confirmed: Stacks;
  #nextId = 1;
  #queued: Op[] = [];
  #running: Op | undefined;
  #optimistic: Op[] = [];
  #finished: Op[] = [];
  #abort: AbortController | undefined;
  #onChange: OpsQueueOptions["onChange"];
  #disposed = false;

  constructor(options: OpsQueueOptions) {
    this.root = options.root;
    this.#run = options.run;
    this.#confirmed = options.confirmed ?? EMPTY;
    this.#onChange = options.onChange;
  }

  get confirmed(): Stacks {
    return this.#confirmed;
  }

  snapshot(): QueueSnapshot {
    return {
      root: this.root,
      queued: [...this.#queued],
      running: this.#running,
      optimistic: [...this.#optimistic],
      finished: [...this.#finished],
    };
  }

  /** The confirmed model with every live op's preview applied in order. */
  predicted(): Stacks {
    let model = this.#confirmed;
    for (const op of this.#liveOps()) {
      const r = applyPreview(model, op.mutation);
      if (r.ok) model = r.model;
    }
    return model;
  }

  /** Enqueue a mutation. Refused when the queue is full or the preview does not apply now. */
  enqueue(mutation: Mutation): EnqueueResult {
    if (this.#disposed) return { ok: false, reason: "The queue is closed" };
    if (this.#queued.length >= MAX_QUEUED) return { ok: false, reason: QUEUE_FULL_MESSAGE };
    const preview = applyPreview(this.predicted(), mutation);
    if (!preview.ok) return { ok: false, reason: preview.reason };
    const op: Op = {
      id: this.#nextId++,
      mutation,
      label: describeMutation(mutation),
      status: "queued",
    };
    this.#queued.push(op);
    this.#pump();
    this.#emit();
    return { ok: true, op };
  }

  /** A fresh `Workspace.Stacks` model after a `Watch` event. Retires confirmed ops, cancels stale ones. */
  confirm(model: Stacks): void {
    if (this.#disposed) return;
    this.#confirmed = model;
    // Everything that already ran is now reflected in the server model.
    for (const op of this.#optimistic) this.#finish({ ...op, status: "done" });
    this.#optimistic = [];
    // Revalidate the waiting ops in order against what will exist once the running op lands.
    let rolling = model;
    if (this.#running) {
      const r = applyPreview(rolling, this.#running.mutation);
      if (r.ok) rolling = r.model;
    }
    const kept: Op[] = [];
    for (const op of this.#queued) {
      const r = applyPreview(rolling, op.mutation);
      if (r.ok) {
        rolling = r.model;
        kept.push(op);
      } else {
        this.#finish({
          ...op,
          status: "cancelled",
          reason: `Cancelled: the stack changed (${r.reason})`,
        });
      }
    }
    this.#queued = kept;
    this.#emit();
  }

  /** Cancel a queued op. A running op is aborted through its signal. */
  cancel(id: number): boolean {
    const i = this.#queued.findIndex((o) => o.id === id);
    const queued = this.#queued[i];
    if (queued) {
      this.#queued.splice(i, 1);
      this.#finish({ ...queued, status: "cancelled", reason: "Cancelled" });
      this.#emit();
      return true;
    }
    if (this.#running?.id === id) {
      this.#abort?.abort();
      return true;
    }
    return false;
  }

  dispose(): void {
    this.#disposed = true;
    this.#abort?.abort();
    for (const op of this.#queued) this.#finish({ ...op, status: "cancelled", reason: "Closed" });
    this.#queued = [];
    this.#emit();
  }

  #liveOps(): Op[] {
    return [...this.#optimistic, ...(this.#running ? [this.#running] : []), ...this.#queued];
  }

  #finish(op: Op): void {
    this.#finished.unshift(op);
    if (this.#finished.length > MAX_FINISHED) this.#finished.length = MAX_FINISHED;
  }

  #pump(): void {
    if (this.#running || this.#disposed) return;
    const next = this.#queued.shift();
    if (!next) return;
    const running: Op = { ...next, status: "running" };
    this.#running = running;
    const abort = new AbortController();
    this.#abort = abort;
    void this.#execute(running, abort);
  }

  async #execute(op: Op, abort: AbortController): Promise<void> {
    let outcome: RunOutcome;
    try {
      outcome = await this.#run(op.mutation, abort.signal);
    } catch (e) {
      outcome = { ok: false, error: e instanceof Error ? e.message : "The operation failed" };
    }
    this.#running = undefined;
    this.#abort = undefined;
    if (outcome.ok && !abort.signal.aborted) {
      this.#optimistic.push({ ...op, status: "optimistic" });
      while (this.#optimistic.length > MAX_OPTIMISTIC) {
        const oldest = this.#optimistic.shift();
        if (oldest) this.#finish({ ...oldest, status: "done" });
      }
    } else if (abort.signal.aborted) {
      this.#finish({ ...op, status: "cancelled", reason: "Cancelled" });
    } else if (!outcome.ok) {
      this.#finish({ ...op, status: "failed", reason: outcome.error });
    }
    this.#pump();
    this.#emit();
  }

  #emit(): void {
    this.#onChange?.(this.snapshot(), this.predicted());
  }
}
