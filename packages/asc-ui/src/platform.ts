// The host seam of asc-ui (SPEC §24.1 IP-D1, IP-D10). One implementation per host: the VS Code
// webview, the JetBrains JCEF window, the web island and the dev host. Views call the
// host only through this interface, so none of them touches `acquireVsCodeApi` or a JCEF query.
import type { BridgeTransport } from "@asc/asc-client";
import type { AscToken } from "./theme/tokens";

export type HostKind = "vscode" | "jetbrains" | "island" | "browser";

/** A file location a view asks the host to open in a native editor. */
export interface OpenTarget {
  /** Workspace-relative path (forward slashes). The host resolves it against the root. */
  readonly path: string;
  /** Revision for a read-only `asc:` document; absent for the working copy. */
  readonly rev?: string;
  readonly line?: number;
}

export interface DiffTarget {
  readonly path: string;
  readonly leftRev: string;
  readonly rightRev?: string;
}

export type Severity = "info" | "warning" | "error";

export interface Disposable {
  dispose(): void;
}

export interface Platform {
  readonly kind: HostKind;
  /** Typed bridge calls. In a webview this is the relay transport (deny-by-default allowlist). */
  readonly transport: BridgeTransport;
  /** The workspace root this view shows (a key for per-root queues and caches). */
  readonly root: string;
  /** Current host theme mapped onto `--asc-*` tokens. Missing tokens use the defaults. */
  themeTokens(): Partial<Record<AscToken, string>>;
  onThemeChange(listener: () => void): Disposable;
  /** Per-view persistent state (scroll, selection). Never holds secrets. */
  getState<T>(key: string): T | undefined;
  setState(key: string, value: unknown): void;
  openFile(target: OpenTarget): void;
  openDiff(target: DiffTarget): void;
  openExternal(url: string): void;
  copyToClipboard(text: string): Promise<void>;
  notify(severity: Severity, message: string): void;
}
