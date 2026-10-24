// Browser entry for the Playwright spec only: mounts the stack editor over a recording fake host.
// Not part of the package exports or the production bundle.
import type { Stack } from "../../state/model";
import { applyTokens, resolveTokens } from "../../theme";
import type { StackTodo } from "./model";
import { stackEditorCss } from "./style";
import { mountStackEditor } from "./view";

declare global {
  interface Window {
    ascEditor: { todos: StackTodo[]; notes: string[] };
    ascStartEditor(stack: Stack, fail?: string): void;
  }
}

window.ascStartEditor = (stack, fail) => {
  applyTokens(document.documentElement, resolveTokens());
  const style = document.createElement("style");
  style.textContent = stackEditorCss;
  document.head.append(style);
  const host = document.createElement("div");
  document.body.append(host);
  const rec = { todos: [] as StackTodo[], notes: [] as string[] };
  mountStackEditor(
    host,
    {
      async applyTodo(todo) {
        rec.todos.push(todo);
        return fail === undefined ? { ok: true } : { ok: false, error: fail };
      },
      notify(_s, m) {
        rec.notes.push(m);
      },
    },
    stack,
  );
  window.ascEditor = rec;
};
