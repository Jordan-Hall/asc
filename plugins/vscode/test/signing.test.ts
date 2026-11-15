import { describe, expect, test } from "bun:test";
import { type CliCall, type CliDirect, type CliResult, cliArgv } from "../src/cli-direct";
import {
  createSigningController,
  parseSigningMessage,
  parseSigningStatus,
  parseSigningTest,
  renderSigningStatus,
  renderSigningTest,
  signingHtml,
} from "../src/views/signing";

const lv = (value: unknown, level: string | null = "global") => ({ value, level });
const status = (key: Record<string, unknown>, configured = true) =>
  JSON.stringify({
    configured,
    key: {
      source: "none",
      path: null,
      id: null,
      fingerprint: null,
      level: null,
      usable: false,
      problem: null,
      ...key,
    },
    format: lv("ssh"),
    commit_sign: lv(true),
    tag_sign: lv(false, null),
    allowed_signers: lv(null, null),
  });

describe("signing: argv of every write", () => {
  test("the key, format and both toggles go as separate argv entries", () => {
    const w = (scope: "global" | "local", key: string, value: string) =>
      cliArgv({ kind: "signing-config", scope, key, value } as CliCall);
    expect(w("global", "user.signingkey", "/home/a b/id.pub")).toEqual([
      "config",
      "--global",
      "user.signingkey",
      "/home/a b/id.pub",
    ]);
    expect(w("local", "user.signingkey", "C:\\keys\\id")).toEqual([
      "config",
      "--local",
      "user.signingkey",
      "C:\\keys\\id",
    ]);
    expect(w("local", "gpg.format", "x509")).toEqual(["config", "--local", "gpg.format", "x509"]);
    expect(w("global", "commit.gpgsign", "true")).toEqual([
      "config",
      "--global",
      "commit.gpgsign",
      "true",
    ]);
    expect(w("global", "tag.gpgsign", "false")).toEqual([
      "config",
      "--global",
      "tag.gpgsign",
      "false",
    ]);
    expect(w("global", "gpg.ssh.allowedSignersFile", "/a/signers")).toEqual([
      "config",
      "--global",
      "gpg.ssh.allowedSignersFile",
      "/a/signers",
    ]);
  });
  test("read verbs", () => {
    expect(cliArgv({ kind: "signing-status" })).toEqual(["signing", "status", "--json"]);
    expect(cliArgv({ kind: "signing-test" })).toEqual(["signing", "test", "--json"]);
  });
  test("out-of-range keys, scopes and values have no argv", () => {
    const w = (scope: string, key: string, value: string) =>
      cliArgv({ kind: "signing-config", scope, key, value } as unknown as CliCall);
    expect(w("system", "gpg.format", "ssh")).toBeNull();
    expect(w("global", "core.sshCommand", "/bin/sh")).toBeNull();
    expect(w("global", "gpg.format", "pgp; rm")).toBeNull();
    expect(w("global", "commit.gpgsign", "yes")).toBeNull();
    expect(w("global", "user.signingkey", "--file=/x")).toBeNull();
    expect(w("global", "user.signingkey", "relative/key")).toBeNull();
    expect(w("global", "user.signingkey", "/a\nb")).toBeNull();
  });
});

describe("signing: webview messages", () => {
  test("allowed messages pass", () => {
    expect(parseSigningMessage({ type: "ready" })).toEqual({ type: "ready" });
    expect(parseSigningMessage({ type: "setFormat", value: "ssh" })).toEqual({
      type: "setFormat",
      value: "ssh",
    });
    expect(parseSigningMessage({ type: "setTagSign", value: false })).toEqual({
      type: "setTagSign",
      value: false,
    });
    expect(parseSigningMessage({ type: "setScope", value: "local" })).toEqual({
      type: "setScope",
      value: "local",
    });
  });
  test("unknown types, keys and values are rejected", () => {
    for (const m of [
      null,
      "ready",
      [],
      {},
      { type: "run" },
      { type: "ready", extra: 1 },
      { type: "pickKey", path: "/etc/passwd" },
      { type: "setKey", value: "/x" },
      { type: "setFormat", value: "pgp" },
      { type: "setFormat", value: 1 },
      { type: "setCommitSign", value: "true" },
      { type: "setScope", value: "system" },
      { type: "setScope", value: "global", key: "x" },
    ]) {
      expect(parseSigningMessage(m)).toBeNull();
    }
  });
});

