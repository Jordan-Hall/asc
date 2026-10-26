// Split dialog DOM view (SPEC §24.3.2). Vanilla DOM, text only via `textContent`. The change's
// `Repo.Diff` text is shown file by file; the active piece tab receives the lines, hunks or files
// the user ticks (unticked ones stay in the last piece, the remainder). Split hands one `SplitSpec`
// to the host, which writes it and runs `split --hunks`. Disabled controls state a reason.
import type { RunOutcome } from "../../ops/queue";
import type { Severity } from "../../platform";
import {
  addPiece,
  assignFile,
  assignHunk,
  assignLine,
  commitBlockedReason,
  initSplit,
  MAX_PIECES,
  pieceCounts,
  pieceOfFile,
  pieceOfLine,
  removePiece,
  setPieceMessage,
  type SplitSpec,
  type SplitState,
  toSpec,
} from "./model";

export interface SplitDialogHost {
  /** Write the spec to a private file and run `split --change <id> --hunks <file>` as one operation. */
  applySplit(change: string, spec: SplitSpec): Promise<RunOutcome>;
  notify(severity: Severity, message: string): void;
}

export interface SplitDialogInput {
  readonly change: string;
  readonly subject: string;
  /** `Repo.Diff` text of the change against its parent, and whether the reply was cut short. */
  readonly diff: string;
  readonly truncated: boolean;
}

export interface SplitDialogView {
  state(): SplitState;
  dispose(): void;
}

