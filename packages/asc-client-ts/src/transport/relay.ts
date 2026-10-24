// `relay` transport: the extension-host side of the `host` transport (SPEC 24.4.5, 24.4.5.1, IP-D4).
// It receives untrusted webview messages and forwards one to the real bridge transport only when it
// matches an entry of allowlist.json. Deny by default: an unknown method, an unclassified or `never`
// RPC, a verb or flag outside the schema, a Forgejo pair outside the list, a body field outside the
// pair's list, or a request over a limit is dropped, logged without its payload, and answered with
// PERMISSION_DENIED. Confirmation text is composed here from the parsed request, never taken from the
// webview. The relay never sees or stores a Forgejo token: the injected `forgejo` executor adds it.
import { isAbsolute, relative, resolve } from "node:path";
import { fromBinary, toBinary } from "@bufbuild/protobuf";
import { RunRequestSchema } from "../gen/asc/v1/bridge_pb";
import { RequestMergeRequestSchema } from "../gen/asc/v1/changes_pb";
import { WsStageRequestSchema } from "../gen/asc/v1/workspace_pb";
import { BridgeError, type BridgeTransport } from "./bridge-node";
import { MAX_FRAME } from "./frame";
import type { ForgejoReply, ForgejoRequest, HostReply } from "./host";

export type Confirm = "none" | "modal";

export interface WriteEntry {
  name: string;
  confirm: Confirm;
  limits: { patchBytes?: number; paths?: number };
}
export interface VerbEntry {
  flags: Record<string, string>;
  pos: string;
  confirm: Confirm;
}
export interface ForgejoPair {
  m: string;
  p: string;
  body?: string[];
  confirm: Confirm;
}

/** The checked form of allowlist.json. */
export interface Allowlist {
  read: ReadonlySet<string>;
  write: ReadonlyMap<string, WriteEntry>;
  reject: ReadonlySet<string>;
  maxArgs: number;
  maxArgBytes: number;
  verbs: ReadonlyMap<string, VerbEntry>;
  pairs: readonly ForgejoPair[];
}

type Json = Record<string, unknown>;
const isObj = (v: unknown): v is Json => typeof v === "object" && v !== null && !Array.isArray(v);

function strs(v: unknown, what: string): string[] {
  if (!Array.isArray(v) || !v.every((x) => typeof x === "string")) {
    throw new Error(`${what}: strings expected`);
  }
  return v as string[];
}

function conf(v: unknown, what: string): Confirm {
  if (v === undefined || v === "") return "none";
  if (v === "none" || v === "modal") return v;
  throw new Error(`${what}: bad confirm`);
}

