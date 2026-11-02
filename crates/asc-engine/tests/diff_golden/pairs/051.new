// Webview hosts for asc-ui (SPEC 24.4.5): the `asc.smartlog` view and editor panels with serializers.
// The editor API is reached through the small structural types below so the logic runs under Bun.
// The webview holds no token. Avatar requests are served here; every other message goes to
// `onRequest` (the P28b relay) and is dropped and logged when nothing takes it (deny by default).
import { AvatarFetcher } from "./avatar";
import { pageHtml } from "./csp";
import {
  PANEL_KINDS,
  type PanelKind,
  type PanelState,
  panelTitle,
  SMARTLOG_VIEW,
  viewType,
} from "./panels";

export interface Disposable {
  dispose(): void;
}
export interface WebviewLike {
  html: string;
  readonly cspSource: string;
  options: { enableScripts?: boolean; localResourceRoots?: readonly unknown[] };
  /** The webview URI of a file below the asc-ui dist. */
  distUri(file: string): string;
  postMessage(message: unknown): PromiseLike<boolean>;
  onDidReceiveMessage(listener: (message: unknown) => void): Disposable;
}
export interface PanelLike extends Disposable {
  readonly webview: WebviewLike;
  title: string;
  reveal(): void;
  onDidDispose(listener: () => void): Disposable;
}
export interface WebviewHostApi {
  /** The asc-ui dist directory as an editor URI (the only local resource root). */
  distRoot: unknown;
  distHasStyle: boolean;
  createPanel(viewType: string, title: string): PanelLike;
  registerViewProvider(id: string, resolve: (webview: WebviewLike) => void): Disposable;
  registerSerializer(
    viewType: string,
    restore: (panel: PanelLike, state: unknown) => Promise<void>,
  ): Disposable;
  log(line: string): void;
}

/** The allowlist relay (`createRelayHandler`). Return true when it took the message. */
export type RequestHandler = (webview: WebviewLike, message: unknown) => boolean | Promise<boolean>;

export interface AvatarRequest {
  id: number;
  avatar: string;
}

export function isAvatarRequest(m: unknown): m is AvatarRequest {
  if (typeof m !== "object" || m === null) return false;
  const r = m as Record<string, unknown>;
  return typeof r.id === "number" && typeof r.avatar === "string" && Object.keys(r).length === 2;
}

export interface WebviewHostOptions {
  api: WebviewHostApi;
  /** asc workspace roots; the first is the default. */
  roots: readonly string[];
  avatars?: AvatarFetcher;
  onRequest?: RequestHandler;
}

export interface WebviewHost extends Disposable {
  /** Opens or reveals an editor tab. Returns false when there is no asc root. */
  open(kind: PanelKind): boolean;
}

export function registerWebviews(o: WebviewHostOptions): WebviewHost {
  const avatars = o.avatars ?? new AvatarFetcher();
  const subs: Disposable[] = [];
  const open = new Map<PanelKind, PanelLike>();
  const defaultRoot = o.roots[0] ?? "";

  const route = async (webview: WebviewLike, m: unknown): Promise<void> => {
    try {
      if (isAvatarRequest(m)) {
        await webview.postMessage({ id: m.id, avatar: await avatars.get(m.avatar) });
        return;
      }
      if (o.onRequest && (await o.onRequest(webview, m))) return;
    } catch {
      // fall through: a failing handler never lets the message through
    }
    o.api.log("dropped a webview message: no allowlist entry");
  };

  const mount = (webview: WebviewLike, view: string, root: string): Disposable => {
    webview.options = { enableScripts: true, localResourceRoots: [o.api.distRoot] };
    webview.html = pageHtml({
      cspSource: webview.cspSource,
      scriptUri: webview.distUri("index.js"),
      ...(o.api.distHasStyle ? { styleUri: webview.distUri("index.css") } : {}),
      view,
      root,
    });
    return webview.onDidReceiveMessage((m) => void route(webview, m));
  };

  const adopt = (kind: PanelKind, panel: PanelLike, root: string): void => {
    const msg = mount(panel.webview, kind, root);
    open.set(kind, panel);
    panel.onDidDispose(() => {
      msg.dispose();
      if (open.get(kind) === panel) open.delete(kind);
    });
  };

  subs.push(
    o.api.registerViewProvider(SMARTLOG_VIEW, (w) => void mount(w, "smartlog", defaultRoot)),
  );

  for (const kind of PANEL_KINDS) {
    subs.push(
      o.api.registerSerializer(viewType(kind), async (panel, state) => {
        const s = (state ?? {}) as PanelState;
        const root = typeof s.root === "string" && o.roots.includes(s.root) ? s.root : defaultRoot;
        if (root === "") return panel.dispose();
        panel.title = panelTitle(kind);
        adopt(kind, panel, root);
      }),
    );
  }

  return {
    open(kind) {
      const existing = open.get(kind);
      if (existing) {
        existing.reveal();
        return true;
      }
      if (defaultRoot === "") return false;
      adopt(kind, o.api.createPanel(viewType(kind), panelTitle(kind)), defaultRoot);
      return true;
    },
    dispose() {
      for (const s of subs) s.dispose();
      for (const p of open.values()) p.dispose();
      open.clear();
    },
  };
}
