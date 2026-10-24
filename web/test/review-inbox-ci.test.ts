import { describe, expect, test } from "bun:test";
import { changeIdToString } from "@asc/asc-client";
import { Ci, CiStatus } from "@asc/asc-client/ci";
import { Inbox, InboxSection } from "@asc/asc-client/inbox";
import { Review, ThreadStatus } from "@asc/asc-client/review";
import { listRuns } from "../src/api/ci";
import { countInbox, listInbox } from "../src/api/inbox";
import { MockTransport } from "../src/api/mock";
import { listThreads } from "../src/api/review";

const CHANGE = "kmkmkmkmkmkmkmkmkmkmkmkmkmkmkmkm";
const bytes = new Uint8Array(16).fill(0x11);

describe("review, inbox and ci clients", () => {
  test("listThreads sends the change and maps the threads", async () => {
    const t = new MockTransport().onUnary(Review.method.listThreads, (req) => {
      expect(req.repo?.name).toBe("o/r");
      expect(req.change.length).toBe(16);
      expect(req.status).toBe(ThreadStatus.OPEN);
      return {
        openThreads: 1,
        threads: [
          {
            id: 7n,
            status: ThreadStatus.OPEN,
            openedBy: "ann",
            anchor: { path: new TextEncoder().encode("a/b.rs"), version: 2, startLine: 9 },
            comments: [{ id: 1n, author: "ann", body: "why?", created: 5n }],
          },
        ],
      };
    });
    const v = await listThreads(t, "o/r", CHANGE, { status: ThreadStatus.OPEN });
    expect(v.openThreads).toBe(1);
    expect(v.threads[0]).toMatchObject({ id: 7, path: "a/b.rs", version: 2, resolved: false });
    expect(v.threads[0]?.comments[0]?.body).toBe("why?");
    expect(t.calls).toEqual(["asc.v1.Review/ListThreads"]);
  });

  test("listInbox and countInbox map sections", async () => {
    const t = new MockTransport()
      .onUnary(Inbox.method.list, () => ({
        total: 1,
        items: [{ change: bytes, section: InboxSection.NEEDS_REVIEW, pinned: true, updated: 3n }],
      }))
      .onUnary(Inbox.method.count, (req) => ({
        total: 4,
        etag: `after-${req.ifNoneMatch}`,
        counts: [{ section: InboxSection.RETURNED, count: 4 }],
      }));
    const l = await listInbox(t, "o/r");
    expect(l.items[0]).toMatchObject({
      change: changeIdToString(bytes),
      section: InboxSection.NEEDS_REVIEW,
      pinned: true,
      updated: 3,
    });
    const c = await countInbox(t, "o/r", "e1");
    expect(c.etag).toBe("after-e1");
    expect(c.bySection.get(InboxSection.RETURNED)).toBe(4);
  });

  test("listRuns maps runs with their jobs", async () => {
    const t = new MockTransport().onUnary(Ci.method.listRuns, () => ({
      runs: [
        {
          id: 2n,
          name: "build",
          status: CiStatus.FAILURE,
          jobs: [{ id: 3n, name: "unit", status: CiStatus.FAILURE, logSize: 10n }],
        },
      ],
    }));
    const v = await listRuns(t, "o/r");
    expect(v.runs[0]).toMatchObject({ id: 2, change: "", status: CiStatus.FAILURE });
    expect(v.runs[0]?.jobs[0]).toMatchObject({ id: 3, name: "unit", logSize: 10 });
  });
});
