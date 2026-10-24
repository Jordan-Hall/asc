// The router: a route table with lazy components, loaders that warm the query cache, history
// navigation inside a transition (the old page stays until the new chunk is ready), and links that
// prefetch on intent. See docs/WEB-UI.md sections 3 and 4.4.
import {
  type AnchorHTMLAttributes,
  type ComponentType,
  createContext,
  type ReactNode,
  startTransition,
  Suspense,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  lazy,
  type LazyExoticComponent,
} from "react";
import { compile, matchPattern, type Params, type Pattern } from "./match";

export interface RouteDef {
  id: string;
  /** `/:owner/:repo/tree/:ref/*path` */
  path: string;
  /** Loads the screen's chunk. */
  component: () => Promise<{ default: ComponentType }>;
  /** Starts the screen's queries so they run while the chunk downloads. */
  load?: (params: Params, search: URLSearchParams) => void;
}

interface Compiled {
  def: RouteDef;
  pattern: Pattern;
  load: () => Promise<{ default: ComponentType }>;
  View: LazyExoticComponent<ComponentType>;
}

export interface Match {
  id: string;
  params: Params;
}

export interface Loc {
  pathname: string;
  search: string;
}

export function compileRoutes(defs: readonly RouteDef[]): readonly Compiled[] {
  // Routes that share a component thunk share one lazy view, so moving between them keeps the
  // mounted screen (repo home to a subdirectory does not flash a skeleton).
  const shared = new Map<RouteDef["component"], Pick<Compiled, "load" | "View">>();
  const compiled = defs.map((def) => {
    let entry = shared.get(def.component);
    if (!entry) {
      let p: Promise<{ default: ComponentType }> | undefined;
      const load = () => (p ??= def.component());
      entry = { load, View: lazy(load) };
      shared.set(def.component, entry);
    }
    return { def, pattern: compile(def.path), ...entry };
  });
  return compiled.toSorted((a, b) => b.pattern.score - a.pattern.score);
}

export function matchRoutes(
  routes: readonly Compiled[],
  pathname: string,
): { route: Compiled; params: Params } | undefined {
  for (const route of routes) {
    const params = matchPattern(route.pattern, pathname);
    if (params) return { route, params };
  }
  return undefined;
}

interface RouterApi {
  base: string;
  loc: Loc;
  match: { route: Compiled; params: Params } | undefined;
  navigate: (to: string, opts?: { replace?: boolean }) => void;
  prefetch: (to: string) => void;
}

const RouterContext = createContext<RouterApi | null>(null);

function useRouter(): RouterApi {
  const r = useContext(RouterContext);
  if (!r) throw new Error("RouterProvider is missing");
  return r;
}

function stripBase(pathname: string, base: string): string {
  if (pathname === base) return "/";
  return pathname.startsWith(`${base}/`) ? pathname.slice(base.length) : pathname;
}

function readLocation(base: string): Loc {
  return { pathname: stripBase(window.location.pathname, base), search: window.location.search };
}

function split(to: string): Loc {
  const q = to.indexOf("?");
  const h = to.indexOf("#");
  const end = h < 0 ? to.length : h;
  return q < 0 || q > end
    ? { pathname: to.slice(0, end), search: "" }
    : { pathname: to.slice(0, q), search: to.slice(q, end) };
}

export function RouterProvider(props: {
  routes: readonly Compiled[];
  base: string;
  children: ReactNode;
}): ReactNode {
  const { routes, base } = props;
  const [loc, setLoc] = useState<Loc>(() => readLocation(base));

  const warm = useCallback(
    (to: string) => {
      const l = split(to);
      const m = matchRoutes(routes, l.pathname);
      if (!m) return;
      void m.route.load();
      m.route.def.load?.(m.params, new URLSearchParams(l.search));
    },
    [routes],
  );

  const navigate = useCallback(
    (to: string, opts?: { replace?: boolean }) => {
      warm(to);
      const url = `${base}${to === "/" ? "" : to}` || "/";
      if (opts?.replace) window.history.replaceState(null, "", url);
      else window.history.pushState(null, "", url);
      startTransition(() => setLoc(readLocation(base)));
    },
    [base, warm],
  );

  useEffect(() => {
    const onPop = () => startTransition(() => setLoc(readLocation(base)));
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, [base]);

  const match = useMemo(() => matchRoutes(routes, loc.pathname), [routes, loc.pathname]);

  // Start the current route's loaders (first load and deep links).
  const search = loc.search;
  useEffect(() => {
    if (match) match.route.def.load?.(match.params, new URLSearchParams(search));
  }, [match, search]);

  const api = useMemo<RouterApi>(
    () => ({ base, loc, match, navigate, prefetch: warm }),
    [base, loc, match, navigate, warm],
  );
  return <RouterContext.Provider value={api}>{props.children}</RouterContext.Provider>;
}

export function useNavigate(): RouterApi["navigate"] {
  return useRouter().navigate;
}

export function useLocation(): Loc {
  return useRouter().loc;
}

export function useParams(): Params {
  return useRouter().match?.params ?? {};
}

export function useRouteId(): string | undefined {
  return useRouter().match?.route.def.id;
}

export function useSearchParams(): URLSearchParams {
  const { search } = useRouter().loc;
  return useMemo(() => new URLSearchParams(search), [search]);
}

/** Renders the matched route, or `notFound`. `fallback` shows while the chunk loads. */
export function RouteView(props: { fallback: ReactNode; notFound: ReactNode }): ReactNode {
  const { match } = useRouter();
  if (!match) return props.notFound;
  const View = match.route.View;
  return (
    <Suspense fallback={props.fallback}>
      <View />
    </Suspense>
  );
}

const INTENT_MS = 65;

export interface LinkProps extends Omit<AnchorHTMLAttributes<HTMLAnchorElement>, "href"> {
  to: string;
  /** Warm the target's chunk and queries on hover or focus. Default true. */
  prefetch?: boolean;
  replace?: boolean;
}

export function Link({ to, prefetch = true, replace, onClick, ...rest }: LinkProps): ReactNode {
  const { base, navigate, prefetch: warm } = useRouter();
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const cancel = () => clearTimeout(timer.current);
  const intent = () => {
    if (!prefetch) return;
    const conn = (navigator as Navigator & { connection?: { saveData?: boolean } }).connection;
    if (conn?.saveData) return;
    cancel();
    timer.current = setTimeout(() => warm(to), INTENT_MS);
  };
  useEffect(() => cancel, []);
  return (
    <a
      {...rest}
      href={`${base}${to === "/" ? "" : to}` || "/"}
      onPointerEnter={intent}
      onFocus={intent}
      onPointerLeave={cancel}
      onBlur={cancel}
      onTouchStart={() => prefetch && warm(to)}
      onClick={(e) => {
        onClick?.(e);
        if (
          e.defaultPrevented ||
          e.button !== 0 ||
          e.metaKey ||
          e.ctrlKey ||
          e.shiftKey ||
          e.altKey ||
          rest.target === "_blank"
        )
          return;
        e.preventDefault();
        navigate(to, replace === undefined ? undefined : { replace });
      }}
    />
  );
}

/** The URL fragment (`#L12`), re-read on hashchange. */
export function useHash(): string {
  return useSyncExternalStore(
    (cb) => {
      window.addEventListener("hashchange", cb);
      return () => window.removeEventListener("hashchange", cb);
    },
    () => window.location.hash,
    () => "",
  );
}