export function mountSplitDialog(
  container: HTMLElement,
  host: SplitDialogHost,
  input: SplitDialogInput,
): SplitDialogView {
  const doc = container.ownerDocument;
  let state = initSplit(input.diff, input.truncated, input.subject);
  let active = 0;
  let busy = false;
  let error = "";

  const root = doc.createElement("div");
  root.className = "asc-split";
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

  const change = (next: SplitState): void => {
    state = next;
    render();
  };

  function paintBar(): void {
    const apply = root.querySelector<HTMLButtonElement>('[data-action="apply"]');
    if (apply) gate(apply, busy ? "Applying" : commitBlockedReason(state));
  }

  function renderTabs(): void {
    const counts = pieceCounts(state);
    const tabs = doc.createElement("div");
    tabs.className = "asc-sp-pieces";
    for (let k = 0; k < state.pieces; k++) {
      const last = k === state.pieces - 1;
      const tab = button(
        `${last ? "Remainder" : `Piece ${k + 1}`} (${counts[k] ?? 0})`,
        "piece",
        () => {
          active = k;
          render();
        },
      );
      tab.classList.add("asc-sp-piece");
      tab.dataset.testid = `sp-piece-${k}`;
      tab.setAttribute("aria-pressed", String(k === active));
      if (last) gate(tab, "Receives whatever is not ticked");
      tabs.append(tab);
    }
    const add = button("Add piece", "add-piece", () => change(addPiece(state)));
    gate(add, busy ? "Applying" : state.pieces >= MAX_PIECES ? "Too many pieces" : undefined);
    const rm = button("Remove piece", "remove-piece", () => change(removePiece(state, active)));
    gate(rm, busy ? "Applying" : state.pieces <= 2 ? "At least two pieces" : undefined);
    tabs.append(add, rm);
    root.append(tabs);

    for (let k = 0; k < state.pieces; k++) {
      const last = k === state.pieces - 1;
      const field = doc.createElement("input");
      field.type = "text";
      field.className = "asc-sp-msg";
      field.dataset.testid = `sp-message-${k}`;
      field.hidden = k !== active && !last;
      field.placeholder = last ? "Message (empty keeps the original)" : "Message";
      field.setAttribute("aria-label", `Message for piece ${k + 1}`);
      field.value = state.messages[k] ?? "";
      field.disabled = busy;
      field.addEventListener("input", () => {
        state = setPieceMessage(state, k, field.value);
        paintBar();
      });
      root.append(field);
    }
  }

  function renderFile(file: SplitState["files"][number], f: number): HTMLElement {
    const box = doc.createElement("section");
    box.className = "asc-sp-file";
    box.dataset.testid = `sp-file-${f}`;
    const head = doc.createElement("div");
    head.className = "asc-sp-file-head";
    const path = doc.createElement("span");
    path.className = "asc-sp-path";
    path.textContent = file.path || "(unknown path)";
    head.append(path);
    if (file.unsupported) {
      const why = doc.createElement("span");
      why.textContent = file.unsupported;
      head.append(why);
    } else if (!file.granular) {
      const whole = doc.createElement("input");
      whole.type = "checkbox";
      whole.dataset.testid = `sp-whole-${f}`;
      whole.setAttribute("aria-label", `Move ${file.path} whole`);
      whole.checked = pieceOfFile(state, f) === active;
      whole.disabled = busy;
      whole.addEventListener("change", () =>
        change(assignFile(state, f, whole.checked ? active : state.pieces - 1)),
      );
      const lab = doc.createElement("span");
      lab.textContent = "whole file";
      head.append(lab, whole);
    } else {
      head.append(
        button("All", "file-all", () => change(assignFile(state, f, active))),
        button("None", "file-none", () => change(assignFile(state, f, state.pieces - 1))),
      );
    }
    box.append(head);
    file.hunks.forEach((hk, h) => {
      const hh = doc.createElement("div");
      hh.className = "asc-sp-hunk-head";
      const title = doc.createElement("span");
      title.textContent = `@@ -${hk.oldStart},${hk.oldLen} +${hk.newStart},${hk.newLen} @@`;
      const hb = button("Hunk", "hunk-all", () => change(assignHunk(state, f, h, active)));
      hb.dataset.testid = `sp-hunk-${f}-${h}`;
      hh.append(title, hb);
      box.append(hh);
      hk.lines.forEach((ln, l) => {
        const row = doc.createElement("div");
        row.className = `asc-sp-line asc-${ln.kind}`;
        const mark = doc.createElement("span");
        mark.textContent = ln.kind === "add" ? "+" : ln.kind === "del" ? "-" : " ";
        const text = doc.createElement("span");
        text.textContent = ln.text;
        if (ln.kind === "ctx") {
          row.append(doc.createElement("span"), mark, text);
        } else {
          const tick = doc.createElement("input");
          tick.type = "checkbox";
          tick.dataset.testid = `sp-line-${f}-${h}-${l}`;
          tick.setAttribute("aria-label", `Line ${l + 1} of hunk ${h + 1} in ${file.path}`);
          const p = pieceOfLine(state, f, h, l);
          tick.checked = p === active;
          tick.disabled = busy;
          tick.addEventListener("change", () =>
            change(assignLine(state, f, h, l, tick.checked ? active : state.pieces - 1)),
          );
          const badge = doc.createElement("span");
          badge.className = "asc-sp-badge";
          badge.textContent = p === state.pieces - 1 ? "R" : String(p + 1);
          row.append(tick, badge, mark, text);
        }
        box.append(row);
      });
    });
    return box;
  }

  function render(): void {
    root.replaceChildren();
    active = Math.min(active, state.pieces - 2);
    renderTabs();
    if (state.truncated) {
      const n = doc.createElement("div");
      n.className = "asc-sp-note";
      n.textContent = "The diff is truncated; the change cannot be split here.";
      root.append(n);
    }
    state.files.forEach((file, f) => root.append(renderFile(file, f)));

    const bar = doc.createElement("div");
    bar.className = "asc-sp-bar";
    bar.append(button("Split", "apply", () => void commit()));
    root.append(bar);
    const err = doc.createElement("div");
    err.className = "asc-sp-error";
    err.dataset.testid = "sp-error";
    err.setAttribute("role", "alert");
    err.textContent = error;
    root.append(err);
    paintBar();
  }

  async function commit(): Promise<void> {
    if (busy || commitBlockedReason(state) !== undefined) return;
    error = "";
    let spec: SplitSpec;
    try {
      spec = toSpec(state);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      render();
      return;
    }
    busy = true;
    render();
    try {
      const out = await host.applySplit(input.change, spec);
      if (out.ok) host.notify("info", "Change split");
      else {
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
    state: () => state,
    dispose() {
      root.remove();
    },
  };
}
