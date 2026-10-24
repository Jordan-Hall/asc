// Generates src/forgejo/schema.ts: openapi-typescript types for exactly the operations in the
// `forgejo` section of src/transport/allowlist.json (SPEC §24.2, §24.4.5.1). Anything not on the
// allowlist is never typed, so the client cannot call it by accident.
//
//   bun scripts/gen-forgejo.ts [--swagger <swagger.v1.json|v1_json.tmpl>] [--check]
//
// Default source is the fork's template at D:/dev/forgejo/templates/swagger/v1_json.tmpl (its
// `{{...}}` placeholders are blanked). Path parameters are renamed to the allowlist's {o} {r} {n} {id}.
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import openapiTS, { astToString } from "openapi-typescript";

type Json = Record<string, unknown>;
interface Param {
  name: string;
  in: string;
  [k: string]: unknown;
}
interface Op {
  operationId?: string;
  parameters?: Param[];
  responses?: Record<string, unknown>;
  [k: string]: unknown;
}
interface Swagger {
  swagger: string;
  info: unknown;
  paths: Record<string, Record<string, Op>>;
  definitions?: Record<string, unknown>;
  parameters?: Record<string, Param>;
  responses?: Record<string, unknown>;
}
export interface Pair {
  m: string;
  p: string;
  body?: string[];
}

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");
const DEFAULT_SWAGGER = "D:/dev/forgejo/templates/swagger/v1_json.tmpl";

const norm = (p: string): string => p.replace(/\{[^}]*\}/g, "{}");
const paramNames = (p: string): string[] => [...p.matchAll(/\{([^}]*)\}/g)].map((m) => m[1] ?? "");

// TODO(C41b): the inbox endpoints are not in the fork's swagger yet; this hand-written fragment
// stands in until the fork emits them. TODO(P05): same for the run rerun endpoint.
const inboxItem = {
  type: "object",
  properties: {
    id: { type: "integer", format: "int64" },
    section: { type: "string" },
    pinned: { type: "boolean" },
    snoozed_until: { type: "string", format: "date-time" },
    pull_request: { $ref: "#/definitions/PullRequest" },
  },
};
const OVERLAY: Record<string, Record<string, Op>> = {
  "GET /user/asc/inbox": {
    get: {
      operationId: "ascInboxList",
      parameters: [
        { name: "section", in: "query", type: "string" },
        { name: "limit", in: "query", type: "integer" },
        { name: "page", in: "query", type: "integer" },
      ],
      responses: { "200": { description: "inbox", schema: { type: "array", items: inboxItem } } },
    },
  },
  "PUT /user/asc/inbox/{id}": {
    put: {
      operationId: "ascInboxUpdate",
      parameters: [
        { name: "id", in: "path", type: "integer", format: "int64", required: true },
        {
          name: "body",
          in: "body",
          schema: {
            type: "object",
            properties: {
              pinned: { type: "boolean" },
              snoozed_until: { type: "string", format: "date-time" },
            },
          },
        },
      ],
      responses: { "200": { description: "updated", schema: inboxItem } },
    },
  },
  "POST /repos/{o}/{r}/actions/runs/{id}/rerun": {
    post: {
      operationId: "ascActionRunRerun",
      parameters: [
        { name: "owner", in: "path", type: "string", required: true },
        { name: "repo", in: "path", type: "string", required: true },
        { name: "run_id", in: "path", type: "integer", format: "int64", required: true },
      ],
      responses: { "200": { description: "queued" } },
    },
  },
};

function collectRefs(node: unknown, out: Set<string>): void {
  if (Array.isArray(node)) {
    for (const n of node) collectRefs(n, out);
  } else if (node && typeof node === "object") {
    for (const [k, v] of Object.entries(node as Json)) {
      if (k === "$ref" && typeof v === "string") out.add(v);
      else collectRefs(v, out);
    }
  }
}

export function filterSwagger(
  sw: Swagger,
  pairs: Pair[],
): { swagger: Swagger; overlaid: string[] } {
  const byNorm = new Map(Object.keys(sw.paths).map((p) => [norm(p), p]));
  const paths: Swagger["paths"] = {};
  const overlaid: string[] = [];
  for (const pair of pairs) {
    const key = `${pair.m} ${pair.p}`;
    const method = pair.m.toLowerCase();
    const swPath = byNorm.get(norm(pair.p));
    let op = swPath ? sw.paths[swPath]?.[method] : undefined;
    if (!op) {
      op = OVERLAY[key]?.[method];
      if (!op) throw new Error(`gen-forgejo: ${key} is neither in the swagger nor in the overlay`);
      overlaid.push(key);
    }
    const clone = structuredClone(op);
    // Rename path parameters positionally to the allowlist's names.
    const swNames = swPath ? paramNames(swPath) : paramNames(pair.p);
    const names = paramNames(pair.p);
    clone.parameters = (clone.parameters ?? []).map((prm) => {
      if (prm.in !== "path") return prm;
      const i = swNames.indexOf(prm.name);
      const to = i >= 0 ? names[i] : undefined;
      return to ? { ...prm, name: to } : prm;
    });
    (paths[pair.p] ??= {})[method] = clone;
  }
  // Prune definitions, parameters and responses to what is reachable.
  const seen = new Set<string>();
  const queue = new Set<string>();
  collectRefs(paths, queue);
  const definitions: Record<string, unknown> = {};
  const parameters: Record<string, Param> = {};
  const responses: Record<string, unknown> = {};
  while (queue.size) {
    const ref = queue.values().next().value as string;
    queue.delete(ref);
    if (seen.has(ref)) continue;
    seen.add(ref);
    const [, section, name] = /^#\/(definitions|parameters|responses)\/(.+)$/.exec(ref) ?? [];
    if (!section || !name) continue;
    const target = (sw as unknown as Record<string, Record<string, unknown> | undefined>)[
      section
    ]?.[name];
    if (target === undefined) continue;
    if (section === "definitions") definitions[name] = target;
    else if (section === "parameters") parameters[name] = target as Param;
    else responses[name] = target;
    collectRefs(target, queue);
  }
  return {
    swagger: {
      swagger: "2.0",
      info: { title: "Forgejo API (asc allowlist subset)", version: "1" },
      paths,
      definitions,
      parameters,
      responses,
    },
    overlaid,
  };
}

