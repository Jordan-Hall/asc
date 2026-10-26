// Smartlog DOM view (SPEC §24.4.5). Vanilla DOM, text set only through `textContent`, so no
// server string is ever parsed as markup. Interactions: click selects, double click checks out,
// drag reparents with a predicted-conflict badge (`move -n` first), buttons carry a stated reason
// when disabled.
import type { MoveTarget } from "../../ops/mutations";
import type { EnqueueResult, RunOutcome } from "../../ops/queue";
import type { Severity } from "../../platform";
import type { Stacks } from "../../state/model";
import {
  type ActionId,
  type ActionState,
  buildRows,
  type ChangeExtras,
  type ChangeRow,
  type CloudDraft,
  dropBlockedReason,
  type MovePrediction,
  predictionText,
  type SmartlogInput,
  type UncommittedFile,
  UNCOMMITTED_ID,
  type VerbCall,
} from "./model";

export interface SmartlogHost {
  /** Enqueue the move on the optimistic queue (IP-D10). */
  enqueueMove(change: string, onto: MoveTarget): EnqueueResult;
  /** `move --onto … -n`: report conflicts without changing anything. */
  predictMove(change: string, onto: MoveTarget, signal: AbortSignal): Promise<MovePrediction>;
  /** Run a verb through `Bridge.Run`. Destructive verbs get the host's native confirmation. */
  run(call: VerbCall): Promise<RunOutcome>;
  notify(severity: Severity, message: string): void;
}

export interface SmartlogData {
  readonly stacks: Stacks;
  readonly extras?: Readonly<Record<string, ChangeExtras>>;
  readonly uncommitted?: readonly UncommittedFile[];
  readonly hidden?: readonly string[];
  readonly cloudDrafts?: readonly CloudDraft[];
}

export interface SmartlogView {
  update(data: SmartlogData): void;
  dispose(): void;
}

const el = <K extends keyof HTMLElementTagNameMap>(
  doc: Document,
  tag: K,
  cls?: string,
  text?: string,
): HTMLElementTagNameMap[K] => {
  const e = doc.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
};

const ACTION_VERB: Record<Exclude<ActionId, "create" | "modify">, (id: string) => VerbCall> = {
  checkout: (change) => ({ verb: "switch", change }),
  restack: (change) => ({ verb: "restack", change }),
  uncommit: () => ({ verb: "uncommit" }),
  unamend: () => ({ verb: "unamend" }),
  abandon: (change) => ({ verb: "abandon", change }),
  hide: (change) => ({ verb: "hide", change }),
  unhide: (change) => ({ verb: "unhide", change }),
};

