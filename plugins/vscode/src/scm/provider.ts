// The SCM provider (SPEC 24.4.2): groups conflicts, staged, changes, untracked (hidden when empty),
// a count of changed files, a resource cap of 10,000, targeted status deltas from `Workspace.Watch`,
// quick diff against HEAD and "diff against stack parent". `ScmModel` is editor-free so it runs under
// Bun; `registerScm` is the only `vscode` user.
import { type CallParams, callStream, callUnary, fromHex } from "@asc/asc-client";
import { Workspace } from "@asc/asc-client/workspace";
import type * as vscodeTypes from "vscode";
import type { BridgeManager } from "../bridge";
import { DecorationProvider, type DecorationSource, isConflict } from "../decorations";
import {
  type AscAt,
  type AscReader,
  createAscReader,
  parseAscUri,
  type ReadApi,
  toAscUri,
} from "../fs/ascfs";

export const RESOURCE_CAP = 10_000;
export type GroupId = "conflicts" | "staged" | "changes" | "untracked";
export const GROUP_ORDER: readonly GroupId[] = ["conflicts", "staged", "changes", "untracked"];
export const GROUP_LABEL: Record<GroupId, string> = {
  conflicts: "Merge Conflicts",
  staged: "Staged Changes",
  changes: "Changes",
  untracked: "Untracked",
};

export interface FileStatus {
  path: string;
  xy: string;
  origPath?: string;
}

export interface StatusReply {
  headRef: string;
  files: FileStatus[];
  token: string;
  behind: number;
}

export interface WatchMsg {
  kind: "paths" | "head" | "stage" | "resync" | "other";
  paths: string[];
  token: string;
}

/** The bridge calls the SCM needs; tests supply a fake. */
export interface ScmApi extends ReadApi {
  status(root: string, paths: string[] | null, signal?: AbortSignal): Promise<StatusReply>;
  watch(root: string, sinceToken: string, signal: AbortSignal): AsyncIterable<WatchMsg>;
  /** The commit the current change sits on (its stack parent), or null when unknown. */
  stackParent(root: string): Promise<string | null>;
}

export interface Resource {
  path: string;
  xy: string;
  group: GroupId;
  origPath?: string;
}

export interface GroupView {
  id: GroupId;
  label: string;
  resources: Resource[];
}

function isStaged(xy: string): boolean {
  const x = xy[0] ?? ".";
  return x !== "." && x !== " " && x !== "?" && x !== "!";
}
function isChanged(xy: string): boolean {
  const y = xy[1] ?? ".";
  return y !== "." && y !== " " && y !== "?" && y !== "!";
}

/** The groups one file appears in: a file can be both staged and changed. */
export function groupsOf(xy: string): GroupId[] {
  if (xy === "!!") return [];
  if (xy === "??") return ["untracked"];
  if (isConflict(xy)) return ["conflicts"];
  const out: GroupId[] = [];
  if (isStaged(xy)) out.push("staged");
  if (isChanged(xy)) out.push("changes");
  return out;
}

export interface ScmModelOptions {
  root: string;
  api: ScmApi;
  debounceMs?: number;
  onError?: (e: unknown) => void;
}

export class ScmModel implements DecorationSource {
  readonly decorations = new DecorationProvider(this);
  private readonly entries = new Map<string, FileStatus>();
  private readonly listeners = new Set<() => void>();
  private token = "";
  private headRef = "";
  private behindN = 0;
  private abort: AbortController | null = null;
  private seq = 0;

  constructor(private readonly o: ScmModelOptions) {}

  get root(): string {
    return this.o.root;
  }
  get branch(): string {
    return this.headRef;
  }
  get behind(): number {
    return this.behindN;
  }

  xyFor(path: string): string | undefined {
    return this.entries.get(path)?.xy;
  }

  onDidChange(fn: () => void): { dispose(): void } {
    this.listeners.add(fn);
    return { dispose: () => void this.listeners.delete(fn) };
  }

  /** Groups in display order; the total never exceeds RESOURCE_CAP and `more` counts the rest. */
  groups(): { groups: GroupView[]; more: number } {
    const views = new Map<GroupId, Resource[]>(GROUP_ORDER.map((g) => [g, []]));
    const sorted = [...this.entries.values()].sort((a, b) => (a.path < b.path ? -1 : 1));
    let total = 0;
    let more = 0;
    for (const id of GROUP_ORDER) {
      for (const f of sorted) {
        if (!groupsOf(f.xy).includes(id)) continue;
        if (total >= RESOURCE_CAP) {
          more++;
          continue;
        }
        total++;
        views.get(id)?.push({
          path: f.path,
          xy: f.xy,
          group: id,
          ...(f.origPath === undefined ? {} : { origPath: f.origPath }),
        });
      }
    }
    return {
      groups: GROUP_ORDER.map((id) => ({
        id,
        label: GROUP_LABEL[id],
        resources: views.get(id) ?? [],
      })),
      more,
    };
  }

