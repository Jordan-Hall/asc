// P10: validates `src/transport/allowlist.json` (the one checked-in source of the relay allowlist,
// SPEC 24.4.5.1) against its three authorities and exits 1 on any finding:
//   1. the proto descriptors: every RPC of every service is classified exactly once (read, write,
//      never, or the `run` entry), and every name in the file is a real RPC;
//   2. the Rust bridge schema `argv_schema.rs`: the verbs, flags, positionals and confirmations agree;
//   3. the fork's `swagger.v1.json` (path in `--swagger` or `ASC_SWAGGER`): every Forgejo pair is a real
//      operation and every body field is a real property. Without a swagger file the pairs are checked
//      structurally only and the script says so (TODO(C41b): the fork's swagger is not in this repo).
// P48 generates `Allowlist.kt` from the same JSON. The file is never rewritten by this script.
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { DescFile } from "@bufbuild/protobuf";
import { file_asc_v1_bridge } from "../src/gen/asc/v1/bridge_pb";
import { file_asc_v1_changes } from "../src/gen/asc/v1/changes_pb";
import { file_asc_v1_cloud } from "../src/gen/asc/v1/cloud_pb";
import { file_asc_v1_codeintel } from "../src/gen/asc/v1/codeintel_pb";
import { file_asc_v1_projects } from "../src/gen/asc/v1/projects_pb";
import { file_asc_v1_repo } from "../src/gen/asc/v1/repo_pb";
import { file_asc_v1_workspace } from "../src/gen/asc/v1/workspace_pb";

export interface RustVerb {
  name: string;
  flags: Record<string, string>;
  pos: string;
  confirm: string;
}

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
export const ALLOWLIST_PATH = join(here, "../src/transport/allowlist.json");
export const ARGV_SCHEMA_PATH = join(root, "crates/asc/src/bridge/argv_schema.rs");

/** Every method of every generated service, as `asc.v1.Service/Method`. */
export function protoRpcs(): string[] {
  const files: DescFile[] = [
    file_asc_v1_bridge,
    file_asc_v1_changes,
    file_asc_v1_cloud,
    file_asc_v1_codeintel,
    file_asc_v1_projects,
    file_asc_v1_repo,
    file_asc_v1_workspace,
  ];
  return files.flatMap((f) =>
    f.services.flatMap((s) => s.methods.map((m) => `${s.typeName}/${m.name}`)),
  );
}

/** Reads the `VERBS` table of `argv_schema.rs`. */
export function parseRustVerbs(src: string): RustVerb[] {
  const table = src.slice(src.indexOf("pub const VERBS"));
  const m = src.match(/const M: \(&str, &str\) = \("([^"]+)", "([^"]+)"\);/);
  const shared: [string, string] = m ? [m[1] ?? "", m[2] ?? ""] : ["", ""];
  const out: RustVerb[] = [];
  const re = /\bv\(\s*"([^"]+)",\s*&\[([^\]]*)\],\s*"([^"]*)",\s*"([^"]*)",?\s*\)/g;
  for (const hit of table.matchAll(re)) {
    const flags: Record<string, string> = {};
    for (const f of (hit[2] ?? "").matchAll(/\(\s*"([^"]+)",\s*"([^"]+)"\s*\)|\bM\b/g)) {
      if (f[1] !== undefined && f[2] !== undefined) flags[f[1]] = f[2];
      else flags[shared[0]] = shared[1];
    }
    out.push({ name: hit[1] ?? "", flags, pos: hit[3] ?? "", confirm: hit[4] ?? "" });
  }
  return out;
}

export interface Swagger {
  paths?: Record<string, Record<string, SwaggerOp>>;
  definitions?: Record<string, SwaggerSchema>;
}
interface SwaggerOp {
  parameters?: { in?: string; schema?: SwaggerSchema }[];
}
interface SwaggerSchema {
  $ref?: string;
  properties?: Record<string, unknown>;
}

const VERB_KEYS = new Set(["flags", "pos", "confirm"]);
const FORGEJO_NEVER_PREFIXES = [
  "/admin",
  "/orgs",
  "/user/keys",
  "/user/applications",
  "/hooks",
  "/collaborators",
  "/branch_protections",
];

