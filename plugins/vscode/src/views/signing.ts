// Signing settings page (roadmap row 60c). A webview shows `asc signing status --json` and offers
// controls that write through `asc config --global|--local <key> <value>`. The key file comes from
// the native file picker in the host, never from the webview; the webview sends only messages from a
// fixed allowlist, every value is validated here, and the argv is built in cli-direct.ts. No key
// content is read or sent: the page only ever holds a path, a fingerprint and short labels, and the
// webview sets all text with `textContent`.
import type * as vscodeTypes from "vscode";
import {
  type CliDirect,
  SIGNING_FORMATS,
  type SigningConfigKey,
  type SigningScope,
} from "../cli-direct";
import { buildCsp, escapeAttr } from "../webview/csp";

export const SIGNING_VIEW_TYPE = "asc.signing";
export const SIGNING_TITLE = "asc Signing";

const SOURCES = ["none", "file", "public-key-file", "literal", "agent", "key-id"] as const;
const LEVELS = ["global", "local", "system", "command"] as const;
const MAX_TEXT = 4096;

export interface SigningLevelled<T> {
  value: T;
  level: string | null;
}

export interface SigningStatus {
  configured: boolean;
  key: {
    source: string;
    path: string | null;
    id: string | null;
    fingerprint: string | null;
    level: string | null;
    usable: boolean;
    problem: string | null;
  };
  format: SigningLevelled<string>;
  commitSign: SigningLevelled<boolean>;
  tagSign: SigningLevelled<boolean>;
  allowedSigners: SigningLevelled<string | null>;
}

export interface SigningTestResult {
  ok: boolean;
  format: string | null;
  source: string | null;
  fingerprint: string | null;
  error: string | null;
}

type Obj = Record<string, unknown>;
const isObj = (v: unknown): v is Obj => typeof v === "object" && v !== null && !Array.isArray(v);
const text = (v: unknown): string | null =>
  typeof v === "string" && v.length <= MAX_TEXT ? v : null;
const level = (v: unknown): string | null =>
  typeof v === "string" && (LEVELS as readonly string[]).includes(v) ? v : null;

function levelled<T>(v: unknown, pick: (x: unknown) => T | undefined): SigningLevelled<T> | null {
  if (!isObj(v)) return null;
  const value = pick(v.value);
  return value === undefined ? null : { value, level: level(v.level) };
}

function parseJson(stdout: string): unknown {
  try {
    return JSON.parse(stdout);
  } catch {
    return null;
  }
}

/** Parses `asc signing status --json`; null when the shape is wrong. */
export function parseSigningStatus(stdout: string): SigningStatus | null {
  const j = parseJson(stdout);
  if (!isObj(j) || typeof j.configured !== "boolean" || !isObj(j.key)) return null;
  const k = j.key;
  const source = text(k.source);
  if (source === null || !(SOURCES as readonly string[]).includes(source)) return null;
  if (typeof k.usable !== "boolean") return null;
  const str = (x: unknown) => (typeof x === "string" ? x : x === null ? null : undefined);
  const bool = (x: unknown) => (typeof x === "boolean" ? x : undefined);
  const format = levelled(j.format, (x) => (typeof x === "string" ? x : undefined));
  const commit = levelled(j.commit_sign, bool);
  const tag = levelled(j.tag_sign, bool);
  const allowed = levelled(j.allowed_signers, str);
  if (!format || !commit || !tag || !allowed) return null;
  return {
    configured: j.configured,
    key: {
      source,
      path: text(k.path),
      id: text(k.id),
      fingerprint: text(k.fingerprint),
      level: level(k.level),
      usable: k.usable,
      problem: text(k.problem),
    },
    format,
    commitSign: commit,
    tagSign: tag,
    allowedSigners: allowed,
  };
}

/** Parses `asc signing test --json`. */
export function parseSigningTest(stdout: string): SigningTestResult | null {
  const j = parseJson(stdout);
  if (!isObj(j) || typeof j.ok !== "boolean") return null;
  return {
    ok: j.ok,
    format: text(j.format),
    source: text(j.source),
    fingerprint: text(j.fingerprint),
    error: text(j.error),
  };
}

export interface SigningRow {
  label: string;
  value: string;
}
/** What the webview shows; every field is plain text the page assigns with `textContent`. */
export interface SigningView {
  headline: string;
  ok: boolean;
  rows: SigningRow[];
  scope: SigningScope;
  format: string;
  commitSign: boolean;
  tagSign: boolean;
}

const SOURCE_LABEL: Record<string, string> = {
  none: "No signing key",
  file: "Key file",
  "public-key-file": "Public key file",
  literal: "Key in the settings",
  agent: "SSH agent",
  "key-id": "Key ID",
};