  /** The number of changed files: conflicts, staged and changed, each file once. */
  count(): number {
    let n = 0;
    for (const f of this.entries.values()) if (groupsOf(f.xy).some((g) => g !== "untracked")) n++;
    return n;
  }

  /** Full refresh, or a delta for `paths`: the reply lists only those paths, so a requested path
   * that is absent is clean. Only paths whose status changed are invalidated. */
  async refresh(paths?: string[]): Promise<void> {
    const mine = ++this.seq;
    const delta = paths !== undefined && paths.length > 0 ? paths : null;
    const reply = await this.o.api.status(this.o.root, delta);
    if (delta === null && mine !== this.seq) return; // a newer refresh supersedes this full one
    if (reply.token !== "") this.token = reply.token;
    this.headRef = reply.headRef;
    this.behindN = reply.behind;
    const next = new Map<string, FileStatus>(reply.files.map((f) => [f.path, f]));
    const scope = delta ?? [...this.entries.keys(), ...next.keys()];
    const changed = new Set<string>();
    for (const p of scope) {
      const before = this.entries.get(p)?.xy;
      const now = next.get(p);
      if (now === undefined) this.entries.delete(p);
      else this.entries.set(p, now);
      if (before !== now?.xy) changed.add(p);
    }
    if (changed.size > 0) this.decorations.invalidate([...changed]);
    for (const l of this.listeners) l();
  }

  /** Starts the watch loop. A dropped stream resumes from the last token after a full refresh,
   * since events in the gap are unknown. */
  start(): void {
    if (this.abort) return;
    const ac = new AbortController();
    this.abort = ac;
    void this.loop(ac.signal);
  }

  stop(): void {
    this.abort?.abort();
    this.abort = null;
  }

  private async loop(signal: AbortSignal): Promise<void> {
    const debounce = this.o.debounceMs ?? 100;
    let backoff = 250;
    let pending = new Set<string>();
    let full = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const flush = () => {
      timer = null;
      const paths = [...pending];
      const wantFull = full;
      pending = new Set();
      full = false;
      this.refresh(wantFull ? undefined : paths).catch((e) => this.o.onError?.(e));
    };
    while (!signal.aborted) {
      try {
        await this.refresh();
        for await (const ev of this.o.api.watch(this.o.root, this.token, signal)) {
          backoff = 250;
          if (ev.token !== "") this.token = ev.token;
          if (ev.kind === "paths") for (const p of ev.paths) pending.add(p);
          else if (ev.kind === "head" || ev.kind === "stage" || ev.kind === "resync") full = true;
          else continue;
          timer ??= setTimeout(flush, debounce);
        }
      } catch (e) {
        if (signal.aborted) break;
        this.o.onError?.(e);
      }
      if (signal.aborted) break;
      await new Promise((r) => setTimeout(r, backoff));
      backoff = Math.min(backoff * 2, 10_000);
    }
    if (timer) clearTimeout(timer);
  }
}

// --- the bridge-backed API ----------------------------------------------------------------------

const enc = new TextEncoder();
const dec = new TextDecoder();
const bytes = (s: string): Uint8Array<ArrayBuffer> => enc.encode(s);

function concat(chunks: Uint8Array[]): Uint8Array {
  let n = 0;
  for (const c of chunks) n += c.byteLength;
  const out = new Uint8Array(n);
  let o = 0;
  for (const c of chunks) {
    out.set(c, o);
    o += c.byteLength;
  }
  return out;
}

function hex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

const WATCH_KIND: Record<number, WatchMsg["kind"]> = {
  0: "paths",
  1: "head",
  3: "stage",
  7: "resync",
};

