// A small query cache: one in-flight run per key, stale-while-revalidate, streams that publish
// partial data, and an LRU bound. Keys are strings built by `keyOf`; content-addressed reads pass
// `staleMs: Infinity` and are never refetched (docs/WEB-UI.md section 4).
import { createContext, useContext, useEffect, useSyncExternalStore } from "react";

export type Status = "pending" | "ready" | "error";

/** An immutable snapshot; a new object is made on every change so React can compare by identity. */
export interface Entry<T> {
  readonly status: Status;
  /** Latest data. For a stream this grows until `done`. */
  readonly data: T | undefined;
  readonly error: unknown;
  /** The run has finished (always true for a unary call once ready). */
  readonly done: boolean;
  /** A run is in flight (initial load, stream, or background revalidation). */
  readonly fetching: boolean;
  readonly updatedAt: number;
}

export interface RunContext<T> {
  signal: AbortSignal;
  /** Publish partial data. Calls are coalesced into one notification per scheduler tick. */
  emit(data: T): void;
}

/** Resolves with the final data. A unary call just returns; a stream calls `emit` as it goes. */
export type Runner<T> = (ctx: RunContext<T>) => Promise<T>;

export interface QueryOptions {
  /** Milliseconds a result stays fresh. `Infinity` for content-addressed reads. Default 10 s. */
  staleMs?: number;
  /** Abort the run when the last component using the key unmounts (streams). */
  abortOnRelease?: boolean;
}

const DEFAULT_STALE_MS = 10_000;
const MAX_ENTRIES = 400;

export function keyOf(...parts: readonly unknown[]): string {
  return JSON.stringify(parts);
}

const PENDING: Entry<never> = {
  status: "pending",
  data: undefined,
  error: undefined,
  done: false,
  fetching: false,
  updatedAt: 0,
};

interface Slot {
  entry: Entry<unknown>;
  runner: Runner<unknown>;
  staleMs: number;
  abortOnRelease: boolean;
  subs: Set<() => void>;
  retained: number;
  controller: AbortController | undefined;
  promise: Promise<unknown> | undefined;
}

export interface CacheOptions {
  now?: () => number;
  schedule?: (fn: () => void) => void;
  max?: number;
}

export class QueryCache {
  private readonly slots = new Map<string, Slot>();
  private readonly now: () => number;
  private readonly schedule: (fn: () => void) => void;
  private readonly max: number;

  constructor(opts: CacheOptions = {}) {
    this.now = opts.now ?? Date.now;
    this.schedule = opts.schedule ?? ((fn) => queueMicrotask(fn));
    this.max = opts.max ?? MAX_ENTRIES;
  }

  get size(): number {
    return this.slots.size;
  }

  /** The current snapshot without starting anything. */
  peek<T>(key: string): Entry<T> {
    return (this.slots.get(key)?.entry as Entry<T> | undefined) ?? (PENDING as Entry<T>);
  }

  /**
   * The snapshot for `key`, starting a run if there is none or it is stale. Safe to call from a
   * render: the run starts on a microtask, so nothing is notified synchronously.
   */
  ensure<T>(key: string, runner: Runner<T>, opts: QueryOptions = {}): Entry<T> {
    const existing = this.slots.get(key);
    if (!existing) {
      const slot: Slot = {
        entry: { ...PENDING, fetching: true },
        runner: runner as Runner<unknown>,
        staleMs: opts.staleMs ?? DEFAULT_STALE_MS,
        abortOnRelease: opts.abortOnRelease ?? false,
        subs: new Set(),
        retained: 0,
        controller: undefined,
        promise: undefined,
      };
      this.slots.set(key, slot);
      this.begin(slot);
      this.evict();
      return slot.entry as Entry<T>;
    }
    existing.runner = runner as Runner<unknown>;
    // Refresh recency.
    this.slots.delete(key);
    this.slots.set(key, existing);
    const stale = this.now() - existing.entry.updatedAt > existing.staleMs;
    if (!existing.controller && (existing.entry.status === "pending" || stale)) {
      existing.entry = { ...existing.entry, fetching: true };
      this.begin(existing);
    }
    return existing.entry as Entry<T>;
  }

