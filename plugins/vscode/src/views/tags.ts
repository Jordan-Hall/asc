// Tags tree (SPEC 24.4.4, 24.8.2 A3): the local tags of each root from the allowlisted `Bridge.Run`
// verb `tag --list`, each marked "not pushed" when the asc server does not list it (`Repo.ListRefs`
// with kind `tag`, paged). Actions: create (lightweight, or annotated with a message), delete (modal),
// push every tag (`push --tags`, modal). Refreshed by `Workspace.Watch` ref events and fetched only
// while visible. `TagsModel` is editor-free so it runs under Bun; `registerTags` is the only `vscode` user.
import { callUnary } from "@asc/asc-client";
import { RefKind, Repo } from "@asc/asc-client/repo";
import type * as vscodeTypes from "vscode";
import { type BridgeManager, serverRepoFor } from "../bridge";
import { decodeOpResult, type TagRow, tagEntries } from "../opresult";
import { pump, type RunResult, runVerb, type WatchApi, type WatchKindName } from "./worktrees";

/** Bridge schema limits of the `tag` verb (`str:255` name, `str:1024` message). */
export const TAG_NAME_MAX = 255;
export const TAG_MESSAGE_MAX = 1024;
/** Refs per `ListRefs` page and the most pages read for one refresh. */
export const SHARED_PAGE = 1000;
export const SHARED_MAX_PAGES = 10;

export interface TagItem extends TagRow {
  /** True when the server lists the tag, false when it does not, null when that is not known. */
  shared: boolean | null;
}

export interface TagsApi extends WatchApi {
  run(root: string, verb: string, args: string[], signal?: AbortSignal): Promise<RunResult>;
  /** The tag names the root's asc server holds; null when the root has no asc server. */
  shared(root: string, signal?: AbortSignal): Promise<Set<string> | null>;
}

const REFUSED = /[\s~^:?*[\\]/;

/** Why a tag name cannot be sent, or null. asc itself applies git's ref-name rules. */
export function tagNameProblem(name: string): string | null {
  if (name === "") return "A tag needs a name.";
  if (new TextEncoder().encode(name).byteLength > TAG_NAME_MAX) return "The tag name is too long.";
  if (name.startsWith("-")) return "A tag name cannot start with '-'.";
  for (const ch of name) {
    const c = ch.codePointAt(0) ?? 0;
    if (c < 0x20 || c === 0x7f) return "The tag name has a control character.";
  }
  if (REFUSED.test(name)) return "The tag name has a character git refuses.";
  return null;
}

export interface TagsModelOptions {
  root: string;
  api: TagsApi;
  onError?: (e: unknown) => void;
}

export class TagsModel {
  items: TagItem[] = [];
  private visible = false;
  private stale = true;
  private seq = 0;
  private abort: AbortController | null = null;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly o: TagsModelOptions) {}

  get root(): string {
    return this.o.root;
  }
  get isStale(): boolean {
    return this.stale;
  }

  onChange(fn: () => void): { dispose(): void } {
    this.listeners.add(fn);
    return { dispose: () => void this.listeners.delete(fn) };
  }

  setVisible(v: boolean): void {
    this.visible = v;
    if (v && this.stale) void this.refresh();
  }

  /** Called for every `Watch` event on the root; only ref moves, resyncs and finished ops change tags. */
  onWatch(k: WatchKindName): void {
    if (k !== "ref" && k !== "resync" && k !== "op-finished") return;
    this.stale = true;
    if (this.visible) void this.refresh();
  }

  async refresh(): Promise<void> {
    const mine = ++this.seq;
    this.abort?.abort();
    const ac = new AbortController();
    this.abort = ac;
    try {
      const r = await this.o.api.run(this.o.root, "tag --list", [], ac.signal);
      if (r.exit !== 0) throw new Error(r.stderr[0] ?? `tag --list exited ${r.exit}`);
      const local = tagEntries(decodeOpResult(r.opResult));
      let shared: Set<string> | null = null;
      try {
        shared = await this.o.api.shared(this.o.root, ac.signal);
      } catch (e) {
        // An unreachable server leaves the local list usable; the mark is then unknown.
        if (!ac.signal.aborted) this.o.onError?.(e);
      }
      if (mine !== this.seq) return;
      this.items = local.map((t) => ({
        ...t,
        shared: shared === null ? null : shared.has(t.name),
      }));
      this.stale = false;
      this.emit();
    } catch (e) {
      if (mine === this.seq && !ac.signal.aborted) this.o.onError?.(e);
    }
  }

  /** Creates `name` at `rev` (HEAD when absent); a non-empty message makes it annotated. */
  async create(name: string, opts: { rev?: string; message?: string } = {}): Promise<RunResult> {
    const problem = tagNameProblem(name);
    if (problem !== null) return refused(problem);
    const msg = opts.message?.trim() ?? "";
    if (msg.length > TAG_MESSAGE_MAX || msg.includes("\0")) {
      return refused("The tag message is too long.");
    }
    const r = await this.o.api.run(this.o.root, "tag", [
      ...(msg === "" ? [] : [`-m=${msg}`]),
      name,
      ...(opts.rev ? [opts.rev] : []),
    ]);
    this.take(r);
    return r;
  }

  async remove(name: string): Promise<RunResult> {
    const problem = tagNameProblem(name);
    if (problem !== null) return refused(problem);
    const r = await this.o.api.run(this.o.root, "tag --delete", [name]);
    this.take(r);
    return r;
  }

  /** `push --tags`: sends every local tag to the default remote, then reads the server's list again. */
  async pushAll(): Promise<RunResult> {
    const r = await this.o.api.run(this.o.root, "push", ["--tags"]);
    if (r.exit === 0) await this.refresh();
    return r;
  }

  /** A `tag` result carries the whole list after the change, so no second call is needed. */
  private take(r: RunResult): void {
    if (r.exit !== 0) return;
    const res = decodeOpResult(r.opResult);
    if (res === null || res.detail.case !== "tag") return;
    const was = new Map(this.items.map((t) => [t.name, t.shared]));
    const known = this.items.some((t) => t.shared !== null);
    this.seq++;
    this.items = tagEntries(res).map((t) => ({
      ...t,
      shared: was.get(t.name) ?? (known ? false : null),
    }));
    this.emit();
  }

  private emit(): void {
    for (const l of this.listeners) l();
  }

  dispose(): void {
    this.abort?.abort();
    this.listeners.clear();
  }
}

