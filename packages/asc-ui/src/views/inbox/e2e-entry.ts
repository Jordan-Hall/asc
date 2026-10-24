// Browser entry for the Playwright spec only: mounts the inbox over a recording fake host.
// Not part of the package exports or the production bundle.
import { applyTokens, resolveTokens } from "../../theme";
import type { InboxEntry } from "./model";
import { inboxCss } from "./style";
import { mountInbox } from "./view";

interface Calls {
  updates: { id: number; body: { pinned?: boolean; snoozed_until?: string } }[];
  opened: number[];
}
declare global {
  interface Window {
    ascInbox: { calls: Calls };
    ascStartInbox(entries: InboxEntry[], now: number): void;
  }
}

type Mutable = { id?: number; pinned?: boolean; snoozed_until?: string };

window.ascStartInbox = (seed, now) => {
  applyTokens(document.documentElement, resolveTokens());
  const style = document.createElement("style");
  style.textContent = inboxCss;
  document.head.append(style);
  const host = document.createElement("div");
  document.body.append(host);
  const entries: Mutable[] = seed.map((e) => ({ ...e }));
  const calls: Calls = { updates: [], opened: [] };
  window.ascInbox = { calls };
  mountInbox(host, {
    now: () => now,
    async list() {
      return entries.map((e) => ({ ...e }));
    },
    async update(id, body) {
      calls.updates.push({ id, body });
      const e = entries.find((x) => x.id === id);
      if (e && body.pinned !== undefined) e.pinned = body.pinned;
      if (e && body.snoozed_until !== undefined) e.snoozed_until = body.snoozed_until;
    },
    openPull(n) {
      calls.opened.push(n);
    },
    notify() {},
  });
};
