// GitHub-style shortcuts: single keys and two-key chords (g then c). The state machine is pure
// (`ChordMachine`) so it is tested without a DOM; `useShortcuts` wires it to keydown.
import { useEffect, useRef } from "react";

export interface Binding {
  /** `["g", "c"]` is the chord g then c. */
  keys: readonly string[];
  id: string;
  label: string;
}

export const CHORD_TIMEOUT_MS = 1500;

export interface FeedResult {
  /** The action to run, if the keys completed a binding. */
  action: string | undefined;
  /** Waiting for the next key of a chord. */
  pending: boolean;
}

export class ChordMachine {
  private buffer: string[] = [];
  private last = 0;

  constructor(
    private readonly bindings: readonly Binding[],
    private readonly timeoutMs = CHORD_TIMEOUT_MS,
  ) {}

  feed(key: string, now: number): FeedResult {
    if (this.buffer.length > 0 && now - this.last > this.timeoutMs) this.buffer = [];
    this.last = now;
    for (const attempt of [[...this.buffer, key], [key]]) {
      const exact = this.bindings.find(
        (b) => b.keys.length === attempt.length && b.keys.every((k, i) => k === attempt[i]),
      );
      if (exact) {
        this.buffer = [];
        return { action: exact.id, pending: false };
      }
      const prefix = this.bindings.some(
        (b) => b.keys.length > attempt.length && attempt.every((k, i) => k === b.keys[i]),
      );
      if (prefix) {
        this.buffer = attempt;
        return { action: undefined, pending: true };
      }
    }
    this.buffer = [];
    return { action: undefined, pending: false };
  }

  reset(): void {
    this.buffer = [];
  }
}

/** True when the key press belongs to a text field and must not trigger a shortcut. */
export function typingTarget(target: EventTarget | null): boolean {
  const el = target as { tagName?: string; isContentEditable?: boolean } | null;
  if (!el?.tagName) return false;
  const tag = el.tagName.toLowerCase();
  return tag === "input" || tag === "textarea" || tag === "select" || el.isContentEditable === true;
}

export const BINDINGS: readonly Binding[] = [
  { keys: ["?"], id: "help", label: "Show keyboard shortcuts" },
  { keys: ["/"], id: "search", label: "Focus search" },
  { keys: ["t"], id: "find-file", label: "Go to file" },
  { keys: ["g", "h"], id: "go-home", label: "Go to dashboard" },
  { keys: ["g", "c"], id: "go-code", label: "Go to code" },
  { keys: ["g", "p"], id: "go-changes", label: "Go to changes" },
  { keys: ["g", "a"], id: "go-actions", label: "Go to actions" },
  { keys: ["g", "i"], id: "go-inbox", label: "Go to inbox" },
  { keys: ["g", "s"], id: "go-settings", label: "Go to settings" },
  { keys: ["y"], id: "permalink", label: "Permalink to this commit" },
];

export function useShortcuts(bindings: readonly Binding[], run: (id: string) => void): void {
  const runRef = useRef(run);
  runRef.current = run;
  useEffect(() => {
    const machine = new ChordMachine(bindings);
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey || e.metaKey || e.altKey || e.defaultPrevented) return;
      if (typingTarget(e.target) || e.key.length > 1) return;
      const r = machine.feed(e.key, e.timeStamp);
      if (r.action) {
        e.preventDefault();
        runRef.current(r.action);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [bindings]);
}
