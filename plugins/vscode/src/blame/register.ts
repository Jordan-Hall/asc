// The editor adapter for inline blame, hover, CodeLens and the whole-file blame/heatmap (SPEC
// 24.4.3), plus the bridge-backed `BlameApi`. `vscode` is a parameter, so this file is the only
// place the editor API is touched; the logic is in `model.ts`.
import { callStream, callUnary, changeIdToString, toHex } from "@asc/asc-client";
import { Repo } from "@asc/asc-client/repo";
import { Workspace } from "@asc/asc-client/workspace";
import type * as vscodeTypes from "vscode";
import { type BridgeManager, serverRepoFor } from "../bridge";
import { toAscUri } from "../fs/ascfs";
import {
  ageBuckets,
  type Autolink,
  type BlameApi,
  type BlameCommit,
  type BlameData,
  type BlameHunk,
  BlameService,
  debounce,
  DIRTY_THROTTLE_MS,
  hoverFor,
  INLINE_DEBOUNCE_MS,
  inlineText,
  lensesEnabled,
  lensTitle,
  relativeTime,
  skipReason,
  subjectOf,
} from "./model";

const dec = new TextDecoder();
const enc = new TextEncoder();

/** Blame over `Workspace.Blame` (server streaming; chunks are concatenated). */
export function bridgeBlameApi(mgr: BridgeManager): BlameApi {
  const wsIdOf = async (root: string): Promise<string> => {
    const h = await mgr.hello();
    const r = h.roots.find((x) => x.root === root);
    if (!r) throw new Error("asc root is not known to the bridge");
    return r.wsId;
  };
  return {
    async blame(root, path, buffer, signal) {
      const wsId = await wsIdOf(root);
      const hunks: BlameHunk[] = [];
      const commits = new Map<string, BlameCommit>();
      const reviews: BlameData["reviews"] = [];
      const req = {
        wsId,
        // Dirty documents blame the working copy with the editor bytes; clean ones blame HEAD.
        rev: buffer === null ? "HEAD" : "",
        path: enc.encode(path),
        ...(buffer === null ? {} : { buffer }),
      };
      for await (const r of callStream(mgr.node(), Workspace.method.blame, req, {
        root,
        ...(signal === undefined ? {} : { signal }),
      })) {
        for (const h of r.hunks) {
          hunks.push({
            origin: toHex(h.origin),
            origPath: dec.decode(h.origPath),
            origStart: h.origStart,
            start: h.start,
            len: h.len,
          });
        }
        for (const c of r.commits) {
          const id = toHex(c.id);
          const message = dec.decode(c.message);
          commits.set(id, {
            id,
            subject: subjectOf(message),
            message,
            author: c.author ? dec.decode(c.author.name) : "",
            time: c.author ? Number(c.author.time) : 0,
            changeId: c.change.byteLength === 16 ? changeIdToString(c.change) : "",
            gen: c.gen,
          });
        }
        for (const v of r.reviews) {
          reviews.push({ host: v.host, number: Number(v.number), url: v.url, state: v.state });
        }
      }
      hunks.sort((a, b) => a.start - b.start);
      return { hunks, commits, reviews };
    },
    // `Repo.RepoConfig` of the server repo. A root without an asc server has no autolink rules.
    async autolinks(root): Promise<Autolink[]> {
      const name = await serverRepoFor(mgr, root);
      if (name === null) return [];
      const r = await callUnary(
        mgr.node(),
        Repo.method.repoConfig,
        { repo: { name }, rev: "" },
        { root },
      );
      return r.autolinks.map((a) => ({ prefix: a.prefix, url: a.url }));
    },
  };
}

export interface BlameRegistration {
  dispose(): void;
}

type Watcher = (root: string, signal: AbortSignal) => AsyncIterable<{ kind: string }>;