const at = (l: string | null): string => (l === null ? "" : ` (${l})`);
const onOff = (v: SigningLevelled<boolean>): string => `${v.value ? "On" : "Off"}${at(v.level)}`;

/** The rows and control state for `s`, plus the scope the writes go to. */
export function renderSigningStatus(s: SigningStatus, scope: SigningScope): SigningView {
  const rows: SigningRow[] = [
    { label: "Key source", value: (SOURCE_LABEL[s.key.source] ?? s.key.source) + at(s.key.level) },
  ];
  if (s.key.path !== null) rows.push({ label: "Key path", value: s.key.path });
  if (s.key.id !== null) rows.push({ label: "Key ID", value: s.key.id });
  if (s.key.fingerprint !== null) rows.push({ label: "Fingerprint", value: s.key.fingerprint });
  if (s.key.source !== "none") {
    rows.push({
      label: "Status",
      value: s.key.usable ? "Usable" : `Not usable: ${s.key.problem ?? "unknown problem"}`,
    });
  }
  rows.push(
    { label: "Format", value: s.format.value + at(s.format.level) },
    { label: "Sign commits", value: onOff(s.commitSign) },
    { label: "Sign tags", value: onOff(s.tagSign) },
  );
  if (s.allowedSigners.value !== null) {
    rows.push({ label: "Allowed signers", value: s.allowedSigners.value });
  }
  const headline =
    s.key.source === "none" || !s.configured
      ? "Signing is not set up."
      : s.key.usable
        ? "Signing is ready."
        : "Signing is set up but the key is not usable.";
  return {
    headline,
    ok: s.configured && s.key.usable,
    rows,
    scope,
    format: s.format.value,
    commitSign: s.commitSign.value,
    tagSign: s.tagSign.value,
  };
}

/** One line for a `signing test` result. */
export function renderSigningTest(r: SigningTestResult): { ok: boolean; text: string } {
  if (r.ok) {
    const parts = [r.format, r.source, r.fingerprint].filter((x): x is string => x !== null);
    return { ok: true, text: `Test passed${parts.length ? `: ${parts.join(", ")}` : ""}.` };
  }
  return { ok: false, text: `Test failed: ${r.error ?? "unknown error"}` };
}

/** Messages the webview may send; anything else is dropped. */
export type SigningMessage =
  | { type: "ready" }
  | { type: "refresh" }
  | { type: "test" }
  | { type: "pickKey" }
  | { type: "setFormat"; value: string }
  | { type: "setCommitSign"; value: boolean }
  | { type: "setTagSign"; value: boolean }
  | { type: "setScope"; value: SigningScope };

/** Validates a webview message against the fixed allowlist of types, keys and values. */
export function parseSigningMessage(raw: unknown): SigningMessage | null {
  if (!isObj(raw) || typeof raw.type !== "string") return null;
  const keys = Object.keys(raw);
  const only = (...allowed: string[]) => keys.every((k) => allowed.includes(k));
  switch (raw.type) {
    case "ready":
    case "refresh":
    case "test":
    case "pickKey":
      return only("type") ? { type: raw.type } : null;
    case "setFormat":
      return only("type", "value") &&
        typeof raw.value === "string" &&
        (SIGNING_FORMATS as readonly string[]).includes(raw.value)
        ? { type: "setFormat", value: raw.value }
        : null;
    case "setCommitSign":
    case "setTagSign":
      return only("type", "value") && typeof raw.value === "boolean"
        ? { type: raw.type, value: raw.value }
        : null;
    case "setScope":
      return only("type", "value") && (raw.value === "global" || raw.value === "local")
        ? { type: "setScope", value: raw.value }
        : null;
    default:
      return null;
  }
}

