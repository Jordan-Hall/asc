// The dev host: serves the asc-ui smoke page in a browser against the mock Forgejo. Loopback only;
// plain http is acceptable here because it is a local test fixture (the client opt-in is explicit).
import { fileURLToPath } from "node:url";
import { Hono } from "hono";
import { createMockForgejo, type MockForgejoState, seedState } from "./mock-forgejo";

export interface DevHost {
  readonly url: string;
  readonly state: MockForgejoState;
  stop(): void;
}

const PAGE = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>asc-ui dev host</title></head>
<body><main id="root" aria-live="polite">loading</main><script type="module" src="/app.js"></script></body></html>
`;

export async function startDevHost(opts: { port?: number } = {}): Promise<DevHost> {
  const built = await Bun.build({
    entrypoints: [fileURLToPath(new URL("../client/main.ts", import.meta.url))],
    target: "browser",
    format: "esm",
  });
  const out = built.outputs[0];
  if (!built.success || !out) throw new Error("dev host client build failed");
  const js = await out.text();

  const state = seedState();
  const app = new Hono();
  app.get("/", (c) => c.html(PAGE));
  app.get("/app.js", (c) => c.body(js, 200, { "content-type": "text/javascript" }));
  app.route("/", createMockForgejo(state));
  const server = Bun.serve({ hostname: "127.0.0.1", port: opts.port ?? 0, fetch: app.fetch });
  return { url: `http://127.0.0.1:${server.port}`, state, stop: () => void server.stop(true) };
}