/** Checks the shape of a parsed allowlist.json and returns the lookup form. Throws on anything odd. */
export function parseAllowlist(json: unknown): Allowlist {
  if (
    !isObj(json) ||
    json.version !== 1 ||
    !isObj(json.rpc) ||
    !isObj(json.run) ||
    !isObj(json.forgejo)
  ) {
    throw new Error("allowlist: version 1 expected");
  }
  const write = new Map<string, WriteEntry>();
  for (const w of Array.isArray(json.rpc.write) ? json.rpc.write : []) {
    if (!isObj(w) || typeof w.name !== "string") throw new Error("allowlist: bad write entry");
    const limits = isObj(w.limits) ? w.limits : {};
    for (const [k, v] of Object.entries(limits)) {
      if ((k !== "patchBytes" && k !== "paths") || typeof v !== "number") {
        throw new Error(`allowlist: unknown limit ${k}`);
      }
    }
    write.set(w.name, {
      name: w.name,
      confirm: conf(w.confirm, w.name),
      limits: limits as WriteEntry["limits"],
    });
  }
  const verbs = new Map<string, VerbEntry>();
  for (const [name, v] of Object.entries(isObj(json.run.verbs) ? json.run.verbs : {})) {
    const e = isObj(v) ? v : {};
    const flags = isObj(e.flags) ? e.flags : {};
    for (const t of Object.values(flags)) {
      if (typeof t !== "string") throw new Error("allowlist: bad flag type");
    }
    verbs.set(name, {
      flags: flags as Record<string, string>,
      pos: typeof e.pos === "string" ? e.pos : "",
      confirm: conf(e.confirm, name),
    });
  }
  const pairs: ForgejoPair[] = [];
  for (const p of Array.isArray(json.forgejo.pairs) ? json.forgejo.pairs : []) {
    if (!isObj(p) || typeof p.m !== "string" || typeof p.p !== "string") {
      throw new Error("allowlist: bad pair");
    }
    pairs.push({
      m: p.m,
      p: p.p,
      ...(p.body === undefined ? {} : { body: strs(p.body, "pair body") }),
      confirm: conf(p.confirm, p.p),
    });
  }
  const run = json.run;
  if (typeof run.maxArgs !== "number" || typeof run.maxArgBytes !== "number") {
    throw new Error("allowlist: run limits missing");
  }
  return {
    read: new Set(strs(json.rpc.read, "rpc.read")),
    write,
    reject: new Set(strs(run.reject, "run.reject")),
    maxArgs: run.maxArgs,
    maxArgBytes: run.maxArgBytes,
    verbs,
    pairs,
  };
}

/** What a request turned out to be, after parsing. The confirmation text is built from this. */
export type Parsed =
  | { kind: "rpc"; method: string }
  | { kind: "write"; method: string; paths: number; patchBytes: number; change?: string }
  | { kind: "run"; verb: string; args: string[] }
  | {
      kind: "forgejo";
      method: string;
      template: string;
      params: Record<string, string>;
      body?: Json;
    };

export type Decision =
  | {
      ok: true;
      parsed: Parsed;
      confirm: Confirm;
      /** The request bytes to forward (re-encoded for writes, so unknown fields cannot ride along). */
      bytes?: Uint8Array;
    }
  | { ok: false; reason: string };

const no = (reason: string): Decision => ({ ok: false, reason });

// ---- Run verbs (mirror of argv_schema.rs; the bridge stays the authority) ----

type Ty =
  | { t: "bool" }
  | { t: "str"; max: number }
  | { t: "int"; max: number }
  | { t: "rev" | "path" | "tmpfile" | "user" };

function ty(word: string): Ty | undefined {
  if (word === "bool") return { t: "bool" };
  if (word === "rev" || word === "path" || word === "tmpfile" || word === "user") {
    return { t: word };
  }
  const s = /^(str|int):(\d+)$/.exec(word);
  if (s) return { t: s[1] as "str" | "int", max: Number(s[2]) };
  return undefined;
}

const USER = /^[A-Za-z0-9._-]{1,40}$/;
const hasControl = (s: string): boolean => {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c < 0x20 || c === 0x7f) return true;
  }
  return false;
};
const isRev = (s: string): boolean =>
  s.length >= 1 && s.length <= 256 && !s.startsWith("-") && !/^\s/.test(s) && !hasControl(s);

export interface RunScope {
  /** The opened workspace root the call runs in. */
  root: string;
  /** `$ASC_PLUGIN_TMP`, the per-session directory the host writes plan files into. */
  tmpDir?: string | undefined;
}

