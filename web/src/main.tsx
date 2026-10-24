import { type BridgeTransport, grpcWeb } from "@asc/asc-client";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { createQueries } from "./api/queries";
import { App } from "./app";
import { createAuthStore } from "./lib/auth";
import { QueryCache } from "./lib/query";
import "./styles/app.css";

async function boot(): Promise<void> {
  let session: Storage | undefined;
  try {
    session = window.sessionStorage;
  } catch {
    session = undefined;
  }
  const auth = createAuthStore(session);
  const cache = new QueryCache();

  let transport: BridgeTransport;
  if (MOCK_BUILD) {
    // Development and ASC_WEB_MOCK builds only; removed from production bundles.
    const { demoTransport } = await import("./api/fixtures");
    transport = demoTransport(60);
  } else {
    transport = grpcWeb({
      baseUrl: window.location.origin,
      allowLoopbackHttp: true,
      headers: auth.headers,
    });
  }

  const ctx = { transport, cache, auth, queries: createQueries({ transport, cache }) };
  const root = document.getElementById("root");
  if (!root) throw new Error("#root is missing");
  createRoot(root).render(
    <StrictMode>
      <App ctx={ctx} />
    </StrictMode>,
  );
}

void boot();