  /** Warms the cache without a subscriber. Resolves when the run has finished. */
  async prefetch<T>(key: string, runner: Runner<T>, opts?: QueryOptions): Promise<void> {
    this.ensure(key, runner, opts);
    await this.slots.get(key)?.promise?.catch(() => undefined);
  }

  /** Resolves with the data, running the call if needed; rejects with the run's error. */
  async fetch<T>(key: string, runner: Runner<T>, opts?: QueryOptions): Promise<T> {
    this.ensure(key, runner, opts);
    await this.slots.get(key)?.promise?.catch(() => undefined);
    const e = this.peek<T>(key);
    if (e.status === "error") throw e.error;
    return e.data as T;
  }

  /** Marks every key starting with `prefix` stale; mounted ones refetch now. */
  invalidate(prefix = ""): void {
    for (const [key, slot] of this.slots) {
      if (!key.startsWith(prefix)) continue;
      slot.entry = { ...slot.entry, updatedAt: 0 };
      if (slot.subs.size > 0 && !slot.controller) {
        slot.entry = { ...slot.entry, fetching: true };
        this.begin(slot);
        this.emitChange(slot);
      }
    }
  }

  clear(): void {
    for (const slot of this.slots.values()) slot.controller?.abort();
    this.slots.clear();
  }

  subscribe(key: string, cb: () => void): () => void {
    const slot = this.slots.get(key);
    if (!slot) return () => undefined;
    slot.subs.add(cb);
    slot.retained += 1;
    return () => {
      slot.subs.delete(cb);
      slot.retained -= 1;
      if (slot.retained === 0 && slot.abortOnRelease && slot.controller) {
        slot.controller.abort();
        slot.controller = undefined;
        if (this.slots.get(key) === slot) this.slots.delete(key);
      }
    };
  }

  private emitChange(slot: Slot): void {
    for (const cb of slot.subs) cb();
  }

  private begin(slot: Slot): void {
    const controller = new AbortController();
    slot.controller = controller;
    let queued: unknown;
    let hasQueued = false;
    const flush = () => {
      if (!hasQueued || controller.signal.aborted) return;
      hasQueued = false;
      slot.entry = { ...slot.entry, status: "ready", data: queued, updatedAt: this.now() };
      this.emitChange(slot);
    };
    const emit = (data: unknown) => {
      queued = data;
      if (hasQueued) return;
      hasQueued = true;
      this.schedule(flush);
    };
    slot.promise = Promise.resolve()
      .then(() => slot.runner({ signal: controller.signal, emit }))
      .then(
        (data) => {
          if (controller.signal.aborted) return;
          hasQueued = false;
          slot.entry = {
            status: "ready",
            data,
            error: undefined,
            done: true,
            fetching: false,
            updatedAt: this.now(),
          };
        },
        (error: unknown) => {
          if (controller.signal.aborted) return;
          hasQueued = false;
          slot.entry = { ...slot.entry, status: "error", error, done: true, fetching: false };
        },
      )
      .then(() => {
        if (controller.signal.aborted) return;
        if (slot.controller === controller) slot.controller = undefined;
        this.emitChange(slot);
      });
  }

  private evict(): void {
    if (this.slots.size <= this.max) return;
    for (const [key, slot] of this.slots) {
      if (this.slots.size <= this.max) return;
      if (slot.retained === 0 && !slot.controller) this.slots.delete(key);
    }
  }
}

const CacheContext = createContext<QueryCache | null>(null);
export const CacheProvider = CacheContext.Provider;

export function useCache(): QueryCache {
  const cache = useContext(CacheContext);
  if (!cache) throw new Error("CacheProvider is missing");
  return cache;
}

/** Subscribes to one key, starting the run on first use. */
export function useQuery<T>(key: string, runner: Runner<T>, opts?: QueryOptions): Entry<T> {
  const cache = useCache();
  const entry = useSyncExternalStore(
    (cb) => cache.subscribe(key, cb),
    () => cache.ensure(key, runner, opts),
    () => cache.peek<T>(key),
  );
  // Revalidate when the window regains focus and the data is stale.
  useEffect(() => {
    const onFocus = () => cache.ensure(key, runner, opts);
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  });
  return entry;
}