/** The page shell: the script and stylesheet are files of the extension, never inline. */
export function signingHtml(o: { cspSource: string; scriptUri: string; styleUri: string }): string {
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${escapeAttr(buildCsp(o.cspSource))}">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="stylesheet" href="${escapeAttr(o.styleUri)}">
<title>${SIGNING_TITLE}</title>
</head>
<body>
<main>
<h1>Signing</h1>
<p id="headline" role="status"></p>
<dl id="rows"></dl>
<fieldset>
<legend>Settings</legend>
<label>Write to <select id="scope"><option value="global">Global</option><option value="local">This repository</option></select></label>
<button id="pickKey" type="button">Choose key file...</button>
<label>Format <select id="format"><option value="openpgp">OpenPGP</option><option value="ssh">SSH</option><option value="x509">X.509</option></select></label>
<label><input id="commitSign" type="checkbox"> Sign commits</label>
<label><input id="tagSign" type="checkbox"> Sign tags</label>
</fieldset>
<button id="test" type="button">Test signing</button>
<p id="testResult" role="status"></p>
</main>
<script type="module" src="${escapeAttr(o.scriptUri)}"></script>
</body>
</html>
`;
}

/** The slice of the editor the page uses; the real one is wired in `registerSigning`. */
export interface SigningHost {
  /** Opens the native picker; the chosen absolute path, or null when cancelled. */
  pickKeyFile(): Promise<string | null>;
  post(message: unknown): void;
  error(message: string): void;
}

export interface SigningController {
  handle(raw: unknown): Promise<void>;
}

const msg = (e: unknown): string => String(e instanceof Error ? e.message : e);

export function createSigningController(o: {
  cli: CliDirect;
  root: string;
  host: SigningHost;
}): SigningController {
  let scope: SigningScope = "global";
  const refresh = async (): Promise<void> => {
    let status: SigningStatus | null = null;
    try {
      const r = await o.cli.run(o.root, { kind: "signing-status" });
      status = parseSigningStatus(r.stdout);
    } catch (e) {
      o.host.error(msg(e));
      return;
    }
    if (status === null) o.host.error("asc: the signing status could not be read.");
    else o.host.post({ type: "status", view: renderSigningStatus(status, scope) });
  };
  const write = async (key: SigningConfigKey, value: string): Promise<void> => {
    try {
      const r = await o.cli.run(o.root, { kind: "signing-config", scope, key, value });
      if (r.exit !== 0) o.host.error(`asc config failed: ${r.stderr.at(-1) ?? `exit ${r.exit}`}`);
    } catch (e) {
      o.host.error(msg(e));
    }
    await refresh();
  };
  return {
    async handle(raw) {
      const m = parseSigningMessage(raw);
      if (m === null) return;
      switch (m.type) {
        case "ready":
        case "refresh":
          return refresh();
        case "setScope":
          scope = m.value;
          return refresh();
        case "pickKey": {
          const p = await o.host.pickKeyFile();
          return p === null ? undefined : write("user.signingkey", p);
        }
        case "setFormat":
          return write("gpg.format", m.value);
        case "setCommitSign":
          return write("commit.gpgsign", String(m.value));
        case "setTagSign":
          return write("tag.gpgsign", String(m.value));
        case "test": {
          try {
            const r = await o.cli.run(o.root, { kind: "signing-test" });
            const t = parseSigningTest(r.stdout);
            o.host.post({
              type: "test",
              result:
                t === null ? { ok: false, text: "Test failed: no result." } : renderSigningTest(t),
            });
          } catch (e) {
            o.host.error(msg(e));
          }
        }
      }
    },
  };
}

/** Registers `asc.openSigningSettings`. */
export function registerSigning(
  vscode: typeof vscodeTypes,
  ctx: vscodeTypes.ExtensionContext,
  cli: CliDirect,
  roots: () => readonly string[],
): vscodeTypes.Disposable {
  let panel: vscodeTypes.WebviewPanel | undefined;
  const cmd = vscode.commands.registerCommand("asc.openSigningSettings", () => {
    const root = roots()[0];
    if (root === undefined) return;
    if (panel) {
      panel.reveal();
      return;
    }
    const media = vscode.Uri.joinPath(ctx.extensionUri, "media");
    const p = vscode.window.createWebviewPanel(
      SIGNING_VIEW_TYPE,
      SIGNING_TITLE,
      vscode.ViewColumn.Active,
      { enableScripts: true, localResourceRoots: [media] },
    );
    panel = p;
    p.webview.html = signingHtml({
      cspSource: p.webview.cspSource,
      scriptUri: p.webview.asWebviewUri(vscode.Uri.joinPath(media, "signing.js")).toString(),
      styleUri: p.webview.asWebviewUri(vscode.Uri.joinPath(media, "signing.css")).toString(),
    });
    const ctl = createSigningController({
      cli,
      root,
      host: {
        async pickKeyFile() {
          const r = await vscode.window.showOpenDialog({
            canSelectMany: false,
            canSelectFiles: true,
            canSelectFolders: false,
            openLabel: "Use as signing key",
          });
          return r?.[0]?.scheme === "file" ? r[0].fsPath : null;
        },
        post: (m) => void p.webview.postMessage(m),
        error: (m) => void vscode.window.showErrorMessage(m),
      },
    });
    const sub = p.webview.onDidReceiveMessage((m: unknown) => void ctl.handle(m));
    p.onDidDispose(() => {
      sub.dispose();
      panel = undefined;
    });
  });
  return {
    dispose() {
      cmd.dispose();
      panel?.dispose();
    },
  };
}
