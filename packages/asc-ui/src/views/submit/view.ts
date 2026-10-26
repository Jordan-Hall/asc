// Submit dialog DOM view (SPEC §24.3.2). Vanilla DOM, text only via `textContent`. Options: draft or
// publish, reviewers (from Forgejo assignees only), auto-merge, and the PR title and description of
// each change (defaulting to the change message). Submit runs `submit` once (the host confirms in a
// modal) and then sends the edited text of existing PRs. Disabled controls state a reason.
import type { RunOutcome } from "../../ops/queue";
import type { Severity } from "../../platform";
import type { RunSpec } from "../split/model";
import {
  addReviewer,
  pullEdits,
  type PullEdit,
  removeReviewer,
  reviewerBlockedReason,
  type SubmitChange,
  type SubmitMode,
  type SubmitState,
  setAutoMerge,
  setMode,
  setStack,
  setText,
  initSubmit,
  submitBlockedReason,
  submitRun,
  textOf,
} from "./model";

export interface SubmitDialogHost {
  /** Run the `submit` request (after the host's modal confirmation). */
  runSubmit(run: RunSpec): Promise<RunOutcome>;
  /** Forgejo `PATCH …/pulls/{n}` with `title` and `body` only. */
  editPull(edit: PullEdit): Promise<void>;
  /** Forgejo `GET …/assignees`. */
  loadAssignees(): Promise<readonly string[]>;
  notify(severity: Severity, message: string): void;
}

export interface SubmitDialogView {
  state(): SubmitState;
  dispose(): void;
}