function checkValue(t: Ty, v: string, scope: RunScope, limit: number): string | undefined {
  if (v.includes("\0")) return "NUL byte";
  if (v.length > limit) return "too long";
  switch (t.t) {
    case "bool":
      return "takes no value";
    case "str":
      return (v.length === 0 && t.max <= 1024) || v.length > t.max ? "bad length" : undefined;
    case "int":
      return /^\d{1,9}$/.test(v) && Number(v) <= t.max ? undefined : "bad number";
    case "rev":
      return isRev(v) ? undefined : "not a revision";
    case "user":
      return USER.test(v) ? undefined : "not a user";
    case "path": {
      if (v === "" || /[\r\n]/.test(v)) return "bad path";
      const rel = relative(scope.root, resolve(scope.root, v));
      return rel.startsWith("..") || isAbsolute(rel) ? "path leaves the workspace" : undefined;
    }
    case "tmpfile": {
      if (!scope.tmpDir || !isAbsolute(v) || /[\r\n]/.test(v)) return "bad temporary file";
      const rel = relative(resolve(scope.tmpDir), resolve(v));
      return rel === "" || rel.startsWith("..") || isAbsolute(rel)
        ? "temporary file leaves its directory"
        : undefined;
    }
  }
}

export function checkRun(
  al: Allowlist,
  verb: string,
  args: readonly string[],
  scope: RunScope,
): Decision {
  const entry = al.verbs.get(verb);
  if (!entry) return no("verb not allowed");
  if (args.length > al.maxArgs) return no("too many arguments");
  const seen = new Set<string>();
  const positional: string[] = [];
  for (const arg of args) {
    if (arg.length > al.maxArgBytes + 8 || arg.includes("\0")) return no("bad argument");
    if (!arg.startsWith("-")) {
      positional.push(arg);
      continue;
    }
    const eq = arg.indexOf("=");
    const name = eq < 0 ? arg : arg.slice(0, eq);
    const value = eq < 0 ? undefined : arg.slice(eq + 1);
    if (al.reject.has(name)) return no("flag rejected");
    const spec = Object.hasOwn(entry.flags, name) ? entry.flags[name] : undefined;
    if (spec === undefined) return no("unknown flag");
    const repeat = spec.endsWith("*");
    const t = ty(repeat ? spec.slice(0, -1) : spec);
    if (!t) return no("malformed schema");
    if (!repeat && seen.has(name)) return no("flag given twice");
    seen.add(name);
    if (t.t === "bool") {
      if (value !== undefined) return no("flag takes no value");
    } else {
      if (value === undefined) return no("flag needs a value");
      if (checkValue(t, value, scope, al.maxArgBytes)) return no("bad flag value");
    }
  }
  let next = 0;
  for (const w of entry.pos.split(" ").filter(Boolean)) {
    const m = /^(.*?)([?*+]?)$/.exec(w);
    const t = ty(m?.[1] ?? "");
    if (!t) return no("malformed schema");
    const left = positional.length - next;
    const mult = m?.[2] ?? "";
    const take = mult === "" ? 1 : mult === "?" ? Math.min(left, 1) : left;
    if (take > left || (mult === "+" && take === 0)) return no("missing argument");
    for (const g of positional.slice(next, next + take)) {
      if (checkValue(t, g, scope, al.maxArgBytes)) return no("bad argument");
    }
    next += take;
  }
  if (next < positional.length) return no("too many arguments");
  return { ok: true, parsed: { kind: "run", verb, args: [...args] }, confirm: entry.confirm };
}

// ---- RPCs ----

const hex = (b: Uint8Array): string =>
  Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");

