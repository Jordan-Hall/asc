// Cloud upload state (SPEC 24.3.2, P47b). The status item reads "synced", "N pending" or "offline" (plus
// "N need attention" when the upload queue holds rows the guard held or that failed). It refreshes only on
// events (IP-D9): `Watch KIND_CLOUD`, a `Bridge.Events` connectivity change, a finished op, or a resync;
// there is no timer. Everything the server or the verb printed is untrusted: counts are clamped integers
// and no text from the output is shown. The status comes from `asc cloud status --json` through
// `cliCloudApi` (the bridge allowlist has no entry for it); the "Show cloud drafts" toggle is the
// smartlog's own. Editor-free; `registerCloud` is the only `vscode` user.
import type * as vscodeTypes from "vscode";
import type { CliDirect } from "../cli-direct";

/** Opens the smartlog, where the cloud drafts and their toggle are. */
export const OPEN_COMMAND = "asc.openSmartlog";
/** Largest count shown; a hostile or corrupt number never grows the label. */
export const MAX_COUNT = 999_999;

export type CloudState = "off" | "synced" | "pending" | "attention";

export interface CloudStatus {
  state: CloudState;
  pending: number;
  attention: number;
}

export interface CloudApi {
  /** The encoded `asc cloud status --json` of the root, or null when it could not be read. */
  status(root: string, signal: AbortSignal): Promise<Uint8Array | null>;
}

/** `CloudApi` over `asc cloud status --json`; a non-zero exit reads as "no status". */
export function cliCloudApi(cli: CliDirect): CloudApi {
  return {
    async status(root, signal) {
      const r = await cli.run(root, { kind: "cloud-status" }, signal);
      return r.exit === 0 ? new TextEncoder().encode(r.stdout) : null;
    },
  };
}

export type CloudEventKind = "cloud" | "connectivity" | "opFinished" | "resync";

function count(v: unknown): number {
  if (typeof v !== "number" || !Number.isFinite(v) || v < 0) return 0;
  return Math.min(Math.floor(v), MAX_COUNT);
}

/** Reads the status JSON defensively. Malformed input gives null. */
export function parseCloudStatus(bytes: Uint8Array | null): CloudStatus | null {
  if (bytes === null || bytes.byteLength === 0) return null;
  let v: unknown;
  try {
    v = JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    return null;
  }
  if (typeof v !== "object" || v === null) return null;
  const o = v as Record<string, unknown>;
  if (o.state === "off") return { state: "off", pending: 0, attention: 0 };
  const q =
    typeof o.queue === "object" && o.queue !== null ? (o.queue as Record<string, unknown>) : null;
  if (q === null) return null;
  const pending = count(q.pending);
  const attention = Math.min(count(q.held) + count(q.failed) + count(q.too_large), MAX_COUNT);
  const state: CloudState = attention > 0 ? "attention" : pending > 0 ? "pending" : "synced";
  return { state, pending, attention };
}

export interface CloudView {
  readonly text: string;
  readonly tooltip: string;
  /** False when the item is hidden (cloud off, or no status yet). */
  readonly visible: boolean;
}

export function cloudView(s: CloudStatus | null, serverOffline: boolean): CloudView {
  if (s === null || s.state === "off") {
    return serverOffline
      ? { text: "offline", tooltip: "asc cloud: the server is unreachable", visible: s !== null }
      : { text: "", tooltip: "", visible: false };
  }
  if (serverOffline) {
    const n = s.pending + s.attention;
    return {
      text: "offline",
      tooltip: n > 0 ? `asc cloud: offline, ${n} queued` : "asc cloud: the server is unreachable",
      visible: true,
    };
  }
  if (s.state === "attention") {
    return {
      text: `${s.attention} need attention`,
      tooltip: "asc cloud: drafts held or failed to upload",
      visible: true,
    };
  }
  if (s.state === "pending")
    return {
      text: `${s.pending} pending`,
      tooltip: "asc cloud: drafts waiting to upload",
      visible: true,
    };
  return { text: "synced", tooltip: "asc cloud: all drafts uploaded", visible: true };
}

export interface CloudModelOptions {
  api: CloudApi;
  /** The workspace root whose queue the item shows. */
  root: string;
  onError?: (e: unknown) => void;
}

/** Holds the last status and the server connectivity; refreshes on events only. */
export class CloudStatusModel {
  status: CloudStatus | null = null;
  serverOffline = false;
  private seq = 0;
  private abort: AbortController | null = null;
  private disposed = false;
  private readonly listeners = new Set<() => void>();
  constructor(private readonly o: CloudModelOptions) {}

  view(): CloudView {
    return cloudView(this.status, this.serverOffline);
  }

  onChange(l: () => void): { dispose(): void } {
    this.listeners.add(l);
    return { dispose: () => void this.listeners.delete(l) };
  }

  /** `online` is the `ConnectivityChange.server` state for the root, when the event carried one. */
  async onEvent(kind: CloudEventKind, online?: boolean): Promise<void> {
    if (this.disposed) return;
    if (kind === "connectivity" && online !== undefined) {
      const offline = !online;
      if (offline !== this.serverOffline) {
        this.serverOffline = offline;
        this.emit();
      }
      if (offline) return;
    }
    await this.refresh();
  }

  async refresh(): Promise<void> {
    if (this.disposed) return;
    this.abort?.abort();
    const ac = new AbortController();
    this.abort = ac;
    const mine = ++this.seq;
    let parsed: CloudStatus | null;
    try {
      parsed = parseCloudStatus(await this.o.api.status(this.o.root, ac.signal));
    } catch (e) {
      if (!ac.signal.aborted) this.o.onError?.(e);
      return;
    }
    if (this.disposed || mine !== this.seq) return;
    this.status = parsed;
    this.emit();
  }

  dispose(): void {
    this.disposed = true;
    this.abort?.abort();
    this.listeners.clear();
  }

  private emit(): void {
    for (const l of this.listeners) l();
  }
}

export function registerCloud(
  vscode: typeof vscodeTypes,
  model: CloudStatusModel,
): { dispose(): void } {
  const item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 49);
  item.name = "asc cloud";
  item.command = OPEN_COMMAND;
  const paint = () => {
    const v = model.view();
    item.text = v.text === "" ? "" : `$(cloud) ${v.text}`;
    item.tooltip = v.tooltip;
    if (v.visible) item.show();
    else item.hide();
  };
  const subs = [item, model.onChange(paint)];
  paint();
  return {
    dispose() {
      model.dispose();
      for (const s of subs) s.dispose();
    },
  };
}
