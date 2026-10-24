// "asc: Search code" (SPEC 24.4.7): a quick pick that streams `CodeIntel.Search`, cancels the
// previous request on each keystroke and stops at 200 hits, plus "Open in panel" for the asc-ui
// search view. Without `search.v1` (every local-only workspace) exact search falls back to
// `Repo.Grep`. Search never rebinds Ctrl+Shift+F. The logic is editor-free; `registerSearch` is
// the only `vscode` user and takes the API as a parameter.
import { callStream } from "@asc/asc-client";
import { CodeIntel, SearchMode } from "@asc/asc-client/codeintel";
import { Repo } from "@asc/asc-client/repo";
import type * as vscodeTypes from "vscode";
import type { BridgeManager } from "./bridge";

export const MAX_HITS = 200;
export const SEARCH_DEBOUNCE_MS = 120;
export const FEATURE_SEARCH = "search.v1";
export const COMMAND_SEARCH = "asc.searchCode";
export const COMMAND_OPEN_PANEL = "asc.openSearch";

export interface SearchHit {
  root: string;
  /** Repo-relative path with forward slashes. */
  path: string;
  /** 1-based line. */
  line: number;
  /** 0-based first match column, when known. */
  col: number;
  text: string;
}

/** The calls search needs; tests supply a fake. */
export interface SearchApi {
  /** True when `Hello.features` advertises `search.v1`. */
  hasSearchV1(): Promise<boolean>;
  /** Streams hits for a literal (exact) query in the workspace scope. */
  stream(root: string, pattern: string, signal: AbortSignal): AsyncIterable<SearchHit[]>;
  /** `Repo.Grep` fallback (exact only). */
  grep(root: string, pattern: string, signal: AbortSignal): Promise<SearchHit[]>;
}

const dec = new TextDecoder();

export interface RunOptions {
  limit?: number;
  onHits(all: readonly SearchHit[]): void;
}

export interface RunResult {
  hits: SearchHit[];
  /** The 200-hit cap stopped the stream. */
  capped: boolean;
  /** `Repo.Grep` answered instead of `CodeIntel.Search`. */
  fallback: boolean;
}

/**
 * Runs one query over every root. The caller's `signal` cancels it (a new keystroke aborts the
 * old run); the run aborts its own stream when the cap is reached. Cancellation resolves quietly
 * with the hits so far and never reports an error.
 */
export async function runSearch(
  api: SearchApi,
  roots: readonly string[],
  pattern: string,
  signal: AbortSignal,
  opts: RunOptions,
): Promise<RunResult> {
  const limit = opts.limit ?? MAX_HITS;
  const hits: SearchHit[] = [];
  const res: RunResult = { hits, capped: false, fallback: false };
  if (pattern.trim() === "" || signal.aborted) return res;
  const inner = new AbortController();
  const stop = () => inner.abort();
  signal.addEventListener("abort", stop, { once: true });
  const take = (batch: readonly SearchHit[]): boolean => {
    for (const h of batch) {
      if (hits.length >= limit) break;
      hits.push(h);
    }
    if (hits.length >= limit) {
      res.capped = true;
      inner.abort();
    }
    opts.onHits(hits);
    return res.capped;
  };
  try {
    const v1 = await api.hasSearchV1();
    res.fallback = !v1;
    for (const root of roots) {
      if (inner.signal.aborted) break;
      if (v1) {
        for await (const batch of api.stream(root, pattern, inner.signal)) {
          if (inner.signal.aborted || take(batch)) break;
        }
      } else if (take(await api.grep(root, pattern, inner.signal))) {
        break;
      }
    }
  } catch (e) {
    // An abort (keystroke or cap) is not an error; anything else is.
    if (!inner.signal.aborted) throw e;
  } finally {
    signal.removeEventListener("abort", stop);
  }
  return res;
}

/** Latest-wins runner: starting a run cancels the previous one; a stale run never reports. */
export class SearchSession {
  private ctl: AbortController | null = null;
  private seq = 0;
  constructor(
    private readonly api: SearchApi,
    private readonly roots: readonly string[],
  ) {}

  start(
    pattern: string,
    cb: {
      onHits(hits: readonly SearchHit[]): void;
      onDone(r: RunResult): void;
      onError(e: unknown): void;
    },
  ): Promise<void> {
    this.cancel();
    const ctl = new AbortController();
    this.ctl = ctl;
    const mine = ++this.seq;
    return runSearch(this.api, this.roots, pattern, ctl.signal, {
      onHits: (h) => {
        if (mine === this.seq && !ctl.signal.aborted) cb.onHits(h);
      },
    }).then(
      (r) => {
        if (mine === this.seq) cb.onDone(r);
      },
      (e) => {
        if (mine === this.seq) cb.onError(e);
      },
    );
  }

