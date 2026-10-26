import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { definitions, isSafeServerUrl } from "../../src/mcp";

const doc = readFileSync(join(import.meta.dir, "../../spikes/mcp-oauth.md"), "utf8");

describe("spike-mcp-oauth decision", () => {
  test("the extension offers stdio only and carries no credential", () => {
    const defs = definitions("/bin/asc", [], "https://asc.example.org");
    expect(defs).toHaveLength(1);
    const text = JSON.stringify(defs);
    expect(text).not.toMatch(/authorization|bearer|token|headers/i);
    expect(defs[0]?.args).toEqual(["mcp", "--remote", "https://asc.example.org"]);
  });

  test("plain http and URL credentials are refused", () => {
    expect(isSafeServerUrl("http://asc.example.org")).toBe(false);
    expect(isSafeServerUrl("https://user:pw@asc.example.org")).toBe(false);
    expect(definitions("/bin/asc", [], "http://asc.example.org")).toEqual([]);
  });

  test("the fallback snippet is https, password-prompted and bearer-only", () => {
    const m = /```json\n([\s\S]*?)```/.exec(doc);
    expect(m).not.toBeNull();
    const cfg = JSON.parse(m?.[1] ?? "{}") as {
      inputs: { type: string; id: string; password: boolean }[];
      servers: Record<string, { type: string; url: string; headers: Record<string, string> }>;
    };
    const input = cfg.inputs[0];
    expect(input?.type).toBe("promptString");
    expect(input?.password).toBe(true);
    const server = Object.values(cfg.servers)[0];
    expect(isSafeServerUrl(server?.url ?? "")).toBe(true);
    expect(server?.url.endsWith("/mcp")).toBe(true);
    expect(server?.headers.Authorization).toBe(`Bearer \${input:${input?.id}}`);
    expect(server?.url).not.toContain("token");
  });

  test("the record states the decision and the open item", () => {
    for (const s of [
      "## Decision",
      "## Open item",
      "DCR is not built",
      "resource_metadata",
      "asc:read",
    ]) {
      expect(doc).toContain(s);
    }
  });
});