// Minimal Swagger 2 to OpenAPI 3 conversion for the filtered subset (openapi-typescript 7 reads 3.x only).
const rewriteRefs = (node: unknown): unknown => {
  if (Array.isArray(node)) return node.map(rewriteRefs);
  if (node && typeof node === "object") {
    const out: Json = {};
    for (const [k, v] of Object.entries(node as Json)) {
      if (k === "$ref" && typeof v === "string") {
        out[k] = v
          .replace("#/definitions/", "#/components/schemas/")
          .replace("#/parameters/", "#/components/parameters/")
          .replace("#/responses/", "#/components/responses/");
      } else if (k === "x-nullable") {
        if (v === true) out.nullable = true;
      } else if (!k.startsWith("x-")) {
        out[k] = rewriteRefs(v);
      }
    }
    return out;
  }
  return node;
};

const SCHEMA_KEYS = ["type", "format", "items", "enum", "default", "minimum", "maximum"];
function convertParam(prm: Param): { param?: Json; body?: Json } {
  if (prm.in === "body") {
    return {
      body: {
        required: Boolean(prm.required),
        content: { "application/json": { schema: rewriteRefs(prm.schema) } },
      },
    };
  }
  const schema: Json = {};
  for (const k of SCHEMA_KEYS) if (prm[k] !== undefined) schema[k] = rewriteRefs(prm[k]);
  return {
    param: {
      name: prm.name,
      in: prm.in,
      required: prm.in === "path" ? true : Boolean(prm.required),
      schema,
    },
  };
}

function convertResponse(r: unknown): Json {
  const src = r as Json;
  const out: Json = { description: (src.description as string | undefined) ?? "" };
  if (src.schema) out.content = { "application/json": { schema: rewriteRefs(src.schema) } };
  return out;
}

export function toOpenApi3(sw: Swagger): Json {
  const paths: Json = {};
  for (const [p, methods] of Object.entries(sw.paths)) {
    const item: Json = {};
    for (const [m, op] of Object.entries(methods)) {
      const parameters: Json[] = [];
      let requestBody: Json | undefined;
      for (const prm of op.parameters ?? []) {
        if ("$ref" in prm) {
          parameters.push(rewriteRefs(prm) as Json);
          continue;
        }
        const c = convertParam(prm);
        if (c.param) parameters.push(c.param);
        if (c.body) requestBody = c.body;
      }
      const responses: Json = {};
      for (const [code, r] of Object.entries((op.responses ?? {}) as Json)) {
        responses[code] = (r as Json).$ref ? rewriteRefs(r) : convertResponse(r);
      }
      item[m] = {
        operationId: op.operationId,
        parameters,
        ...(requestBody ? { requestBody } : {}),
        responses: Object.keys(responses).length ? responses : { "200": { description: "" } },
      };
    }
    paths[p] = item;
  }
  const params: Json = {};
  for (const [k, v] of Object.entries(sw.parameters ?? {})) params[k] = convertParam(v).param ?? {};
  const resps: Json = {};
  for (const [k, v] of Object.entries(sw.responses ?? {})) resps[k] = convertResponse(v);
  return {
    openapi: "3.0.3",
    info: { title: "Forgejo API (asc allowlist subset)", version: "1" },
    paths,
    components: {
      schemas: rewriteRefs(sw.definitions ?? {}),
      parameters: params,
      responses: resps,
    },
  };
}

export function loadSwagger(path: string): Swagger {
  return JSON.parse(readFileSync(path, "utf8").replace(/\{\{[^}]*\}\}/g, "x")) as Swagger;
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const at = args.indexOf("--swagger");
  const swaggerPath =
    (at >= 0 ? args[at + 1] : undefined) ?? process.env.ASC_SWAGGER ?? DEFAULT_SWAGGER;
  if (!existsSync(swaggerPath)) {
    process.stderr.write(
      `gen-forgejo: swagger not found at ${swaggerPath} (use --swagger or ASC_SWAGGER)\n`,
    );
    process.exit(1);
  }
  const allowlist = JSON.parse(
    readFileSync(resolve(root, "src/transport/allowlist.json"), "utf8"),
  ) as {
    hostApi: { providers: { forgejo: { pairs: Pair[] } } };
  };
  const { swagger, overlaid } = filterSwagger(
    loadSwagger(swaggerPath),
    allowlist.hostApi.providers.forgejo.pairs,
  );
  for (const o of overlaid)
    process.stderr.write(`gen-forgejo: overlay for ${o} (TODO(C41b)/TODO(P05))\n`);
  const ast = await openapiTS(toOpenApi3(swagger) as unknown as Parameters<typeof openapiTS>[0], {
    alphabetize: true,
  });
  const ts = astToString(ast);
  const out = `// Generated by scripts/gen-forgejo.ts from the allowlist's forgejo pairs. Do not edit.\n${ts}`;
  const target = resolve(root, "src/forgejo/schema.ts");
  if (args.includes("--check")) {
    if (!existsSync(target) || readFileSync(target, "utf8") !== out) {
      process.stderr.write("gen-forgejo: src/forgejo/schema.ts is stale\n");
      process.exit(1);
    }
    return;
  }
  writeFileSync(target, out);
}

if (import.meta.main) await main();
