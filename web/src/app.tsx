import { type ReactNode, useMemo } from "react";
import { type AppCtx, AppProvider } from "./api/queries";
import { Shell } from "./components/shell";
import { PageSkeleton } from "./components/skeleton";
import { CacheProvider } from "./lib/query";
import { compileRoutes, Link, RouteView, RouterProvider } from "./lib/router";
import { createRoutes } from "./routes";
import { BASE } from "./base";

function NotFound(): ReactNode {
  return (
    <div className="mx-auto max-w-xl p-8 text-center">
      <h1 className="mb-2 text-2xl font-semibold text-fg-strong">Not found</h1>
      <p className="mb-4 text-fg-muted">This page does not exist.</p>
      <Link to="/">Back to dashboard</Link>
    </div>
  );
}

export function App(props: { ctx: AppCtx; base?: string }): ReactNode {
  const routes = useMemo(() => compileRoutes(createRoutes(props.ctx)), [props.ctx]);
  return (
    <AppProvider value={props.ctx}>
      <CacheProvider value={props.ctx.cache}>
        <RouterProvider routes={routes} base={props.base ?? BASE}>
          <Shell>
            <RouteView fallback={<PageSkeleton />} notFound={<NotFound />} />
          </Shell>
        </RouterProvider>
      </CacheProvider>
    </AppProvider>
  );
}