export function checkRpc(
  al: Allowlist,
  method: string,
  bytes: Uint8Array,
  scope: RunScope,
): Decision {
  if (method === "asc.v1.Bridge/Run") {
    let req;
    try {
      req = fromBinary(RunRequestSchema, bytes);
    } catch {
      return no("undecodable request");
    }
    const d = checkRun(al, req.verb, req.args, scope);
    if (!d.ok) return d;
    return { ...d, bytes: toBinary(RunRequestSchema, req) };
  }
  if (al.read.has(method)) return { ok: true, parsed: { kind: "rpc", method }, confirm: "none" };
  const w = al.write.get(method);
  if (!w) return no("rpc not allowed");
  try {
    if (method === "asc.v1.Workspace/Stage") {
      const req = fromBinary(WsStageRequestSchema, bytes);
      const paths = req.add.length + req.remove.length;
      if (w.limits.patchBytes !== undefined && req.patch.length > w.limits.patchBytes) {
        return no("patch too large");
      }
      if (w.limits.paths !== undefined && paths > w.limits.paths) return no("too many paths");
      return {
        ok: true,
        parsed: { kind: "write", method, paths, patchBytes: req.patch.length },
        confirm: w.confirm,
        bytes: toBinary(WsStageRequestSchema, req),
      };
    }
    if (method === "asc.v1.Changes/RequestMerge") {
      const req = fromBinary(RequestMergeRequestSchema, bytes);
      return {
        ok: true,
        parsed: { kind: "write", method, paths: 0, patchBytes: 0, change: hex(req.change) },
        confirm: w.confirm,
        bytes: toBinary(RequestMergeRequestSchema, req),
      };
    }
  } catch {
    return no("undecodable request");
  }
  // A write entry without a request check here must never pass.
  return no("write rpc has no request check");
}

// ---- Forgejo ----

const SEGMENT = /^[A-Za-z0-9._~-]+$/;
const MERGE_DO = new Set([
  "merge",
  "rebase",
  "rebase-merge",
  "squash",
  "fast-forward-only",
  "manually-merged",
]);

export interface ForgeScope {
  /** `owner/repo` of the forge repos of the opened asc roots. */
  repos: readonly string[];
}