function refused(why: string): RunResult {
  return { exit: 2, opResult: null, stderr: [why] };
}

export function describeTag(t: TagItem): { label: string; description: string; tooltip: string } {
  const bits = [t.target.slice(0, 12)];
  if (t.annotated && t.subject) bits.push(t.subject);
  if (t.shared === false) bits.push("not pushed");
  const kind = t.annotated ? "Annotated" : "Lightweight";
  return {
    label: t.name,
    description: bits.join("  "),
    tooltip: `${kind} tag ${t.name}\n${t.target}${t.subject ? `\n\n${t.subject}` : ""}`,
  };
}

/** The delete prompt, composed from the parsed tag and never from caller text. */
export function deleteConfirmText(t: TagItem): string {
  const where = t.shared ? " The tag stays on the server." : "";
  return `Delete tag ${t.name} (${t.target.slice(0, 12)})?${where}`;
}

// --- the bridge adapter ---------------------------------------------------------------------------

/** Every tag name of the root's asc server: `Repo.ListRefs` kind `tag`, at most `SHARED_MAX_PAGES` pages. */
export async function sharedTags(
  mgr: BridgeManager,
  root: string,
  signal?: AbortSignal,
): Promise<Set<string> | null> {
  const name = await serverRepoFor(mgr, root);
  if (name === null) return null;
  const out = new Set<string>();
  let after = "";
  for (let page = 0; page < SHARED_MAX_PAGES; page++) {
    const r = await callUnary(
      mgr.node(),
      Repo.method.listRefs,
      { repo: { name }, kind: RefKind.TAG, after, limit: SHARED_PAGE },
      { root, ...(signal === undefined ? {} : { signal }) },
    );
    for (const ref of r.refs) {
      if (ref.name.startsWith("refs/tags/")) out.add(ref.name.slice("refs/tags/".length));
    }
    if (!r.next) break;
    after = r.next;
  }
  return out;
}

export function bridgeTagsApi(mgr: BridgeManager, watch: WatchApi["watch"]): TagsApi {
  return {
    watch,
    run: (root, verb, args, signal) => runVerb(mgr.node(), root, verb, args, signal),
    shared: (root, signal) => sharedTags(mgr, root, signal),
  };
}

