// Native modal confirmations (SPEC 24.4.5, IP-D4). The text is composed by the relay from the parsed
// request (`describe` in @asc/asc-client/relay); nothing the webview sent is shown as-is. Targets are
// sanitised here so a crafted value cannot fake lines of the dialog.
import type { ConfirmRequest } from "@asc/asc-client/relay";

/** The one editor call used: a modal warning with one action button; resolves to the chosen button. */
export type ShowModal = (
  message: string,
  options: { modal: true; detail: string },
  action: string,
) => PromiseLike<string | undefined>;

const MAX_TARGETS = 10;
const MAX_LINE = 160;

export function clean(s: string): string {
  let out = "";
  for (const ch of s) {
    const c = ch.codePointAt(0) ?? 0;
    // Control characters, bidi overrides and line separators become a visible placeholder.
    out += c < 0x20 || c === 0x7f || (c >= 0x80 && c < 0xa0) || (c >= 0x202a && c <= 0x202e) ||
      (c >= 0x2066 && c <= 0x2069) || c === 0x2028 || c === 0x2029
      ? "?"
      : ch;
  }
  return out.length > MAX_LINE ? `${out.slice(0, MAX_LINE - 3)}...` : out;
}

export function confirmDetail(c: ConfirmRequest): string {
  const shown = c.targets.slice(0, MAX_TARGETS).map((t) => `- ${clean(t)}`);
  if (c.targets.length > MAX_TARGETS) shown.push(`- and ${c.targets.length - MAX_TARGETS} more`);
  return shown.join("\n");
}

/** Builds the relay's `confirm` callback. Anything but the explicit button press is a refusal. */
export function nativeConfirm(show: ShowModal): (c: ConfirmRequest) => Promise<boolean> {
  const action = "Continue";
  return async (c) => {
    try {
      const picked = await show(clean(c.title), { modal: true, detail: confirmDetail(c) }, action);
      return picked === action;
    } catch {
      return false;
    }
  };
}
