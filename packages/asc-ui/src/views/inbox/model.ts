// Inbox model (SPEC §24.3.5, §24.4.7): pure grouping of `GET /user/asc/inbox` entries into the
// seven sections, pinned first, snoozed entries hidden until their time passes.

/** The subset of the Forgejo inbox entry the view reads (structurally the swagger type). */
export interface InboxPull {
  readonly number?: number;
  readonly title?: string;
  readonly draft?: boolean;
  readonly state?: string;
}
export interface InboxEntry {
  readonly id?: number;
  readonly section?: string;
  readonly pinned?: boolean;
  readonly snoozed_until?: string;
  readonly pull_request?: InboxPull;
}

export const SECTIONS = [
  { id: "needs_review", title: "Needs your review" },
  { id: "returned", title: "Returned to you" },
  { id: "ready", title: "Ready to merge" },
  { id: "blocked", title: "Blocked" },
  { id: "waiting", title: "Waiting" },
  { id: "drafts", title: "Drafts" },
  { id: "queued", title: "Queued" },
] as const;
export type SectionId = (typeof SECTIONS)[number]["id"];

export interface InboxItem {
  readonly id: number;
  readonly number: number | undefined;
  readonly title: string;
  readonly pinned: boolean;
  readonly snoozedUntil: string | undefined;
  readonly snoozed: boolean;
}
export interface InboxSection {
  readonly id: SectionId;
  readonly title: string;
  readonly items: readonly InboxItem[];
  /** Entries hidden by an active snooze. */
  readonly snoozedCount: number;
}

export const SNOOZE_PRESETS = [
  { id: "1h", label: "1 hour", ms: 3_600_000 },
  { id: "tomorrow", label: "Tomorrow", ms: 86_400_000 },
  { id: "week", label: "1 week", ms: 7 * 86_400_000 },
] as const;

export function isSnoozed(e: Pick<InboxEntry, "snoozed_until">, now: number): boolean {
  if (e.snoozed_until === undefined) return false;
  const t = Date.parse(e.snoozed_until);
  return Number.isFinite(t) && t > now;
}

export function snoozeUntil(presetMs: number, now: number): string {
  return new Date(now + presetMs).toISOString();
}

/** Group entries; unknown sections are dropped. Pinned first, then by PR number. */
export function buildSections(
  entries: readonly InboxEntry[],
  now: number,
  showSnoozed = false,
): InboxSection[] {
  return SECTIONS.map(({ id, title }) => {
    const mine = entries.filter((e) => e.section === id && e.id !== undefined);
    const all: InboxItem[] = mine.map((e) => ({
      id: e.id as number,
      number: e.pull_request?.number,
      title: e.pull_request?.title ?? "(untitled)",
      pinned: e.pinned === true,
      snoozedUntil: e.snoozed_until,
      snoozed: isSnoozed(e, now),
    }));
    const items = all
      .filter((i) => showSnoozed || !i.snoozed)
      .toSorted((a, b) => Number(b.pinned) - Number(a.pinned) || (a.number ?? 0) - (b.number ?? 0));
    return { id, title, items, snoozedCount: all.filter((i) => i.snoozed).length };
  });
}

/** Status-bar count: unsnoozed "needs review" entries. */
export function needsReviewCount(entries: readonly InboxEntry[], now: number): number {
  return entries.filter((e) => e.section === "needs_review" && !isSnoozed(e, now)).length;
}