export function mountSmartlog(
  container: HTMLElement,
  host: SmartlogHost,
  initial: SmartlogData,
): SmartlogView {
  const doc = container.ownerDocument;
  let data = initial;
  let selected: string | undefined;
  let showCloud = false;
  let showHidden = false;
  const checked = new Set<string>();
  let drag: string | undefined;
  let dirtyWhileDragging = false;
  let predictAbort: AbortController | undefined;
  let predictKey = "";
  let disposed = false;

  const input = (): SmartlogInput => ({
    stacks: data.stacks,
    extras: data.extras ?? {},
    uncommitted: data.uncommitted ?? [],
    hidden: new Set(data.hidden ?? []),
    cloudDrafts: data.cloudDrafts ?? [],
    showCloud,
    showHidden,
    checked,
  });

  const runAction = async (call: VerbCall): Promise<void> => {
    const out = await host.run(call);
    if (!out.ok) host.notify("error", out.error);
  };

  function actionButton(a: ActionState, onClick: () => void): HTMLButtonElement {
    const b = el(doc, "button", "asc-action", a.label);
    b.type = "button";
    b.dataset.action = a.id;
    b.setAttribute("aria-disabled", String(!a.enabled));
    if (a.enabled) {
      b.addEventListener("click", (ev) => {
        ev.stopPropagation();
        onClick();
      });
    } else {
      // aria-disabled keeps the button focusable so the reason is reachable by keyboard.
      b.title = a.reason ?? "";
      b.dataset.reason = a.reason ?? "";
      b.classList.add("asc-disabled");
      b.addEventListener("click", (ev) => ev.stopPropagation());
    }
    return b;
  }

  function clearPrediction(): void {
    predictAbort?.abort();
    predictAbort = undefined;
    predictKey = "";
    for (const n of container.querySelectorAll("[data-prediction]")) n.remove();
    for (const n of container.querySelectorAll(".asc-drop-ok,.asc-drop-blocked")) {
      n.classList.remove("asc-drop-ok", "asc-drop-blocked");
    }
  }

  function showPrediction(target: HTMLElement, text: string, conflict: boolean): void {
    let badge = target.querySelector<HTMLElement>("[data-prediction]");
    if (!badge) {
      badge = el(doc, "span", "asc-badge asc-prediction");
      badge.dataset.prediction = "";
      badge.dataset.testid = "prediction";
      target.append(badge);
    }
    badge.textContent = text;
    badge.classList.toggle("asc-conflict", conflict);
  }

  function wireDropTarget(node: HTMLElement, onto: MoveTarget): void {
    node.addEventListener("dragover", (ev) => {
      if (drag === undefined) return;
      const blocked = dropBlockedReason(data.stacks, drag, onto);
      if (blocked !== undefined) {
        node.classList.add("asc-drop-blocked");
        node.title = blocked;
        return;
      }
      ev.preventDefault();
      if (ev.dataTransfer) ev.dataTransfer.dropEffect = "move";
      node.classList.add("asc-drop-ok");
      const key = `${drag}>${onto.kind === "change" ? onto.id : onto.name}`;
      if (key === predictKey) return;
      clearPrediction();
      node.classList.add("asc-drop-ok");
      predictKey = key;
      const abort = new AbortController();
      predictAbort = abort;
      showPrediction(node, predictionText("pending"), false);
      void host.predictMove(drag, onto, abort.signal).then(
        (p) => {
          if (!abort.signal.aborted)
            showPrediction(node, predictionText(p), p.ok && p.conflicts > 0);
        },
        (e: unknown) => {
          if (!abort.signal.aborted) {
            showPrediction(node, predictionText({ ok: false, error: String(e) }), false);
          }
        },
      );
    });
    node.addEventListener("dragleave", (ev) => {
      if (ev.relatedTarget instanceof Node && node.contains(ev.relatedTarget)) return;
      clearPrediction();
    });
    node.addEventListener("drop", (ev) => {
      ev.preventDefault();
      const change = drag;
      clearPrediction();
      if (change === undefined) return;
      const blocked = dropBlockedReason(data.stacks, change, onto);
      if (blocked !== undefined) {
        host.notify("warning", blocked);
        return;
      }
      const res = host.enqueueMove(change, onto);
      if (!res.ok) host.notify("warning", res.reason);
    });
  }

  function renderChange(row: ChangeRow): HTMLElement {
    const li = el(doc, "li", "asc-row asc-change");
    li.dataset.testid = `row-${row.id}`;
    li.dataset.id = row.id;
    li.style.paddingLeft = `${8 + row.depth * 16}px`;
    li.tabIndex = 0;
    li.setAttribute("aria-selected", String(selected === row.id));
    li.classList.toggle("asc-current", row.current);
    li.classList.toggle("asc-selected", selected === row.id);
    li.classList.toggle("asc-hidden", row.hidden);
    li.append(el(doc, "span", "asc-dot", row.current ? "@" : "o"));
    li.append(el(doc, "span", "asc-subject", row.subject));
    for (const b of row.branches) li.append(el(doc, "span", "asc-badge asc-branch", b));
    for (const b of row.badges) {
      const badge = el(doc, "span", `asc-badge asc-${b.kind}`, b.text);
      badge.dataset.badge = b.kind;
      li.append(badge);
    }
    const bar = el(doc, "span", "asc-actions");
    for (const a of row.actions) {
      bar.append(
        actionButton(a, () => {
          const make = ACTION_VERB[a.id as keyof typeof ACTION_VERB];
          void runAction(make(row.id));
        }),
      );
    }
    li.append(bar);
    li.addEventListener("click", () => {
      selected = row.id;
      render();
    });
    li.addEventListener("dblclick", () => {
      const checkout = row.actions.find((a) => a.id === "checkout");
      if (checkout?.enabled) void runAction({ verb: "switch", change: row.id });
      else if (checkout?.reason) host.notify("info", checkout.reason);
    });
    li.draggable = false;
    if (!row.frozen && !row.landed) {
      li.draggable = true;
      li.addEventListener("dragstart", (ev) => {
        drag = row.id;
        ev.dataTransfer?.setData("text/plain", row.id);
        if (ev.dataTransfer) ev.dataTransfer.effectAllowed = "move";
      });
      li.addEventListener("dragend", () => {
        drag = undefined;
        clearPrediction();
        if (dirtyWhileDragging) {
          dirtyWhileDragging = false;
          render();
        }
      });
    }
    wireDropTarget(li, { kind: "change", id: row.id });
    return li;
  }

  function render(): void {
    if (disposed) return;
    if (drag !== undefined) {
      dirtyWhileDragging = true;
      return;
    }
    const rows = buildRows(input());
    const root = el(doc, "div", "asc-smartlog");
    root.dataset.testid = "smartlog";

    const bar = el(doc, "div", "asc-toolbar");
    for (const [label, get, set] of [
      ["Show cloud drafts", () => showCloud, (v: boolean) => (showCloud = v)],
      ["Show hidden", () => showHidden, (v: boolean) => (showHidden = v)],
    ] as const) {
      const lab = el(doc, "label", "asc-toggle");
      const cb = el(doc, "input");
      cb.type = "checkbox";
      cb.checked = get();
      cb.addEventListener("change", () => {
        set(cb.checked);
        render();
      });
      lab.append(cb, doc.createTextNode(` ${label}`));
      bar.append(lab);
    }
    root.append(bar);

    const list = el(doc, "ul", "asc-rows");
    list.setAttribute("role", "list");
    if (rows.length === 0) list.append(el(doc, "li", "asc-empty", "No drafts"));
    for (const row of rows) {
      if (row.kind === "change") {
        list.append(renderChange(row));
      } else if (row.kind === "cloud") {
        const li = el(doc, "li", "asc-row asc-cloud");
        li.dataset.testid = `cloud-${row.id}`;
        li.append(el(doc, "span", "asc-dot", "c"), el(doc, "span", "asc-subject", row.subject));
        li.append(el(doc, "span", "asc-badge", `on ${row.machine}`));
        list.append(li);
      } else {
        const li = el(doc, "li", "asc-row asc-uncommitted");
        li.dataset.testid = "row-uncommitted";
        li.dataset.id = UNCOMMITTED_ID;
        li.append(el(doc, "div", "asc-subject", `Uncommitted changes (${row.files.length})`));
        const files = el(doc, "ul", "asc-files");
        for (const f of row.files) {
          const item = el(doc, "li", "asc-file");
          const lab = el(doc, "label");
          const cb = el(doc, "input");
          cb.type = "checkbox";
          cb.checked = f.checked;
          cb.dataset.path = f.path;
          cb.addEventListener("change", () => {
            if (cb.checked) checked.add(f.path);
            else checked.delete(f.path);
            render();
          });
          lab.append(cb, doc.createTextNode(` ${f.status} `), el(doc, "span", "asc-path", f.path));
          item.append(lab);
          files.append(item);
        }
        li.append(files);
        const acts = el(doc, "span", "asc-actions");
        for (const a of row.actions) {
          acts.append(
            actionButton(a, () => {
              const paths = [...checked];
              void runAction(
                a.id === "create"
                  ? { verb: "create", files: paths }
                  : { verb: "modify", files: paths },
              );
            }),
          );
        }
        li.append(acts);
        list.append(li);
      }
    }
    for (const trunk of data.stacks.trunks) {
      const li = el(doc, "li", "asc-row asc-trunk");
      li.dataset.testid = `trunk-${trunk.name}`;
      li.append(el(doc, "span", "asc-dot", "*"), el(doc, "span", "asc-subject", trunk.name));
      wireDropTarget(li, { kind: "trunk", name: trunk.name });
      list.append(li);
    }
    root.append(list);
    container.replaceChildren(root);
  }

  render();
  return {
    update(next) {
      data = next;
      const live = new Set((next.uncommitted ?? []).map((f) => f.path));
      for (const p of checked) if (!live.has(p)) checked.delete(p);
      render();
    },
    dispose() {
      disposed = true;
      predictAbort?.abort();
      container.replaceChildren();
    },
  };
}
