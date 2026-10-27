import { describe, expect, test } from "bun:test";
import { InboxSection } from "@asc/asc-client/inbox";
import { parseDescription } from "../src/api/changes";
import type { InboxView } from "../src/api/inbox";
import {
  buildNeeds,
  buildToday,
  humanize,
  mentions,
  readyToMerge,
  type RepoInputs,
  segmentOf,
  stackBars,
} from "../src/api/needs";
import type { ChangeRow, StackView } from "../src/api/types";
import { agoShort } from "../src/lib/time";
import { greeting, parseViewer } from "../src/lib/viewer";

const NOW = 1_000_000;

function change(over: Partial<ChangeRow> & { id: string }): ChangeRow {
  return {
    short: over.id.slice(0, 8),
    title: `change ${over.id}`,
    version: 1,
    position: 0,
    stackSize: 1,
    ref: "refs/heads/s",
    landed: false,
    divergent: false,
    needsRestack: false,
    review: { state: "open", number: 1, draft: false, approvals: 0, checks: "success", url: "" },
    author: "ada",
    time: NOW - 600,
    description: { title: "", intent: "", risk: "", riskArea: "", agent: "" },
    stat: undefined,
    ...over,
  };
}

const stack = (changes: ChangeRow[], ref = "refs/heads/rate-limit-headers"): StackView => ({
  ref,
  trunk: "main",
  changes: changes.map((c, i) => ({ ...c, position: i, stackSize: changes.length })),
});

describe("parseDescription", () => {
  test("title, intent and trailers", () => {
    const d = parseDescription(
      "Wire it\n\nEvery call retries.\nRefunds too.\n\nRisk: touches billing.\nRisk-area: billing\nAgent: coder-2\n",
    );
    expect(d).toEqual({
      title: "Wire it",
      intent: "Every call retries.\nRefunds too.",
      risk: "touches billing.",
      riskArea: "billing",
      agent: "coder-2",
    });
  });
  test("a title alone, and trailer-like text inside the prose", () => {
    expect(parseDescription("Only a title").intent).toBe("");
    expect(parseDescription("T\n\nRisk: not a trailer\nbecause prose follows").risk).toBe("");
  });
});

const inbox = (items: InboxView["items"]): InboxView => ({ items, total: items.length, next: "" });
const item = (id: string, section: InboxSection, over = {}) => ({
  change: id,
  section,
  latestVersion: 1,
  author: "ada",
  updated: NOW - 60,
  openThreads: 0,
  pinned: false,
  requestedBy: "ada",
  ...over,
});

const approved = (approvals: number, checks = "success") => ({
  state: "open",
  number: 1,
  draft: false,
  approvals,
  checks,
  url: "",
});

describe("needs", () => {
  test("a review request, a mention and a ready stack, newest first", () => {
    const a = change({ id: "aaaa", title: "Wire retry policy", position: 2 });
    const b = change({ id: "bbbb", title: "Infra rollout window" });
    const ready = [change({ id: "cccc", review: approved(2), time: NOW - 30 })];
    const repos: RepoInputs[] = [
      {
        repo: "acme/payments",
        changes: { trunk: "main", stacks: [stack([a, b]), stack(ready)] },
        inbox: inbox([
          item("aaaa", InboxSection.NEEDS_REVIEW, { updated: NOW - 720 }),
          item("bbbb", InboxSection.RETURNED, { updated: NOW - 100 }),
        ]),
        threads: new Map([
          [
            "bbbb",
            {
              next: "",
              openThreads: 1,
              threads: [
                {
                  id: 1,
                  path: "",
                  version: 1,
                  startLine: 0,
                  resolved: false,
                  openedBy: "grace",
                  comments: [
                    { id: 1, author: "Grace", body: "@jordan please move it", created: NOW - 100 },
                  ],
                },
              ],
            },
          ],
        ]),
      },
    ];
    const rows = buildNeeds(repos, "Jordan");
    expect(rows.map((r) => r.kind)).toEqual(["merge", "mention", "review"]);
    expect(rows[0]?.headline).toBe("Ready to merge: Rate limit headers");
    expect(rows[0]?.detail).toBe("2 approvals · checks green");
    expect(rows[1]?.headline).toBe("You were mentioned: infra rollout window");
    expect(rows[2]?.detail).toBe("stack 1 of 2");
    // Without a viewer name nothing is a mention.
    expect(buildNeeds(repos, "").map((r) => r.kind)).toContain("reply");
  });

  test("ready to merge needs every change approved, green and not a draft", () => {
    const ok = change({ id: "1", review: approved(1) });
    expect(readyToMerge(stack([ok]))?.approvals).toBe(1);
    expect(readyToMerge(stack([ok, change({ id: "2" })]))).toBeUndefined();
    expect(
      readyToMerge(stack([ok, change({ id: "3", review: approved(1, "failure") })])),
    ).toBeUndefined();
    expect(readyToMerge(stack([]))).toBeUndefined();
  });

  test("stack bars: green approved, amber waiting, grey draft", () => {
    const rows = [
      change({ id: "1", review: approved(1, "") }),
      change({ id: "2" }),
      change({
        id: "3",
        review: { state: "open", number: 3, draft: true, approvals: 0, checks: "", url: "" },
      }),
    ];
    expect(rows.map(segmentOf)).toEqual(["done", "waiting", "idle"]);
    const bars = stackBars([
      {
        repo: "a/b",
        changes: { trunk: "main", stacks: [stack(rows, "refs/heads/x")] },
        inbox: undefined,
        threads: new Map(),
      },
    ]);
    expect(bars[0]?.name).toBe("x");
    expect(bars[0]?.segments).toEqual(["done", "waiting", "idle"]);
  });

  test("today lists what others did and leaves the viewer out", () => {
    const rows: RepoInputs[] = [
      {
        repo: "a/b",
        changes: {
          trunk: "main",
          stacks: [
            stack(
              [
                change({ id: "1", author: "coder-2", version: 4 }),
                change({ id: "2", author: "Jordan", version: 2 }),
              ],
              "refs/heads/retry",
            ),
          ],
        },
        inbox: undefined,
        threads: new Map(),
      },
    ];
    const t = buildToday(rows, "Jordan", NOW);
    expect(t.map((e) => `${e.who} ${e.what}`)).toEqual(["coder-2 pushed 4 versions to retry"]);
  });
});

describe("small helpers", () => {
  test("humanize and mentions", () => {
    expect(humanize("refs/heads/cursor-pagination")).toBe("Cursor pagination");
    expect(mentions("hi @Jordan!", "jordan")).toBe(true);
    expect(mentions("mail@jordan.dev", "jordan")).toBe(false);
    expect(mentions("anything", "")).toBe(false);
  });
  test("short ages", () => {
    const now = 100_000_000;
    expect(agoShort(now / 1000 - 12 * 60, now)).toBe("12 min");
    expect(agoShort(now / 1000 - 3 * 3600, now)).toBe("3 h");
    expect(agoShort(now / 1000 - 5 * 86400, now)).toBe("5 d");
    expect(agoShort(now / 1000 - 15 * 86400, now)).toBe("2 w");
  });
  test("viewer names and greetings", () => {
    expect(parseViewer("  Jordan   Upton ")).toEqual({
      name: "Jordan Upton",
      first: "Jordan",
      initials: "JU",
    });
    expect(parseViewer("")).toBeUndefined();
    expect(greeting(9)).toBe("Good morning");
    expect(greeting(14)).toBe("Good afternoon");
    expect(greeting(20)).toBe("Good evening");
  });
});
