import { describe, expect, test } from "bun:test";
import { create, toBinary } from "@bufbuild/protobuf";
import { OpResultSchema, OpStatus, TagDetailSchema, TagEntrySchema } from "@asc/asc-client/cli";
import { decodeOpResult, tagEntries } from "../src/opresult";
import {
  deleteConfirmText,
  describeTag,
  TAG_MESSAGE_MAX,
  TAG_NAME_MAX,
  type TagItem,
  type TagsApi,
  TagsModel,
  tagNameProblem,
} from "../src/views/tags";
import type { RunResult } from "../src/views/worktrees";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const enc = new TextEncoder();

function tagResult(tags: { name: string; annotated?: boolean; subject?: string }[]): RunResult {
  const r = create(OpResultSchema, {
    status: OpStatus.OK,
    detail: {
      case: "tag",
      value: create(TagDetailSchema, {
        tags: tags.map((t) =>
          create(TagEntrySchema, {
            name: t.name,
            target: new Uint8Array(20).fill(0xab),
            annotated: t.annotated ?? false,
            subject: enc.encode(t.subject ?? ""),
          }),
        ),
      }),
    },
  });
  return { exit: 0, opResult: toBinary(OpResultSchema, r), stderr: [] };
}

function fakeApi(o: {
  tags?: string[];
  shared?: Set<string> | null | Error;
  seen?: [string, string[]][];
}): TagsApi & { lists: number } {
  const api = {
    lists: 0,
    watch: async function* () {},
    async run(_root: string, verb: string, args: string[]) {
      o.seen?.push([verb, args]);
      if (verb === "tag --list") api.lists++;
      if (verb === "push") return { exit: 0, opResult: null, stderr: [] };
      return tagResult((o.tags ?? []).map((name) => ({ name })));
    },
    async shared() {
      if (o.shared instanceof Error) throw o.shared;
      return o.shared ?? null;
    },
  };
  return api;
}

describe("tags: opresult", () => {
  test("decodes a tag detail into rows with hex targets", () => {
    const r = tagResult([{ name: "v1", annotated: true, subject: "first" }]);
    const rows = tagEntries(decodeOpResult(r.opResult));
    expect(rows).toEqual([
      { name: "v1", target: "ab".repeat(20), annotated: true, subject: "first" },
    ]);
    expect(tagEntries(null)).toEqual([]);
  });
});

describe("tags: names", () => {
  test("refuses names the bridge or git would refuse", () => {
    expect(tagNameProblem("v1.0.0")).toBeNull();
    expect(tagNameProblem("release/2026")).toBeNull();
    for (const bad of ["", "-x", "a b", "a~1", "a^", "a:b", "a?", "a*", "a[", "a\\b", "a\u0001"]) {
      expect(tagNameProblem(bad)).not.toBeNull();
    }
    expect(tagNameProblem("x".repeat(TAG_NAME_MAX))).toBeNull();
    expect(tagNameProblem("x".repeat(TAG_NAME_MAX + 1))).not.toBeNull();
  });
});

describe("tags: model", () => {
  test("lists only while visible and marks tags the server lacks", async () => {
    const api = fakeApi({ tags: ["v1", "v2"], shared: new Set(["v1"]) });
    const m = new TagsModel({ root: "r", api });
    m.onWatch("ref");
    await sleep(5);
    expect(api.lists).toBe(0);
    m.setVisible(true);
    await sleep(5);
    expect(api.lists).toBe(1);
    expect(m.items.map((t) => [t.name, t.shared])).toEqual([
      ["v1", true],
      ["v2", false],
    ]);
    m.onWatch("paths");
    await sleep(5);
    expect(api.lists).toBe(1);
    m.onWatch("op-finished");
    await sleep(5);
    expect(api.lists).toBe(2);
  });

  test("no server or an unreachable one leaves the mark unknown", async () => {
    const errs: unknown[] = [];
    for (const shared of [null, new Error("down")]) {
      const m = new TagsModel({
        root: "r",
        api: fakeApi({ tags: ["v1"], shared }),
        onError: (e) => errs.push(e),
      });
      await m.refresh();
      expect(m.items[0]?.shared).toBeNull();
    }
    expect(errs).toHaveLength(1);
  });

  test("create and delete send fixed argv and take the returned list", async () => {
    const seen: [string, string[]][] = [];
    const api = fakeApi({ tags: ["v1"], shared: new Set(["v1"]), seen });
    const m = new TagsModel({ root: "r", api });
    await m.refresh();
    api.run = async (_r, verb, args) => {
      seen.push([verb, args]);
      return tagResult([{ name: "v1" }, { name: "v2", annotated: true, subject: "msg" }]);
    };
    await m.create("v2", { message: "  msg  ", rev: "abc" });
    await m.create("v3");
    await m.remove("v1");
    expect(seen.slice(1)).toEqual([
      ["tag", ["-m=msg", "v2", "abc"]],
      ["tag", ["v3"]],
      ["tag --delete", ["v1"]],
    ]);
    expect(m.items.map((t) => [t.name, t.shared])).toEqual([
      ["v1", true],
      ["v2", false],
    ]);
  });

  test("bad input is refused before the bridge is called", async () => {
    const seen: [string, string[]][] = [];
    const m = new TagsModel({ root: "r", api: fakeApi({ seen }) });
    expect((await m.create("-rf")).exit).toBe(2);
    expect((await m.create("v1", { message: "x".repeat(TAG_MESSAGE_MAX + 1) })).exit).toBe(2);
    expect((await m.remove("a b")).exit).toBe(2);
    expect(seen).toEqual([]);
  });

  test("push sends --tags and lists again", async () => {
    const seen: [string, string[]][] = [];
    const api = fakeApi({ tags: ["v1"], shared: new Set(["v1"]), seen });
    const m = new TagsModel({ root: "r", api });
    await m.pushAll();
    expect(seen.map((s) => s[0])).toEqual(["push", "tag --list"]);
    expect(seen[0]?.[1]).toEqual(["--tags"]);
    expect(m.items[0]?.shared).toBe(true);
  });
});

describe("tags: text", () => {
  const t: TagItem = {
    name: "v1",
    target: "ab".repeat(20),
    annotated: true,
    subject: "first",
    shared: false,
  };
  test("describes and confirms from the parsed tag", () => {
    expect(describeTag(t).description).toBe(`${"ab".repeat(6)}  first  not pushed`);
    expect(describeTag(t).tooltip).toContain("Annotated tag v1");
    expect(deleteConfirmText(t)).toBe(`Delete tag v1 (${"ab".repeat(6)})?`);
    expect(deleteConfirmText({ ...t, shared: true })).toContain("stays on the server");
  });
});
