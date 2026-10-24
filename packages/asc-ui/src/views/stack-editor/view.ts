// Stack editor DOM view (SPEC §24.3.2). Vanilla DOM, text only via `textContent`. Rows are listed
// tip first (CSS column-reverse); drag or the arrows reorder, a select picks pick/reword/fold/drop,
// and Apply hands one `StackTodo` to the host (one `reorder --todo` op). Disabled controls state
// a reason.
import type { RunOutcome } from "../../ops/queue";
import type { Severity } from "../../platform";
import type { Stack } from "../../state/model";
import {
  actionBlockedReason,
  commitBlockedReason,
  type EditorState,
  initEditor,
  moveBlockedReason,
  moveItem,
  setAction,
  setMessage,
  type StackTodo,
  type TodoAction,
  toTodo,
} from "./model";

export interface StackEditorHost {
  /** Run `reorder --todo` with the todo as one operation. */
  applyTodo(todo: StackTodo): Promise<RunOutcome>;
  notify(severity: Severity, message: string): void;
}

export interface StackEditorView {
  /** Replace the stack (resets the edits). */
  update(stack: Stack): void;
  state(): EditorState;
  dispose(): void;
}

const ACTIONS: readonly TodoAction[] = ["pick", "reword", "fold", "drop"];

export function mountStackEditor(
  container: HTMLElement,
  host: StackEditorHost,
  initial: Stack,
): StackEditorView {
  const doc = container.ownerDocument;
  let stack = initial;
  let state = initEditor(stack);
  let busy = false;
  let error = "";
  let dragFrom = -1;

  const root = doc.createElement("div");
  root.className = "asc-stack-editor";
  container.append(root);

  const gate = (b: HTMLButtonElement, reason: string | undefined): void => {
    b.disabled = reason !== undefined;
    if (reason !== undefined) b.title = reason;
    else b.removeAttribute("title");
  };

  function button(label: string, action: string): HTMLButtonElement {
    const b = doc.createElement("button");
    b.type = "button";
    b.className = "asc-action";
    b.dataset.action = action;
    b.textContent = label;
    return b;
  }

  function paintBar(): void {
    const apply = root.querySelector<HTMLButtonElement>('[data-action="apply"]');
    if (apply) gate(apply, busy ? "Applying" : commitBlockedReason(state));
  }

  function render(): void {
    root.replaceChildren();
    const list = doc.createElement("ol");
    list.className = "asc-se-rows";
    state.items.forEach((it, index) => {
      const li = doc.createElement("li");
      li.className = `asc-se-row asc-se-${it.action}`;
      li.dataset.testid = `se-row-${it.id}`;
      li.draggable = !it.frozen && !busy;
      li.addEventListener("dragstart", () => {
        dragFrom = index;
      });
      li.addEventListener("dragover", (e) => {
        if (dragFrom >= 0 && !it.frozen) {
          e.preventDefault();
          li.classList.add("asc-drop-target");
        }
      });
      li.addEventListener("dragleave", () => li.classList.remove("asc-drop-target"));
      li.addEventListener("drop", (e) => {
        e.preventDefault();
        const from = dragFrom;
        dragFrom = -1;
        state = moveItem(state, from, index);
        render();
      });

      const subject = doc.createElement("span");
      subject.className = "asc-subject";
      subject.textContent = it.subject;
      li.append(subject);

      const sel = doc.createElement("select");
      sel.dataset.testid = "se-action";
      sel.setAttribute("aria-label", `Action for ${it.id}`);
      for (const a of ACTIONS) {
        const o = doc.createElement("option");
        o.value = a;
        o.textContent = a;
        const why = actionBlockedReason(state, it.id, a);
        o.disabled = why !== undefined;
        if (why !== undefined) o.title = why;
        sel.append(o);
      }
      sel.value = it.action;
      sel.disabled = busy;
      sel.addEventListener("change", () => {
        state = setAction(state, it.id, sel.value as TodoAction);
        render();
      });
      li.append(sel);

      // Rows read tip first, so "Up" moves toward the tip (index + 1).
      for (const [dir, label, tid] of [
        [1, "Up", "up"],
        [-1, "Down", "down"],
      ] as const) {
        const b = button(label, tid);
        gate(b, busy ? "Applying" : moveBlockedReason(state, index, dir));
        b.addEventListener("click", () => {
          state = moveItem(state, index, index + dir);
          render();
        });
        li.append(b);
      }

      if (it.action === "reword" || it.action === "fold") {
        const input = doc.createElement("input");
        input.type = "text";
        input.className = "asc-se-msg";
        input.dataset.testid = "se-message";
        input.setAttribute("aria-label", `Message for ${it.id}`);
        input.value = it.message;
        input.addEventListener("input", () => {
          state = setMessage(state, it.id, input.value);
          paintBar();
        });
        li.append(input);
      }
      list.append(li);
    });
    root.append(list);

    const bar = doc.createElement("div");
    bar.className = "asc-se-bar";
    const apply = button("Apply", "apply");
    apply.addEventListener("click", () => void commit());
    const reset = button("Reset", "reset");
    reset.addEventListener("click", () => {
      state = initEditor(stack);
      error = "";
      render();
    });
    bar.append(apply, reset);
    root.append(bar);
    const err = doc.createElement("div");
    err.className = "asc-se-error";
    err.dataset.testid = "se-error";
    err.setAttribute("role", "alert");
    err.textContent = error;
    root.append(err);
    paintBar();
  }

  async function commit(): Promise<void> {
    if (busy || commitBlockedReason(state) !== undefined) return;
    busy = true;
    error = "";
    render();
    try {
      const out = await host.applyTodo(toTodo(state));
      if (out.ok) {
        host.notify("info", "Stack updated");
      } else {
        error = out.error;
        host.notify("error", out.error);
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      host.notify("error", error);
    } finally {
      busy = false;
      render();
    }
  }

  render();
  return {
    update(next) {
      stack = next;
      state = initEditor(next);
      error = "";
      render();
    },
    state: () => state,
    dispose() {
      root.remove();
    },
  };
}