describe("signing: status rendering", () => {
  const render = (s: string) => {
    const p = parseSigningStatus(s);
    if (p === null) throw new Error("unparsed");
    return renderSigningStatus(p, "global");
  };
  test("none", () => {
    const v = render(status({}, false));
    expect(v.headline).toBe("Signing is not set up.");
    expect(v.ok).toBe(false);
    expect(v.rows[0]).toEqual({ label: "Key source", value: "No signing key" });
    expect(v.rows.some((r) => r.label === "Status")).toBe(false);
  });
  test("file", () => {
    const v = render(
      status({
        source: "file",
        path: "/k/id",
        fingerprint: "SHA256:abc",
        level: "local",
        usable: true,
      }),
    );
    expect(v.ok).toBe(true);
    expect(v.rows).toContainEqual({ label: "Key source", value: "Key file (local)" });
    expect(v.rows).toContainEqual({ label: "Key path", value: "/k/id" });
    expect(v.rows).toContainEqual({ label: "Fingerprint", value: "SHA256:abc" });
    expect(v.rows).toContainEqual({ label: "Status", value: "Usable" });
    expect(v.rows).toContainEqual({ label: "Format", value: "ssh (global)" });
    expect(v.rows).toContainEqual({ label: "Sign commits", value: "On (global)" });
    expect(v.rows).toContainEqual({ label: "Sign tags", value: "Off" });
    expect(v.commitSign).toBe(true);
    expect(v.format).toBe("ssh");
  });
  test("agent", () => {
    const v = render(status({ source: "agent", id: "ssh-ed25519 AAAA", usable: true }));
    expect(v.rows[0]?.value).toBe("SSH agent");
    expect(v.rows).toContainEqual({ label: "Key ID", value: "ssh-ed25519 AAAA" });
    expect(v.headline).toBe("Signing is ready.");
  });
  test("unusable", () => {
    const v = render(status({ source: "file", path: "/k/id", problem: "file not found" }));
    expect(v.ok).toBe(false);
    expect(v.headline).toBe("Signing is set up but the key is not usable.");
    expect(v.rows).toContainEqual({ label: "Status", value: "Not usable: file not found" });
  });
  test("a malformed status is refused", () => {
    expect(parseSigningStatus("not json")).toBeNull();
    expect(parseSigningStatus("{}")).toBeNull();
    expect(parseSigningStatus(status({ source: "bogus" }))).toBeNull();
  });
});

describe("signing: test result rendering", () => {
  test("ok", () => {
    const r = parseSigningTest(
      JSON.stringify({ ok: true, format: "ssh", source: "file", fingerprint: "SHA256:x", error: null }),
    );
    expect(r && renderSigningTest(r)).toEqual({ ok: true, text: "Test passed: ssh, file, SHA256:x." });
  });
  test("failure", () => {
    const r = parseSigningTest(
      JSON.stringify({ ok: false, format: null, source: null, fingerprint: null, error: "no key" }),
    );
    expect(r && renderSigningTest(r)).toEqual({ ok: false, text: "Test failed: no key" });
    expect(parseSigningTest("{}")).toBeNull();
  });
});

describe("signing: controller and page", () => {
  const run = (statusOut = status({ source: "file", usable: true })) => {
    const calls: CliCall[] = [];
    const posted: unknown[] = [];
    const errors: string[] = [];
    const cli: CliDirect = {
      async run(_root, call): Promise<CliResult> {
        calls.push(call);
        return { exit: 0, stdout: call.kind === "signing-test" ? '{"ok":true}' : statusOut, stderr: [] };
      },
    };
    const ctl = createSigningController({
      cli,
      root: "/r",
      host: {
        pickKeyFile: async () => "/picked/key",
        post: (m) => posted.push(m),
        error: (m) => errors.push(m),
      },
    });
    return { ctl, calls, posted, errors };
  };
  test("the picked path is written to the chosen scope, then status refreshes", async () => {
    const { ctl, calls, posted } = run();
    await ctl.handle({ type: "setScope", value: "local" });
    await ctl.handle({ type: "pickKey" });
    expect(calls.map((c) => c.kind)).toEqual(["signing-status", "signing-config", "signing-status"]);
    expect(calls[1]).toEqual({
      kind: "signing-config",
      scope: "local",
      key: "user.signingkey",
      value: "/picked/key",
    });
    expect(posted.at(-1)).toMatchObject({ type: "status" });
  });
  test("an invalid message does nothing; test posts a rendered result", async () => {
    const { ctl, calls, posted } = run();
    await ctl.handle({ type: "setFormat", value: "evil" });
    expect(calls).toEqual([]);
    await ctl.handle({ type: "test" });
    expect(posted).toEqual([{ type: "test", result: { ok: true, text: "Test passed." } }]);
  });
  test("page has no inline script and uses the shared CSP", () => {
    const h = signingHtml({ cspSource: "vscode-webview://x", scriptUri: "s.js", styleUri: "s.css" });
    expect(h).toContain("script-src vscode-webview://x");
    expect(h).not.toMatch(/<script>/);
    expect(h).not.toMatch(/<script[^>]*>[^<]+<\/script>/);
  });
});