export interface ValidateInput {
  allowlist: unknown;
  rpcs: readonly string[];
  rustVerbs: readonly RustVerb[];
  swagger?: Swagger | undefined;
}

type Json = Record<string, unknown>;
const isObj = (v: unknown): v is Json => typeof v === "object" && v !== null && !Array.isArray(v);
const strings = (v: unknown): string[] =>
  Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];

function swaggerBodyProps(sw: Swagger, op: SwaggerOp): Set<string> | undefined {
  const schema = op.parameters?.find((p) => p.in === "body")?.schema;
  const ref = schema?.$ref?.replace("#/definitions/", "");
  const def = ref ? sw.definitions?.[ref] : schema;
  return def?.properties ? new Set(Object.keys(def.properties)) : undefined;
}

/** All findings; an empty list means the allowlist is valid. */
export function validateAllowlist(input: ValidateInput): string[] {
  const errors: string[] = [];
  const a = input.allowlist;
  if (!isObj(a) || a.version !== 1) return ["allowlist: version must be 1"];

  const rpc = isObj(a.rpc) ? a.rpc : {};
  const seen = new Map<string, string>();
  const add = (kind: string, name: string) => {
    const before = seen.get(name);
    if (before) errors.push(`rpc ${name} is classified twice (${before}, ${kind})`);
    seen.set(name, kind);
  };
  for (const n of strings(rpc.read)) add("read", n);
  for (const n of strings(rpc.never)) add("never", n);
  for (const n of strings(rpc.run)) add("run", n);
  const write = Array.isArray(rpc.write) ? rpc.write : [];
  for (const w of write) {
    if (!isObj(w) || typeof w.name !== "string") {
      errors.push("rpc.write entry needs a name");
      continue;
    }
    add("write", w.name);
    if (w.confirm !== "none" && w.confirm !== "modal") {
      errors.push(`rpc.write ${w.name}: confirm must be "none" or "modal"`);
    }
    for (const k of Object.keys(isObj(w.limits) ? w.limits : {})) {
      if (k !== "patchBytes" && k !== "paths")
        errors.push(`rpc.write ${w.name}: unknown limit ${k}`);
    }
  }
  const real = new Set(input.rpcs);
  for (const n of input.rpcs) if (!seen.has(n)) errors.push(`rpc ${n} is unclassified`);
  for (const n of seen.keys())
    if (!real.has(n)) errors.push(`rpc ${n} does not exist in the protos`);
  if (!strings(rpc.run).includes("asc.v1.Bridge/Run"))
    errors.push("rpc.run must hold asc.v1.Bridge/Run");

  // Run verbs against the Rust schema.
  const run = isObj(a.run) ? a.run : {};
  const verbs = isObj(run.verbs) ? run.verbs : {};
  const rust = new Map(input.rustVerbs.map((v) => [v.name, v]));
  if (rust.size === 0) errors.push("argv_schema.rs: no verbs found (parser out of date?)");
  const reject = new Set(strings(run.reject));
  for (const [name, spec] of Object.entries(verbs)) {
    const r = rust.get(name);
    if (!r) {
      errors.push(`run verb "${name}" is not in argv_schema.rs`);
      continue;
    }
    const s = isObj(spec) ? spec : {};
    for (const k of Object.keys(s)) {
      if (!VERB_KEYS.has(k)) errors.push(`run verb "${name}": unknown key ${k}`);
    }
    const flags = isObj(s.flags) ? s.flags : {};
    for (const [f, t] of Object.entries(flags)) {
      if (reject.has(f)) errors.push(`run verb "${name}": flag ${f} is in run.reject`);
      if (r.flags[f] !== t) {
        errors.push(
          `run verb "${name}": flag ${f} is ${String(t)}, Rust has ${r.flags[f] ?? "none"}`,
        );
      }
    }
    for (const f of Object.keys(r.flags)) {
      if (!(f in flags)) errors.push(`run verb "${name}": flag ${f} is missing (Rust has it)`);
    }
    if ((s.pos ?? "") !== r.pos) {
      errors.push(`run verb "${name}": pos "${String(s.pos ?? "")}" differs from Rust "${r.pos}"`);
    }
    if ((s.confirm ?? "") !== r.confirm) {
      errors.push(`run verb "${name}": confirm differs from Rust "${r.confirm}"`);
    }
  }
  for (const n of rust.keys()) {
    if (!(n in verbs))
      errors.push(`run verb "${n}" is in argv_schema.rs but not in allowlist.json`);
  }
  for (const k of [
    "aliases",
    "stdin",
    "endOfOptions",
    "maxArgs",
    "maxArgBytes",
    "types",
    "reject",
  ]) {
    if (!(k in run)) errors.push(`run.${k} is missing`);
  }

  // Forgejo pairs.
  const forgejo = isObj(a.forgejo) ? a.forgejo : {};
  const pairs = Array.isArray(forgejo.pairs) ? forgejo.pairs : [];
  if (forgejo.base !== "/api/v1") errors.push('forgejo.base must be "/api/v1"');
  const keys = new Set<string>();
  for (const p of pairs) {
    if (!isObj(p) || typeof p.m !== "string" || typeof p.p !== "string") {
      errors.push("forgejo pair needs m and p");
      continue;
    }
    const path = p.p;
    const key = `${p.m} ${path}`;
    if (keys.has(key)) errors.push(`forgejo pair ${key} is listed twice`);
    keys.add(key);
    if (!["GET", "POST", "PUT", "PATCH"].includes(p.m)) {
      errors.push(`forgejo pair ${key}: method not allowed`);
    }
    if (!p.p.startsWith("/") || p.p.includes("..")) errors.push(`forgejo pair ${key}: bad path`);
    if (FORGEJO_NEVER_PREFIXES.some((x) => path.startsWith(x) || path.includes(`}${x}`))) {
      errors.push(`forgejo pair ${key} is on the never list`);
    }
    if (p.m === "PATCH" && p.p === "/repos/{o}/{r}") {
      errors.push(`forgejo pair ${key} is on the never list`);
    }
    if (p.m !== "GET" && !Array.isArray(p.body)) {
      if (!/\/(rerun)$/.test(p.p))
        errors.push(`forgejo pair ${key}: a write needs a body field list`);
    }
    if (p.m === "GET" && p.body !== undefined)
      errors.push(`forgejo pair ${key}: a GET has no body`);
    if (input.swagger) {
      const op = input.swagger.paths?.[p.p]?.[p.m.toLowerCase()];
      if (!op) {
        errors.push(`forgejo pair ${key} is not in the swagger`);
      } else if (Array.isArray(p.body)) {
        const props = swaggerBodyProps(input.swagger, op);
        if (props) {
          for (const f of strings(p.body)) {
            if (!props.has(f))
              errors.push(`forgejo pair ${key}: body field ${f} is not in the swagger`);
          }
        }
      }
    }
  }
  return errors;
}

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

if (import.meta.main) {
  const swaggerPath = arg("--swagger") ?? process.env.ASC_SWAGGER;
  const swagger =
    swaggerPath && existsSync(swaggerPath)
      ? (JSON.parse(readFileSync(swaggerPath, "utf8")) as Swagger)
      : undefined;
  if (!swagger) {
    process.stderr.write(
      "gen-allowlist: no swagger.v1.json (--swagger or ASC_SWAGGER); Forgejo pairs checked structurally only. TODO(C41b)\n",
    );
    if (process.argv.includes("--require-swagger")) process.exit(1);
  }
  const errors = validateAllowlist({
    allowlist: JSON.parse(readFileSync(ALLOWLIST_PATH, "utf8")),
    rpcs: protoRpcs(),
    rustVerbs: parseRustVerbs(readFileSync(ARGV_SCHEMA_PATH, "utf8")),
    swagger,
  });
  for (const e of errors) process.stderr.write(`gen-allowlist: ${e}\n`);
  if (errors.length > 0) process.exit(1);
  process.stdout.write("gen-allowlist: allowlist.json is valid\n");
}