export function mountSubmitDialog(
  container: HTMLElement,
  host: SubmitDialogHost,
  changes: readonly SubmitChange[],
  current?: string,
): SubmitDialogView {
  const doc = container.ownerDocument;
  let state = initSubmit(changes);
  let busy = false;
  let error = "";
  let loadNote = "Loading reviewers";
  let disposed = false;

  const root = doc.createElement("div");
  root.className = "asc-submit";
  container.append(root);

  const gate = (b: HTMLButtonElement, reason: string | undefined): void => {
    b.disabled = reason !== undefined;
    if (reason !== undefined) b.title = reason;
    else b.removeAttribute("title");
  };

  function button(label: string, action: string, onClick: () => void): HTMLButtonElement {
    const b = doc.createElement("button");
    b.type = "button";
    b.className = "asc-action";
    b.dataset.action = action;
    b.textContent = label;
    b.addEventListener("click", onClick);
    return b;
  }

  function row(...kids: Node[]): HTMLElement {
    const r = doc.createElement("div");
    r.className = "asc-su-row";
    r.append(...kids);
    return r;
  }

  function labelled(text: string, control: HTMLElement): HTMLLabelElement {
    const l = doc.createElement("label");
    const t = doc.createElement("span");
    t.textContent = text;
    l.append(t, " ", control);
    return l;
  }

  function checkbox(
    testid: string,
    checked: boolean,
    onChange: (v: boolean) => void,
  ): HTMLInputElement {
    const c = doc.createElement("input");
    c.type = "checkbox";
    c.dataset.testid = testid;
    c.checked = checked;
    c.disabled = busy;
    c.addEventListener("change", () => onChange(c.checked));
    return c;
  }

  const paintBar = (): void => {
    const go = root.querySelector<HTMLButtonElement>('[data-action="submit"]');
    if (go) gate(go, busy ? "Submitting" : submitBlockedReason(state));
  };

  function render(): void {
    root.replaceChildren();

    root.append(
      row(
        labelled(
          "Whole stack",
          checkbox("su-stack", state.stack, (v) => {
            state = setStack(state, v);
            render();
          }),
        ),
        labelled(
          "Auto-merge",
          checkbox("su-auto", state.autoMerge, (v) => {
            state = setAutoMerge(state, v);
            render();
          }),
        ),
      ),
    );

    const mode = doc.createElement("select");
    mode.dataset.testid = "su-mode";
    mode.setAttribute("aria-label", "Draft or publish");
    for (const [v, l] of [
      ["default", "Repository default"],
      ["draft", "Draft"],
      ["publish", "Publish"],
    ] as const) {
      const o = doc.createElement("option");
      o.value = v;
      o.textContent = l;
      mode.append(o);
    }
    mode.value = state.mode;
    mode.disabled = busy;
    mode.addEventListener("change", () => {
      state = setMode(state, mode.value as SubmitMode);
      render();
    });
    root.append(row(labelled("PRs", mode)));

    const chips = doc.createElement("span");
    for (const r of state.reviewers) {
      const chip = doc.createElement("span");
      chip.className = "asc-su-chip";
      chip.dataset.testid = `su-reviewer-${r}`;
      const name = doc.createElement("span");
      name.textContent = r;
      const x = button("Remove", "remove-reviewer", () => {
        state = removeReviewer(state, r);
        render();
      });
      x.setAttribute("aria-label", `Remove reviewer ${r}`);
      x.disabled = busy;
      chip.append(name, x);
      chips.append(chip);
    }
    const pick = doc.createElement("select");
    pick.dataset.testid = "su-add-reviewer";
    pick.setAttribute("aria-label", "Add reviewer");
    const none = doc.createElement("option");
    none.value = "";
    none.textContent = state.candidates ? "Add reviewer" : loadNote;
    pick.append(none);
    for (const c of state.candidates ?? []) {
      const o = doc.createElement("option");
      o.value = c;
      o.textContent = c;
      const why = reviewerBlockedReason(state, c);
      o.disabled = why !== undefined;
      if (why !== undefined) o.title = why;
      pick.append(o);
    }
    pick.disabled = busy || state.candidates === undefined;
    pick.addEventListener("change", () => {
      state = addReviewer(state, pick.value);
      render();
    });
    root.append(row(chips, pick));

    for (const c of state.changes) {
      const t = textOf(state, c.id);
      const box = doc.createElement("section");
      box.className = "asc-su-change";
      box.dataset.testid = `su-change-${c.id}`;
      const title = doc.createElement("input");
      title.type = "text";
      title.className = "asc-su-field";
      title.dataset.testid = `su-title-${c.id}`;
      title.setAttribute("aria-label", `PR title for ${c.id}`);
      title.value = t.title;
      title.disabled = busy;
      const body = doc.createElement("textarea");
      body.className = "asc-su-field";
      body.dataset.testid = `su-body-${c.id}`;
      body.setAttribute("aria-label", `PR description for ${c.id}`);
      body.value = t.body;
      body.disabled = busy;
      const edit = (): void => {
        state = setText(state, c.id, { title: title.value, body: body.value });
        paintBar();
      };
      title.addEventListener("input", edit);
      body.addEventListener("input", edit);
      box.append(title, body);
      if (!c.pull) {
        const n = doc.createElement("div");
        n.textContent = "New PR: the text is used when it is created";
        box.append(n);
      }
      root.append(box);
    }

    const bar = doc.createElement("div");
    bar.className = "asc-su-bar";
    bar.append(button("Submit", "submit", () => void commit()));
    root.append(bar);
    const err = doc.createElement("div");
    err.className = "asc-su-error";
    err.dataset.testid = "su-error";
    err.setAttribute("role", "alert");
    err.textContent = error;
    root.append(err);
    paintBar();
  }

  async function commit(): Promise<void> {
    if (busy || submitBlockedReason(state) !== undefined) return;
    error = "";
    let run: RunSpec;
    try {
      run = submitRun(state, state.stack ? undefined : current);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      render();
      return;
    }
    busy = true;
    render();
    try {
      const out = await host.runSubmit(run);
      if (!out.ok) {
        error = out.error;
        host.notify("error", out.error);
      } else {
        const failed: string[] = [];
        for (const e of pullEdits(state)) {
          try {
            await host.editPull(e);
          } catch (err) {
            failed.push(err instanceof Error ? err.message : String(err));
          }
        }
        if (failed.length) {
          error = `Submitted; ${failed.length} PR text edit(s) failed: ${failed[0]}`;
          host.notify("warning", error);
        } else host.notify("info", "Submitted");
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
  host.loadAssignees().then(
    (list) => {
      if (disposed) return;
      state = { ...state, candidates: list };
      render();
    },
    (e: unknown) => {
      if (disposed) return;
      loadNote = `Reviewers unavailable: ${e instanceof Error ? e.message : "error"}`;
      render();
    },
  );
  return {
    state: () => state,
    dispose() {
      disposed = true;
      root.remove();
    },
  };
}