export function bridgeApi(mgr: BridgeManager): ScmApi {
  const wsIdOf = async (root: string): Promise<string> => {
    const h = await mgr.hello();
    const r = h.roots.find((x) => x.root === root);
    if (!r) throw new Error("asc root is not known to the bridge");
    return r.wsId;
  };
  const params = (root: string, signal?: AbortSignal): CallParams => ({
    root,
    ...(signal === undefined ? {} : { signal }),
  });
  return {
    async readFile(root, path, at: AscAt, signal) {
      const wsId = await wsIdOf(root);
      const p = bytes(path);
      const sel =
        at.kind === "rev"
          ? { case: "revPath" as const, value: { rev: at.rev, path: p } }
          : at.kind === "stage"
            ? { case: "stage" as const, value: { stage: at.stage, path: p } }
            : { case: "fileId" as const, value: fromHex(at.fileId) };
      const chunks: Uint8Array[] = [];
      for await (const r of callStream(
        mgr.node(),
        Workspace.method.readFile,
        { wsId, at: sel },
        params(root, signal),
      )) {
        chunks.push(r.data);
      }
      return concat(chunks);
    },
    async status(root, paths, signal) {
      const wsId = await wsIdOf(root);
      const r = await callUnary(
        mgr.node(),
        Workspace.method.status,
        { wsId, paths: (paths ?? []).map(bytes), untracked: true },
        params(root, signal),
      );
      return {
        headRef: r.headRef,
        behind: r.behind,
        token: r.token,
        files: r.files.map((f) => ({
          path: dec.decode(f.path),
          xy: f.xy,
          ...(f.origPath.byteLength > 0 ? { origPath: dec.decode(f.origPath) } : {}),
        })),
      };
    },
    async *watch(root, sinceToken, signal) {
      const wsId = await wsIdOf(root);
      for await (const e of callStream(
        mgr.node(),
        Workspace.method.watch,
        { wsId, sinceToken },
        params(root, signal),
      )) {
        yield {
          kind: WATCH_KIND[e.kind] ?? "other",
          paths: e.paths.map((p) => dec.decode(p)),
          token: e.token,
        };
      }
    },
    async stackParent(root) {
      const wsId = await wsIdOf(root);
      const r = await callUnary(mgr.node(), Workspace.method.stacks, { wsId }, params(root));
      for (const s of r.stacks) {
        const i = s.changes.findIndex((c) => c.current);
        if (i < 0) continue;
        const prev = i > 0 ? s.changes[i - 1]?.commit : s.base;
        if (prev !== undefined && prev.byteLength > 0) return hex(prev);
      }
      return null;
    },
  };
}

// --- the editor adapter -------------------------------------------------------------------------

export const ASC_SCHEME = "asc";

export interface ScmRegistration {
  dispose(): void;
}

/** Registers the `asc:` file system, one SourceControl per asc root, quick diff, decorations and the
 * 'diff against stack parent' command. Call after the feature check passes. */