export function checkForgejo(al: Allowlist, req: ForgejoRequest, scope: ForgeScope): Decision {
  if (typeof req.method !== "string" || typeof req.path !== "string") {
    return no("bad forgejo request");
  }
  const [rawPath = "", query] = req.path.split("?", 2) as [string, string | undefined];
  if (/[\\#]|%2e|%2f|%5c|%00/i.test(req.path) || hasControl(req.path) || !rawPath.startsWith("/")) {
    return no("bad path");
  }
  if (query !== undefined && !/^[A-Za-z0-9_.~%=&,+-]*$/.test(query)) return no("bad query");
  const segs = rawPath.split("/").slice(1);
  if (segs.some((s) => !SEGMENT.test(s) || s === "." || s === "..")) return no("bad path");
  for (const pair of al.pairs) {
    if (pair.m !== req.method) continue;
    const want = pair.p.split("/").slice(1);
    if (want.length !== segs.length) continue;
    const params: Record<string, string> = {};
    let hit = true;
    for (const [i, w] of want.entries()) {
      const g = segs[i] ?? "";
      const name = /^\{(.+)\}$/.exec(w)?.[1];
      if (name === undefined) {
        if (w !== g) hit = false;
      } else if ((name === "n" || name === "id") && !/^\d{1,18}$/.test(g)) {
        hit = false;
      } else {
        params[name] = g;
      }
    }
    if (!hit) continue;
    if (params.o !== undefined && !scope.repos.includes(`${params.o}/${params.r}`)) {
      return no("repo not opened");
    }
    let body: Json | undefined;
    if (pair.body) {
      if (req.body !== undefined && !isObj(req.body)) return no("bad body");
      body = req.body ?? {};
      for (const k of Object.keys(body)) {
        if (!pair.body.includes(k)) return no("body field not allowed");
      }
      if (pair.p.endsWith("/reviews") && "event" in body) return no("pending review only");
      if ("Do" in body && (typeof body.Do !== "string" || !MERGE_DO.has(body.Do))) {
        return no("bad merge mode");
      }
    } else if (req.body !== undefined) {
      return no("no body allowed");
    }
    return {
      ok: true,
      parsed: {
        kind: "forgejo",
        method: pair.m,
        template: pair.p,
        params,
        ...(body ? { body } : {}),
      },
      confirm: pair.confirm,
    };
  }
  return no("forgejo operation not allowed");
}

/** Native confirmation text, composed only from the parsed request. */
export function describe(p: Parsed): { title: string; targets: string[] } {
  switch (p.kind) {
    case "run":
      return {
        title: `Run "asc ${p.verb}"`,
        targets: p.args.map((a) => (a.length > 120 ? `${a.slice(0, 117)}...` : a)),
      };
    case "write":
      return {
        title: p.method.endsWith("RequestMerge") ? "Merge change" : "Stage changes",
        targets: p.change ? [p.change] : [`${p.paths} path(s)`],
      };
    case "forgejo": {
      const targets = Object.entries(p.params).map(([k, v]) => `${k}=${v}`);
      const title = p.template.endsWith("/merge")
        ? "Merge pull request"
        : p.template.endsWith("/reviews/{id}")
          ? "Submit review"
          : `${p.method} ${p.template}`;
      return { title, targets };
    }
    case "rpc":
      return { title: p.method, targets: [] };
  }
}

// ---- The relay ----

export interface ConfirmRequest {
  title: string;
  targets: string[];
  parsed: Parsed;
}

export interface RelayLog {
  event: "dropped" | "denied" | "upstream-error";
  reason: string;
  /** The RPC name or "forgejo"; never request payloads. */
  what?: string | undefined;
}

export interface RelayOptions {
  upstream: BridgeTransport;
  allowlist: Allowlist;
  /** Sends a reply to the webview. */
  send(reply: HostReply): void;
  /** Absolute roots the user opened; a call's root must be one of them (or empty). */
  roots(): readonly string[];
  /** `owner/repo` of the forge repos of the opened roots. */
  forgeRepos(): readonly string[];
  tmpDir?: string | undefined;
  /** Shows the native modal. Without it, every entry that needs confirmation is denied. */
  confirm?: ((c: ConfirmRequest) => Promise<boolean>) | undefined;
  /** Runs a checked Forgejo request and adds the token; without it Forgejo requests are denied. */
  forgejo?: ((req: ForgejoRequest, signal: AbortSignal) => Promise<ForgejoReply>) | undefined;
  log?: ((e: RelayLog) => void) | undefined;
}

export interface Relay {
  /** Takes one raw webview message. Never throws. */
  handle(message: unknown): void;
  close(): void;
}

const PERMISSION_DENIED = 7;
const MAX_LIVE = 64;

export function relay(opts: RelayOptions): Relay {
  const live = new Map<number, AbortController>();

  const deny = (id: number, e: RelayLog, reply = true) => {
    opts.log?.(e);
    if (reply) opts.send({ id, end: { code: PERMISSION_DENIED, message: "request not allowed" } });
  };

  async function confirmed(d: Extract<Decision, { ok: true }>): Promise<boolean> {
    if (d.confirm !== "modal") return true;
    return (await opts.confirm?.({ ...describe(d.parsed), parsed: d.parsed })) === true;
  }

  async function runCall(
    id: number,
    d: Extract<Decision, { ok: true }>,
    call: { method: string; bytes: Uint8Array; root: string },
    ac: AbortController,
  ) {
    try {
      if (!(await confirmed(d))) {
        opts.send({ id, end: { code: PERMISSION_DENIED, message: "not confirmed" } });
        return;
      }
      if (ac.signal.aborted) return;
      for await (const r of opts.upstream.stream({
        method: call.method,
        request: d.bytes ?? call.bytes,
        root: call.root,
        signal: ac.signal,
      })) {
        if (ac.signal.aborted) return;
        opts.send({ id, reply: r });
      }
      if (!ac.signal.aborted) opts.send({ id, end: { code: 0, message: "" } });
    } catch (e) {
      if (ac.signal.aborted) return;
      if (e instanceof BridgeError) {
        opts.send({
          id,
          end: {
            code: e.code,
            message: e.message,
            ...(e.detail.length ? { detail: e.detail } : {}),
          },
        });
      } else {
        opts.log?.({ event: "upstream-error", reason: "upstream failed", what: call.method });
        opts.send({ id, end: { code: 13, message: "internal error" } });
      }
    } finally {
      live.delete(id);
    }
  }

  async function runForgejo(
    id: number,
    d: Extract<Decision, { ok: true }>,
    req: ForgejoRequest,
    ac: AbortController,
  ) {
    try {
      if (!(await confirmed(d))) {
        opts.send({ id, end: { code: PERMISSION_DENIED, message: "not confirmed" } });
        return;
      }
      const forgejo = await opts.forgejo?.(req, ac.signal);
      if (!forgejo) {
        opts.send({ id, end: { code: PERMISSION_DENIED, message: "forgejo is not available" } });
      } else if (!ac.signal.aborted) {
        opts.send({ id, forgejo });
      }
    } catch {
      if (!ac.signal.aborted)
        opts.send({ id, end: { code: 13, message: "forgejo request failed" } });
    } finally {
      live.delete(id);
    }
  }

  return {
    handle(message) {
      if (!isObj(message)) return deny(0, { event: "dropped", reason: "not an object" }, false);
      const id = message.id;
      if (typeof id !== "number" || !Number.isInteger(id) || id < 1 || id > 0xffffffff) {
        return deny(0, { event: "dropped", reason: "bad id" }, false);
      }
      if (message.cancel === true) {
        live.get(id)?.abort();
        live.delete(id);
        return;
      }
      if (live.has(id)) return deny(id, { event: "dropped", reason: "id in use" }, false);
      if (live.size >= MAX_LIVE) return deny(id, { event: "dropped", reason: "too many calls" });

      if (message.forgejo !== undefined) {
        const f = message.forgejo;
        if (!isObj(f) || typeof f.method !== "string" || typeof f.path !== "string") {
          return deny(id, { event: "dropped", reason: "bad forgejo request" });
        }
        const req: ForgejoRequest = {
          method: f.method,
          path: f.path,
          ...("body" in f ? { body: f.body } : {}),
        };
        const d = checkForgejo(opts.allowlist, req, { repos: opts.forgeRepos() });
        if (!d.ok) return deny(id, { event: "denied", reason: d.reason, what: "forgejo" });
        if (!opts.forgejo || (d.confirm === "modal" && !opts.confirm)) {
          return deny(id, { event: "denied", reason: "unavailable", what: "forgejo" });
        }
        const ac = new AbortController();
        live.set(id, ac);
        void runForgejo(id, d, req, ac);
        return;
      }

      const { method, bytes, root } = message;
      if (
        typeof method !== "string" ||
        !(bytes instanceof Uint8Array) ||
        bytes.length > MAX_FRAME
      ) {
        return deny(id, { event: "dropped", reason: "bad request" });
      }
      if (root !== undefined && typeof root !== "string") {
        return deny(id, { event: "dropped", reason: "bad root" });
      }
      const roots = opts.roots();
      const chosen = root ?? "";
      if (chosen !== "" && !roots.includes(chosen)) {
        return deny(id, { event: "denied", reason: "root not opened", what: "rpc" });
      }
      const scope: RunScope = { root: chosen || (roots[0] ?? ""), tmpDir: opts.tmpDir };
      const d = checkRpc(opts.allowlist, method, bytes, scope);
      if (!d.ok) {
        // The method name comes from the webview; log it only when it is a known shape.
        const what = /^asc\.v1\.[A-Za-z]+\/[A-Za-z]+$/.test(method) ? method : "rpc";
        return deny(id, { event: "denied", reason: d.reason, what });
      }
      if (d.confirm === "modal" && !opts.confirm) {
        return deny(id, { event: "denied", reason: "no confirmation available", what: method });
      }
      const ac = new AbortController();
      live.set(id, ac);
      void runCall(id, d, { method, bytes, root: chosen }, ac);
    },
    close() {
      for (const ac of live.values()) ac.abort();
      live.clear();
    },
  };
}