/** Settings read through `vscode.workspace.getConfiguration("asc.blame")`. */
export function registerBlame(
  vscode: typeof vscodeTypes,
  mgr: BridgeManager,
  roots: readonly string[],
  log: (line: string) => void,
  apiOverride?: BlameApi,
): BlameRegistration {
  const api = apiOverride ?? bridgeBlameApi(mgr);
  const svc = new BlameService({ api });
  const subs: { dispose(): unknown }[] = [];
  const cfg = () => vscode.workspace.getConfiguration("asc.blame");

  const norm = (s: string) => (process.platform === "win32" ? s.toLowerCase() : s);
  const locate = (doc: vscodeTypes.TextDocument): { root: string; path: string } | null => {
    if (doc.uri.scheme !== "file") return null;
    const p = doc.uri.fsPath.replace(/\\/g, "/");
    for (const root of roots) {
      const r = root.replace(/\\/g, "/").replace(/\/+$/, "");
      if (norm(p).startsWith(`${norm(r)}/`)) return { root, path: p.slice(r.length + 1) };
    }
    return null;
  };
  const eligible = (doc: vscodeTypes.TextDocument): { root: string; path: string } | null => {
    const loc = locate(doc);
    const text = doc.getText();
    const why = skipReason({
      scheme: doc.uri.scheme,
      insideRoot: loc !== null,
      // UTF-16 units never exceed the UTF-8 length by more than 3x; chars is a cheap lower bound.
      byteLength: text.length,
      head: enc.encode(text.slice(0, 8192)),
    });
    return why === null ? loc : null;
  };
  /** Dirty documents send their bytes; the version is the cache key. */
  const load = (doc: vscodeTypes.TextDocument, loc: { root: string; path: string }) =>
    svc.load(loc.root, loc.path, doc.version, doc.isDirty ? enc.encode(doc.getText()) : null);

  // --- inline blame ---------------------------------------------------------------------------
  const inlineType = vscode.window.createTextEditorDecorationType({
    after: { color: new vscode.ThemeColor("asc.blameForeground"), margin: "0 0 0 3em" },
    isWholeLine: false,
  });
  subs.push(inlineType);
  const inlineOn = () => cfg().get<boolean>("inline", true);

  const refreshInline = async (editor: vscodeTypes.TextEditor | undefined): Promise<void> => {
    if (!editor) return;
    if (!inlineOn()) {
      editor.setDecorations(inlineType, []);
      return;
    }
    const loc = eligible(editor.document);
    if (!loc) {
      editor.setDecorations(inlineType, []);
      return;
    }
    try {
      const data = await load(editor.document, loc);
      const line = editor.selection.active.line;
      const text = inlineText(data, line, svc.nowSec());
      const end = editor.document.lineAt(line).range.end;
      editor.setDecorations(
        inlineType,
        text === null
          ? []
          : [
              {
                range: new vscode.Range(end, end),
                renderOptions: { after: { contentText: text } },
              },
            ],
      );
    } catch (e) {
      editor.setDecorations(inlineType, []);
      log(`asc blame: ${String(e)}`);
    }
  };
  const onSelection = debounce(INLINE_DEBOUNCE_MS, (e: vscodeTypes.TextEditor | undefined) => {
    void refreshInline(e);
  });
  const onEdit = debounce(DIRTY_THROTTLE_MS, () => {
    void refreshInline(vscode.window.activeTextEditor);
    codeLensChanged.fire();
  });
  const codeLensChanged = new vscode.EventEmitter<void>();
  subs.push(codeLensChanged, {
    dispose: () => {
      onSelection.cancel();
      onEdit.cancel();
    },
  });

  subs.push(
    vscode.window.onDidChangeTextEditorSelection((e) => onSelection(e.textEditor)),
    vscode.window.onDidChangeActiveTextEditor((e) => void refreshInline(e)),
    vscode.workspace.onDidChangeTextDocument((e) => {
      if (e.contentChanges.length > 0) onEdit();
    }),
    vscode.workspace.onDidSaveTextDocument((doc) => {
      const loc = locate(doc);
      if (loc) svc.invalidate(loc.root);
      void refreshInline(vscode.window.activeTextEditor);
      codeLensChanged.fire();
    }),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("asc.blame")) {
        void refreshInline(vscode.window.activeTextEditor);
        codeLensChanged.fire();
      }
    }),
  );

  // --- hover ------------------------------------------------------------------------------------
  subs.push(
    vscode.languages.registerHoverProvider(
      { scheme: "file" },
      {
        async provideHover(doc, pos) {
          const loc = eligible(doc);
          if (!loc || !inlineOn()) return undefined;
          // Hover only the inline text's line end region; plain code hovers stay untouched.
          const lineEnd = doc.lineAt(pos.line).range.end;
          if (pos.character < lineEnd.character) return undefined;
          const data = svc.peek(loc.root, loc.path, doc.isDirty ? doc.version : null);
          if (!data) return undefined;
          const rules = await svc.autolinks(loc.root);
          const h = hoverFor(data, pos.line, rules, svc.nowSec(), loc);
          if (!h) return undefined;
          const md = new vscode.MarkdownString(undefined, true);
          // Only the asc.blame.* commands are trusted, and their arguments are plain data.
          md.isTrusted = {
            enabledCommands: [
              "asc.blame.openChanges",
              "asc.blame.previous",
              "asc.blame.openPr",
              "asc.blame.copyChangeId",
            ],
          };
          md.appendMarkdown(h.markdown);
          if (h.actions.length > 0) {
            md.appendMarkdown("\n\n");
            md.appendMarkdown(
              h.actions
                .map(
                  (a) =>
                    `[${a.label}](command:${a.command}?${encodeURIComponent(JSON.stringify(a.args))})`,
                )
                .join(" | "),
            );
          }
          return new vscode.Hover(md);
        },
      },
    ),
  );

  // --- CodeLens: unresolved lenses at top-level symbols; no RPC in provide ---------------------
  const lensInfo = new WeakMap<
    vscodeTypes.CodeLens,
    { root: string; path: string; version: number | null; start: number; end: number }
  >();
  subs.push(
    vscode.languages.registerCodeLensProvider(
      { scheme: "file" },
      {
        onDidChangeCodeLenses: codeLensChanged.event,
        async provideCodeLenses(doc) {
          if (!cfg().get<boolean>("codeLens", true) || !lensesEnabled(doc.lineCount)) return [];
          const loc = eligible(doc);
          if (!loc) return [];
          const syms =
            (await vscode.commands.executeCommand<vscodeTypes.DocumentSymbol[]>(
              "vscode.executeDocumentSymbolProvider",
              doc.uri,
            )) ?? [];
          const version = doc.isDirty ? doc.version : null;
          // Warm the cache for resolve; the lenses themselves are returned without waiting on it.
          void load(doc, loc)
            .then(() => codeLensChanged.fire())
            .catch(() => {});
          return syms.map((s) => {
            const lens = new vscode.CodeLens(s.range);
            lensInfo.set(lens, {
              ...loc,
              version,
              start: s.range.start.line,
              end: s.range.end.line,
            });
            return lens;
          });
        },
        resolveCodeLens(lens) {
          const i = lensInfo.get(lens);
          const data = i && svc.peek(i.root, i.path, i.version);
          const title = data ? lensTitle(data, i.start, i.end, svc.nowSec()) : null;
          lens.command = {
            title: title ?? "asc",
            command: "asc.blame.toggleFile",
          };
          return lens;
        },
      },
    ),
  );

  // --- whole-file blame and heatmap -------------------------------------------------------------
  const HEAT = [0, 1, 2, 3, 4].map((i) =>
    vscode.window.createTextEditorDecorationType({
      borderWidth: "0 0 0 3px",
      borderStyle: "solid",
      borderColor: new vscode.ThemeColor(`asc.heatmap${i}`),
      isWholeLine: true,
    }),
  );
  subs.push(...HEAT);
  const fileType = vscode.window.createTextEditorDecorationType({});
  subs.push(fileType);
  const modes = new Map<string, "blame" | "heatmap">();

  const paintFile = async (editor: vscodeTypes.TextEditor): Promise<void> => {
    const mode = modes.get(editor.document.uri.toString());
    const clear = () => {
      editor.setDecorations(fileType, []);
      for (const t of HEAT) editor.setDecorations(t, []);
    };
    const loc = eligible(editor.document);
    if (!mode || !loc) return clear();
    const data = await load(editor.document, loc).catch(() => undefined);
    if (!data) return clear();
    const now = svc.nowSec();
    const buckets = ageBuckets(data, HEAT.length);
    const perBucket: vscodeTypes.Range[][] = HEAT.map(() => []);
    const labels: vscodeTypes.DecorationOptions[] = [];
    // Visible ranges only (SPEC 24.4.3).
    for (const vr of editor.visibleRanges) {
      for (const h of data.hunks) {
        const s = Math.max(h.start, vr.start.line);
        const e = Math.min(h.start + h.len - 1, vr.end.line);
        if (s > e) continue;
        const range = new vscode.Range(s, 0, e, 0);
        if (mode === "heatmap") perBucket[buckets.get(h) ?? 0]?.push(range);
        else {
          const c = data.commits.get(h.origin);
          const text = c ? `${c.author}, ${relativeTime(c.time, now)}` : "You";
          labels.push({
            range: new vscode.Range(s, 0, s, 0),
            renderOptions: {
              before: {
                contentText: text,
                color: new vscode.ThemeColor("asc.blameForeground"),
                margin: "0 1em 0 0",
              },
            },
          });
        }
      }
    }
    editor.setDecorations(fileType, labels);
    HEAT.forEach((t, i) => editor.setDecorations(t, perBucket[i] ?? []));
  };
  const repaintAll = () => {
    for (const e of vscode.window.visibleTextEditors) void paintFile(e);
  };
  const toggle = (mode: "blame" | "heatmap") => () => {
    const ed = vscode.window.activeTextEditor;
    if (!ed) return;
    const k = ed.document.uri.toString();
    if (modes.get(k) === mode) modes.delete(k);
    else modes.set(k, mode);
    void paintFile(ed);
  };
  subs.push(
    vscode.window.onDidChangeTextEditorVisibleRanges((e) => void paintFile(e.textEditor)),
    vscode.window.onDidChangeVisibleTextEditors(repaintAll),
    vscode.commands.registerCommand("asc.blame.toggleFile", toggle("blame")),
    vscode.commands.registerCommand("asc.blame.toggleHeatmap", toggle("heatmap")),
    vscode.commands.registerCommand("asc.blame.toggleInline", async () => {
      await cfg().update("inline", !inlineOn(), vscode.ConfigurationTarget.Global);
    }),
  );

  // --- hover actions ----------------------------------------------------------------------------
  const ascUri = (root: string, path: string, rev: string): vscodeTypes.Uri => {
    const u = toAscUri({ root, path, at: { kind: "rev", rev } });
    return vscode.Uri.from({ scheme: "asc", path: u.path, query: u.query });
  };
  const isRoot = (root: unknown): root is string =>
    typeof root === "string" && roots.includes(root);
  const isId = (id: unknown): id is string =>
    typeof id === "string" && /^[0-9a-f]{40,64}$/.test(id);
  subs.push(
    vscode.commands.registerCommand("asc.blame.openChanges", (root, path, id) => {
      if (!isRoot(root) || typeof path !== "string" || !isId(id)) return;
      return vscode.commands.executeCommand(
        "vscode.diff",
        ascUri(root, path, `${id}^`),
        ascUri(root, path, id),
        `${path} (${id.slice(0, 8)})`,
      );
    }),
    vscode.commands.registerCommand("asc.blame.previous", (root, path, id) => {
      if (!isRoot(root) || typeof path !== "string" || !isId(id)) return;
      return vscode.commands.executeCommand("vscode.open", ascUri(root, path, `${id}^`));
    }),
    vscode.commands.registerCommand("asc.blame.openPr", (url) => {
      // https only: the command can be reached from hover markdown, so the argument is re-checked.
      if (typeof url !== "string" || !/^https:\/\/[^\s]+$/.test(url)) return;
      return vscode.env.openExternal(vscode.Uri.parse(url, true));
    }),
    vscode.commands.registerCommand("asc.blame.copyChangeId", (id) => {
      if (typeof id !== "string" || !/^[k-z]{32}$/.test(id)) return;
      return vscode.env.clipboard.writeText(id);
    }),
  );

  // --- invalidation by KIND_HEAD (and resync) -----------------------------------------------------
  const abort = new AbortController();
  subs.push({ dispose: () => abort.abort() });
  const watch: Watcher = (root, signal) => watchKinds(mgr, root, signal);
  for (const root of roots) {
    void (async () => {
      try {
        for await (const ev of watch(root, abort.signal)) {
          if (ev.kind === "head" || ev.kind === "resync") {
            svc.invalidate(root);
            void refreshInline(vscode.window.activeTextEditor);
            repaintAll();
            codeLensChanged.fire();
          }
        }
      } catch (e) {
        if (!abort.signal.aborted) log(`asc blame watch: ${String(e)}`);
      }
    })();
  }

  void refreshInline(vscode.window.activeTextEditor);
  return {
    dispose() {
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}

async function* watchKinds(
  mgr: BridgeManager,
  root: string,
  signal: AbortSignal,
): AsyncGenerator<{ kind: string }> {
  const h = await mgr.hello();
  const r = h.roots.find((x) => x.root === root);
  if (!r) return;
  for await (const e of callStream(
    mgr.node(),
    Workspace.method.watch,
    { wsId: r.wsId, sinceToken: "" },
    { root, signal },
  )) {
    yield { kind: e.kind === 1 ? "head" : e.kind === 7 ? "resync" : "other" };
  }
}