// --- the editor adapter ---------------------------------------------------------------------------

export function registerTags(
  vscode: typeof vscodeTypes,
  api: TagsApi,
  roots: readonly string[],
  log: (line: string) => void,
): { dispose(): void } {
  const subs: { dispose(): unknown }[] = [];
  const ac = new AbortController();
  const models = roots.map(
    (root) => new TagsModel({ root, api, onError: (e) => log(`asc tags: ${String(e)}`) }),
  );
  const changed = new vscode.EventEmitter<void>();
  subs.push(changed);
  for (const m of models) {
    subs.push(m.onChange(() => changed.fire()));
    void pump(
      api,
      m.root,
      ac.signal,
      (k) => m.onWatch(k),
      (e) => log(`asc watch: ${String(e)}`),
    );
  }
  interface Node {
    model: TagsModel;
    tag: TagItem;
  }
  const provider: vscodeTypes.TreeDataProvider<Node> = {
    onDidChangeTreeData: changed.event,
    getChildren: (el) =>
      el ? [] : models.flatMap((model) => model.items.map((tag) => ({ model, tag }))),
    getTreeItem({ tag }) {
      const d = describeTag(tag);
      const item = new vscode.TreeItem(d.label, vscode.TreeItemCollapsibleState.None);
      item.description = d.description;
      item.tooltip = d.tooltip;
      item.contextValue = "asc.tag";
      item.iconPath = new vscode.ThemeIcon(tag.shared === false ? "cloud-upload" : "tag");
      return item;
    },
  };
  const view = vscode.window.createTreeView("asc.tags", { treeDataProvider: provider });
  subs.push(
    view,
    view.onDidChangeVisibility((e) => {
      for (const m of models) m.setVisible(e.visible);
    }),
  );
  for (const m of models) m.setVisible(view.visible);

  const report = (what: string, r: RunResult) => {
    if (r.exit !== 0) {
      void vscode.window.showErrorMessage(
        `asc ${what} failed${r.stderr[0] ? `: ${r.stderr[0]}` : ""}`,
      );
    }
  };
  const pickModel = async (): Promise<TagsModel | undefined> => {
    if (models.length <= 1) return models[0];
    const root = await vscode.window.showQuickPick(
      models.map((m) => m.root),
      { placeHolder: "Choose an ASC root" },
    );
    return models.find((m) => m.root === root);
  };
  subs.push(
    vscode.commands.registerCommand("asc.tags.create", async () => {
      const m = await pickModel();
      if (!m) return;
      const name = await vscode.window.showInputBox({
        prompt: "Tag name",
        validateInput: (v) => tagNameProblem(v.trim()),
      });
      if (name === undefined) return;
      const rev = await vscode.window.showInputBox({ prompt: "Revision to tag (empty for HEAD)" });
      if (rev === undefined) return;
      const message = await vscode.window.showInputBox({
        prompt: "Tag message (empty for a lightweight tag)",
        validateInput: (v) => (v.length > TAG_MESSAGE_MAX ? "The tag message is too long." : null),
      });
      if (message === undefined) return;
      const opts: { rev?: string; message?: string } = { message };
      if (rev.trim() !== "") opts.rev = rev.trim();
      report("tag", await m.create(name.trim(), opts));
    }),
    vscode.commands.registerCommand("asc.tags.delete", async (n?: Node) => {
      if (!n) return;
      const ok = await vscode.window.showWarningMessage(
        deleteConfirmText(n.tag),
        { modal: true },
        "Delete",
      );
      if (ok === "Delete") report("tag --delete", await n.model.remove(n.tag.name));
    }),
    vscode.commands.registerCommand("asc.tags.push", async () => {
      const m = await pickModel();
      if (!m) return;
      const ok = await vscode.window.showWarningMessage(
        "Push every local tag to the default remote?",
        { modal: true },
        "Push",
      );
      if (ok === "Push") report("push --tags", await m.pushAll());
    }),
    vscode.commands.registerCommand("asc.tags.refresh", () => {
      for (const m of models) void m.refresh();
    }),
  );
  return {
    dispose() {
      ac.abort();
      for (const m of models) m.dispose();
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
