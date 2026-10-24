// Actions runs and job logs (SPEC §24.3.5). Pure models; data comes from the host Actions REST
// API (P05) through the host, so nothing here touches the network.
export type RunTone = "ok" | "fail" | "running" | "pending" | "neutral";

export interface RunRow {
  readonly id: number;
  readonly title: string;
  readonly status: string;
  readonly ref?: string;
  readonly commit?: string;
  readonly event?: string;
}

export interface JobRow {
  readonly id: number;
  readonly name: string;
  readonly status: string;
}

export const LOG_TAIL_LINES = 200;
export const LOG_MAX_CHARS = 200_000;

export function runTone(status: string): RunTone {
  switch (status) {
    case "success":
      return "ok";
    case "failure":
    case "cancelled":
      return "fail";
    case "running":
      return "running";
    case "waiting":
    case "queued":
    case "blocked":
      return "pending";
    default:
      return "neutral";
  }
}

/** Reason a re-run button is disabled; undefined when enabled. */
export function rerunDisabledReason(status: string): string | undefined {
  const tone = runTone(status);
  return tone === "running" || tone === "pending" ? "Run has not finished" : undefined;
}

// Terminal escapes (CSI, OSC) and C0 control characters other than tab and newline.
const ANSI =
  // oxlint-disable-next-line no-control-regex
  /\u001b\[[0-9;?]*[ -/]*[@-~]|\u001b\][^\u0007]*(?:\u0007|\u001b\\)|[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g;

/** Remove terminal escapes and control characters, keep the last `lines` lines. */
export function tailLog(raw: string, lines = LOG_TAIL_LINES): { text: string; truncated: boolean } {
  const bounded = raw.length > LOG_MAX_CHARS ? raw.slice(raw.length - LOG_MAX_CHARS) : raw;
  const clean = bounded.replace(/\r\n?/g, "\n").replace(ANSI, "");
  const all = clean.split("\n");
  if (all.length > 0 && all[all.length - 1] === "") all.pop();
  const truncated = raw.length > LOG_MAX_CHARS || all.length > lines;
  return { text: all.slice(-lines).join("\n"), truncated };
}
