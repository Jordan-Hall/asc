// Stack editor model (SPEC §24.3.2): pure state and rules for reorder, fold, drop and reword,
// committed as one `asc reorder --todo` operation (P03). No DOM, so the rules are unit-tested.
import type { Stack } from "../../state/model";

export type TodoAction = "pick" | "fold" | "drop" | "reword";

/** Wire shape of `StackTodo` (proto3 JSON, `crates/asc/src/stack/cli/reorder_todo.rs`), bottom to top. */
export interface TodoItem {
  readonly action: TodoAction;
  readonly change: string;
  readonly message?: string;
}
export interface StackTodo {
  readonly items: readonly TodoItem[];
}

export const MAX_ITEMS = 4096;
export const MAX_MESSAGE = 64 * 1024;
/** `reorder_todo` refuses id prefixes shorter than this; the editor always sends full ids. */
export const MIN_ID = 4;

export interface EditorItem {
  readonly id: string;
  readonly subject: string;
  readonly frozen: boolean;
  readonly action: TodoAction;
  /** Reword text (required for `reword`) or an optional replacement message for `fold`. */
  readonly message: string;
}

export interface EditorState {
  /** Bottom to top, as the todo is. */
  readonly items: readonly EditorItem[];
  readonly original: readonly string[];
}

export function initEditor(stack: Stack): EditorState {
  const items = stack.changes.map<EditorItem>((c) => ({
    id: c.id,
    subject: c.subject,
    frozen: c.frozen,
    action: "pick",
    message: c.subject,
  }));
  return { items, original: items.map((i) => i.id) };
}

const replaceAt = (items: readonly EditorItem[], i: number, item: EditorItem): EditorItem[] =>
  items.map((x, k) => (k === i ? item : x));

/** Why `action` cannot be set on `id`; undefined when it can. */
export function actionBlockedReason(s: EditorState, id: string, action: TodoAction): string | undefined {
  const i = s.items.findIndex((x) => x.id === id);
  const it = s.items[i];
  if (!it) return "Unknown change";
  if (action === "pick") return undefined;
  if (it.frozen) return "Frozen";
  if (action === "fold") {
    const below = s.items.slice(0, i).some((x) => x.action !== "drop");
    if (!below) return "Nothing below to fold into";
  }
  return undefined;
}

export function setAction(s: EditorState, id: string, action: TodoAction): EditorState {
  const i = s.items.findIndex((x) => x.id === id);
  const it = s.items[i];
  if (!it || actionBlockedReason(s, id, action) !== undefined) return s;
  return fixFolds({ ...s, items: replaceAt(s.items, i, { ...it, action }) });
}

export function setMessage(s: EditorState, id: string, message: string): EditorState {
  const i = s.items.findIndex((x) => x.id === id);
  const it = s.items[i];
  return it ? { ...s, items: replaceAt(s.items, i, { ...it, message }) } : s;
}

/** After an edit an item that folds may have nothing kept below it; it falls back to pick. */
function fixFolds(s: EditorState): EditorState {
  let kept = 0;
  const items = s.items.map((x) => {
    const fixed = x.action === "fold" && kept === 0 ? { ...x, action: "pick" as const } : x;
    if (fixed.action !== "drop") kept++;
    return fixed;
  });
  return { ...s, items };
}

/** Move the item at `from` so it lands at index `to` (both bottom-to-top positions). */
export function moveItem(s: EditorState, from: number, to: number): EditorState {
  if (from === to || from < 0 || to < 0 || from >= s.items.length || to >= s.items.length) return s;
  const items = [...s.items];
  const [it] = items.splice(from, 1);
  if (!it || it.frozen) return s;
  items.splice(to, 0, it);
  return fixFolds({ ...s, items });
}

/** `dir` -1 moves toward the bottom of the stack, +1 toward the tip. */
export function moveBlockedReason(s: EditorState, index: number, dir: -1 | 1): string | undefined {
  const it = s.items[index];
  if (!it) return "Unknown change";
  if (it.frozen) return "Frozen";
  const j = index + dir;
  if (j < 0) return "Already at the bottom";
  if (j >= s.items.length) return "Already at the top";
  if (s.items[j]?.frozen) return "Frozen";
  return undefined;
}

export function isDirty(s: EditorState): boolean {
  return (
    s.items.length !== s.original.length ||
    s.items.some((x, i) => x.id !== s.original[i] || x.action !== "pick")
  );
}

/** Why the todo cannot be committed; undefined when it can. */
export function commitBlockedReason(s: EditorState): string | undefined {
  if (s.items.length === 0) return "The stack is empty";
  if (s.items.length > MAX_ITEMS) return "Too many changes";
  if (!isDirty(s)) return "No edits";
  if (s.items.every((x) => x.action === "drop")) return "Dropping every change: use abandon";
  for (const x of s.items) {
    if (x.id.length < MIN_ID) return "A change id is too short";
    if (x.action === "reword" && x.message.trim() === "") return `Reword of ${x.id} needs a message`;
    if (x.message.length > MAX_MESSAGE) return `The message of ${x.id} is too long`;
    if (x.message.includes("\0")) return `The message of ${x.id} has a NUL byte`;
  }
  return undefined;
}

/** The `StackTodo` for the edited list. Throws when `commitBlockedReason` is set. */
export function toTodo(s: EditorState): StackTodo {
  const why = commitBlockedReason(s);
  if (why !== undefined) throw new Error(why);
  return {
    items: s.items.map<TodoItem>((x) => {
      if (x.action === "reword") return { action: "reword", change: x.id, message: x.message };
      // A fold carries a message only when the user changed it from the subject.
      if (x.action === "fold" && x.message.trim() !== "" && x.message !== x.subject) {
        return { action: "fold", change: x.id, message: x.message };
      }
      return { action: x.action, change: x.id };
    }),
  };
}

/**
 * Argv the host runs through the bridge. The todo travels in a private temp file (the host
 * writes `JSON.stringify(todo)` into `$ASC_PLUGIN_TMP`), so messages never reach argv or logs.
 */
export function reorderArgv(file: string): readonly string[] {
  return ["reorder", "--end-of-options", "--todo", file];
}