  cancel(): void {
    this.ctl?.abort();
    this.ctl = null;
    this.seq++;
  }
}

/** Quick pick row text. Untrusted file content is shown as plain label text, never markdown. */
export function hitLabel(h: SearchHit): string {
  return h.text.trim().slice(0, 200);
}
export function hitDescription(h: SearchHit): string {
  return `${h.path}:${h.line}`;
}

const posix = (p: string) => p.replace(/\\/g, "/");

const toHit = (root: string, path: Uint8Array, line: number, col: number, text: Uint8Array) => ({
  root,
  path: posix(dec.decode(path)),
  line,
  col,
  text: dec.decode(text),
});

/** Search over the plugin bridge: `CodeIntel.Search` (streamed) or `Repo.Grep`. */
export function bridgeSearchApi(mgr: BridgeManager): SearchApi {
  return {
    async hasSearchV1() {
      return (await mgr.hello()).features.includes(FEATURE_SEARCH);
    },
    async *stream(root, pattern, signal) {
      // TODO(P23): revision, change and stack scopes belong to the asc-ui panel, which the quick
      // pick hands over to; the quick pick searches the workspace scope only.
      const req = {
        scope: { at: { case: "workspace" as const, value: true } },
        pattern,
        mode: SearchMode.LITERAL,
        budget: { maxResults: MAX_HITS },
      };
      for await (const r of callStream(mgr.node(), CodeIntel.method.search, req, { root, signal })) {
        const out: SearchHit[] = [];
        for (const f of r.files) {
          for (const m of f.matches) out.push(toHit(root, f.path, m.line, m.cols[0] ?? 0, m.text));
        }
        yield out;
      }
    },
    async grep(root, pattern, signal) {
      const out: SearchHit[] = [];
      const req = { rev: "HEAD", pattern, fixed: true, ignoreCase: true, maxResults: MAX_HITS };
      for await (const r of callStream(mgr.node(), Repo.method.grep, req, { root, signal })) {
        for (const m of r.matches) out.push(toHit(root, m.path, m.line, 0, m.text));
      }
      return out;
    },
  };
}

interface HitItem extends vscodeTypes.QuickPickItem {
  hit?: SearchHit;
  panel?: true;
}

export interface SearchRegistration {
  dispose(): void;
}

export function registerSearch(
  vscode: typeof vscodeTypes,
  mgr: BridgeManager,
  roots: readonly string[],
  log: (line: string) => void,
  apiOverride?: SearchApi,
): SearchRegistration {
  const api = apiOverride ?? bridgeSearchApi(mgr);
  const cmd = vscode.commands.registerCommand(COMMAND_SEARCH, () => {
    const session = new SearchSession(api, roots);
    const qp = vscode.window.createQuickPick<HitItem>();
    qp.placeholder = "Search code (index-backed, no file hydration)";
    const panelItem: HitItem = { label: "$(link-external) Open in panel", panel: true, alwaysShow: true };
    let timer: ReturnType<typeof setTimeout> | undefined;
    const run = (text: string) => {
      qp.busy = true;
      void session.start(text, {
        onHits: (hits) => {
          qp.items = [
            panelItem,
            ...hits.map((h) => ({ label: hitLabel(h), description: hitDescription(h), alwaysShow: true, hit: h })),
          ];
        },
        onDone: (r) => {
          qp.busy = false;
          qp.title = r.capped
            ? `First ${MAX_HITS} results; refine or open in panel`
            : r.fallback
              ? "Exact search only: no code index on this workspace"
              : undefined;
        },
        onError: (e) => {
          qp.busy = false;
          log(`asc search: ${String(e)}`);
        },
      });
    };
    qp.onDidChangeValue((v) => {
      session.cancel();
      if (timer) clearTimeout(timer);
      if (v.trim() === "") {
        qp.items = [];
        qp.busy = false;
        return;
      }
      timer = setTimeout(() => run(v), SEARCH_DEBOUNCE_MS);
    });
    qp.onDidAccept(() => {
      const sel = qp.selectedItems[0];
      qp.hide();
      if (sel?.panel) void vscode.commands.executeCommand(COMMAND_OPEN_PANEL);
      else if (sel?.hit) {
        const h = sel.hit;
        const uri = vscode.Uri.file(`${posix(h.root).replace(/\/+$/, "")}/${h.path}`);
        const pos = new vscode.Position(Math.max(0, h.line - 1), h.col);
        void vscode.window.showTextDocument(uri, { selection: new vscode.Range(pos, pos) });
      }
    });
    qp.onDidHide(() => {
      if (timer) clearTimeout(timer);
      session.cancel();
      qp.dispose();
    });
    qp.show();
  });
  return { dispose: () => cmd.dispose() };
}