export function registerScm(
  vscode: typeof vscodeTypes,
  mgr: BridgeManager,
  roots: readonly string[],
  log: (line: string) => void,
): ScmRegistration {
  const api = bridgeApi(mgr);
  const reader = createAscReader(api);
  const subs: { dispose(): unknown }[] = [];
  const models = new Map<string, ScmModel>();

  const ascUri = (root: string, relPath: string, at: AscAt): vscodeTypes.Uri => {
    const u = toAscUri({ root, path: relPath, at });
    return vscode.Uri.from({ scheme: ASC_SCHEME, path: u.path, query: u.query });
  };
  const norm = (s: string) => (process.platform === "win32" ? s.toLowerCase() : s);
  const rel = (root: string, fsPath: string): string | null => {
    const r = root.replace(/\\/g, "/").replace(/\/+$/, "");
    const p = fsPath.replace(/\\/g, "/");
    return norm(p).startsWith(`${norm(r)}/`) ? p.slice(r.length + 1) : null;
  };

  subs.push(
    vscode.workspace.registerFileSystemProvider(ASC_SCHEME, ascFileSystem(vscode, reader), {
      isReadonly: true,
      isCaseSensitive: true,
    }),
  );

  const changed = new vscode.EventEmitter<vscodeTypes.Uri | vscodeTypes.Uri[] | undefined>();
  subs.push(changed);
  subs.push(
    vscode.window.registerFileDecorationProvider({
      onDidChangeFileDecorations: changed.event,
      provideFileDecoration(uri) {
        if (uri.scheme !== "file") return undefined;
        for (const [root, m] of models) {
          const p = rel(root, uri.fsPath);
          if (p === null) continue;
          const d = m.decorations.decorate(p);
          return d
            ? new vscode.FileDecoration(d.badge, d.tooltip, new vscode.ThemeColor(d.color))
            : undefined;
        }
        return undefined;
      },
    }),
  );

  for (const root of roots) {
    const model = new ScmModel({ root, api, onError: (e) => log(`asc status: ${String(e)}`) });
    models.set(root, model);
    const sc = vscode.scm.createSourceControl("asc", "asc", vscode.Uri.file(root));
    sc.inputBox.placeholder = "Message (Ctrl+Enter to create a change on ‹change›)";
    sc.acceptInputCommand = { command: "asc.create", title: "Create" };
    sc.quickDiffProvider = {
      provideOriginalResource(uri) {
        if (uri.scheme !== "file") return undefined;
        const p = rel(root, uri.fsPath);
        return p === null ? undefined : ascUri(root, p, { kind: "rev", rev: "HEAD" });
      },
    };
    const groupObjs = new Map<GroupId, vscodeTypes.SourceControlResourceGroup>();
    for (const id of GROUP_ORDER) {
      const g = sc.createResourceGroup(id, GROUP_LABEL[id]);
      g.hideWhenEmpty = true;
      groupObjs.set(id, g);
    }
    const render = () => {
      const { groups, more } = model.groups();
      for (const v of groups) {
        const g = groupObjs.get(v.id);
        if (!g) continue;
        g.label =
          more > 0 && v.id === "untracked" ? `${v.label} (${more} more not shown)` : v.label;
        g.resourceStates = v.resources.map((r): vscodeTypes.SourceControlResourceState => {
          const file = vscode.Uri.file(`${root}/${r.path}`);
          const left = ascUri(
            root,
            r.path,
            r.group === "staged" ? { kind: "stage", stage: 0 } : { kind: "rev", rev: "HEAD" },
          );
          return {
            resourceUri: file,
            contextValue: r.xy,
            command:
              r.group === "untracked"
                ? { command: "vscode.open", title: "Open", arguments: [file] }
                : {
                    command: "vscode.diff",
                    title: "Open Changes",
                    arguments: [
                      left,
                      file,
                      `${r.path} (${r.group === "staged" ? "Staged" : "HEAD"})`,
                    ],
                  },
          };
        });
      }
      sc.count = model.count();
    };
    subs.push(model.onDidChange(render));
    subs.push(
      model.decorations.onDidChange((paths) =>
        changed.fire(paths.map((p) => vscode.Uri.file(`${root}/${p}`))),
      ),
    );
    model.start();
    subs.push(sc, ...groupObjs.values(), { dispose: () => model.stop() });
  }

  subs.push(
    vscode.commands.registerCommand("asc.diffAgainstStackParent", async (arg?: vscodeTypes.Uri) => {
      const uri = arg ?? vscode.window.activeTextEditor?.document.uri;
      if (!uri || uri.scheme !== "file") return;
      for (const root of roots) {
        const p = rel(root, uri.fsPath);
        if (p === null) continue;
        const parent = await api.stackParent(root).catch(() => null);
        if (parent === null) {
          void vscode.window.showInformationMessage("ASC: this change has no stack parent.");
          return;
        }
        await vscode.commands.executeCommand(
          "vscode.diff",
          ascUri(root, p, { kind: "rev", rev: parent }),
          uri,
          `${p} (stack parent)`,
        );
        return;
      }
    }),
  );

  return {
    dispose() {
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}

function ascFileSystem(
  vscode: typeof vscodeTypes,
  reader: AscReader,
): vscodeTypes.FileSystemProvider {
  const emitter = new vscode.EventEmitter<vscodeTypes.FileChangeEvent[]>();
  const denied = (): never => {
    throw vscode.FileSystemError.NoPermissions("ASC: files are read-only");
  };
  // stat is followed by readFile for the same uri; one entry avoids reading twice.
  let last: { key: string; data: Uint8Array } | null = null;
  const load = async (uri: vscodeTypes.Uri): Promise<Uint8Array> => {
    const key = uri.toString();
    if (last?.key === key) return last.data;
    try {
      const data = await reader.read(parseAscUri(uri.path, uri.query));
      last = { key, data };
      return data;
    } catch (e) {
      throw vscode.FileSystemError.FileNotFound(e instanceof Error ? e.message : String(e));
    }
  };
  return {
    onDidChangeFile: emitter.event,
    watch: () => new vscode.Disposable(() => {}),
    async stat(uri) {
      const data = await load(uri);
      return { type: vscode.FileType.File, ctime: 0, mtime: 0, size: data.byteLength };
    },
    readDirectory: () => [],
    createDirectory: denied,
    async readFile(uri) {
      const data = await load(uri);
      last = null;
      return data;
    },
    writeFile: denied,
    delete: denied,
    rename: denied,
  };
}
