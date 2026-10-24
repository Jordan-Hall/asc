// Inbox DOM view (SPEC §24.3.5, §24.4.7). Vanilla DOM; server text only through `textContent`.
// Pin and snooze go through the inbox API via the host; the view re-reads after each change.
import type { Severity } from "../../platform";
import { buildSections, type InboxEntry, SNOOZE_PRESETS, snoozeUntil } from "./model";

export interface InboxHost {
  list(): Promise<readonly InboxEntry[]>;
  /** `PUT /user/asc/inbox/{id}`. */
  update(id: number, body: { pinned?: boolean; snoozed_until?: string }): Promise<void>;
  openPull(number: number): void;
  notify(severity: Severity, message: string): void;
  now?(): number;
}

export interface InboxView {
  refresh(): Promise<void>;
  dispose(): void;
}

export function mountInbox(container: HTMLElement, host: InboxHost): InboxView {
  const doc = container.ownerDocument;
  const now = () => host.now?.() ?? Date.now();
  let entries: readonly InboxEntry[] = [];
  let showSnoozed = false;
  let disposed = false;

  const root = doc.createElement("div");
  root.className = "asc-inbox";
  root.dataset.testid = "inbox";
  container.append(root);

  const el = <K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string) => {
    const e = doc.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  };

  const button = (text: string, action: string, onClick: () => void) => {
    const b = el("button", "asc-action", text);
    b.type = "button";
    b.dataset.action = action;
    b.addEventListener("click", onClick);
    return b;
  };

  async function act(id: number, body: { pinned?: boolean; snoozed_until?: string }): Promise<void> {
    try {
      await host.update(id, body);
    } catch (err) {
      host.notify("error", err instanceof Error ? err.message : "inbox update failed");
    }
    await refresh();
  }

  function render(): void {
    if (disposed) return;
    root.replaceChildren();
    const bar = el("div", "asc-toolbar");
    const label = el("label");
    const box = el("input");
    box.type = "checkbox";
    box.checked = showSnoozed;
    box.setAttribute("aria-label", "Show snoozed");
    box.addEventListener("change", () => {
      showSnoozed = box.checked;
      render();
    });
    label.append(box, doc.createTextNode(" Show snoozed"));
    bar.append(label);
    root.append(bar);

    for (const s of buildSections(entries, now(), showSnoozed)) {
      const sec = el("section", "asc-inbox-section");
      sec.dataset.testid = `section-${s.id}`;
      sec.append(el("h3", undefined, `${s.title} (${s.items.length})`));
      if (s.items.length === 0) sec.append(el("div", "asc-inbox-empty", "Nothing here"));
      const ul = el("ul", "asc-inbox-items");
      for (const it of s.items) {
        const li = el("li", it.snoozed ? "asc-inbox-item asc-snoozed" : "asc-inbox-item");
        li.dataset.testid = `item-${it.id}`;
        const prefix = it.number === undefined ? "" : `#${it.number} `;
        const title = el("span", "asc-inbox-title", `${prefix}${it.title}`);
        const n = it.number;
        if (n !== undefined) title.addEventListener("click", () => host.openPull(n));
        li.append(title);
        if (it.pinned) {
          const b = el("span", "asc-badge", "pinned");
          b.dataset.badge = "pinned";
          li.append(b);
        }
        if (it.snoozed) {
          const b = el("span", "asc-badge", "snoozed");
          b.dataset.badge = "snoozed";
          li.append(b);
        }
        li.append(button(it.pinned ? "Unpin" : "Pin", "pin", () => void act(it.id, { pinned: !it.pinned })));
        if (it.snoozed) {
          li.append(
            button("Unsnooze", "unsnooze", () => void act(it.id, { snoozed_until: new Date(0).toISOString() })),
          );
        } else {
          const sel = el("select", "asc-action");
          sel.dataset.action = "snooze";
          sel.setAttribute("aria-label", "Snooze");
          const none = el("option", undefined, "Snooze...");
          none.value = "";
          sel.append(none);
          for (const p of SNOOZE_PRESETS) {
            const o = el("option", undefined, p.label);
            o.value = p.id;
            sel.append(o);
          }
          sel.addEventListener("change", () => {
            const p = SNOOZE_PRESETS.find((x) => x.id === sel.value);
            if (p) void act(it.id, { snoozed_until: snoozeUntil(p.ms, now()) });
          });
          li.append(sel);
        }
        ul.append(li);
      }
      sec.append(ul);
      root.append(sec);
    }
  }

  async function refresh(): Promise<void> {
    try {
      entries = await host.list();
    } catch (err) {
      host.notify("error", err instanceof Error ? err.message : "inbox unavailable");
    }
    render();
  }

  render();
  void refresh();
  return {
    refresh,
    dispose() {
      disposed = true;
      root.remove();
    },
  };
}
