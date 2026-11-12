// The read files a build sends to `asc prefetch --learn` (SPEC §10.2 item 5b): only absolute
// files inside the work tree, outside node_modules, without a query or a virtual module, NUL
// separated; and the profile name the plugins learn into.

import { describe, expect, test } from "bun:test";
import { join } from "node:path";

import { learnInput, profileName } from "../src/core.js";

const top = join(process.platform === "win32" ? "C:\\" : "/", "work", "repo");

describe("learnInput", () => {
  test("keeps the work tree's own files once", () => {
    const a = join(top, "src", "a.ts");
    const b = join(top, "src", "b.tsx");
    const got = learnInput(
      [
        a,
        `${b}?import`,
        a,
        "\0virtual:entry",
        "src/relative.ts",
        join(top, "node_modules", "react", "index.js"),
        join(top, "..", "other", "c.ts"),
      ],
      top,
    );
    expect(got.split("\0")).toEqual([a, b]);
  });

  test("is empty when nothing qualifies", () => {
    expect(learnInput(["\0x", "rel.ts"], top)).toBe("");
  });
});

describe("profileName", () => {
  test("is the tool and the project without spaces", () => {
    const build = { tool: "vite", root: top, outputs: [], parts: {}, envPrefixes: [] };
    expect(profileName(build, { name: "my app" })).toBe("vite:my-app");
  });
});
