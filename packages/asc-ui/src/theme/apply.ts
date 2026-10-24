import { ASC_TOKENS, type AscToken, isAscToken } from "./tokens";

// A value reaches a style sheet, so it must not be able to close the declaration or the rule.
const UNSAFE_VALUE = /[;{}<>\\]|\/\*|url\s*\(|@import|expression\s*\(/i;

/** Merge host overrides over the defaults. Unknown names and unsafe values are dropped. */
export function resolveTokens(
  overrides: Partial<Record<string, string>> = {},
): Record<AscToken, string> {
  const out: Record<string, string> = { ...ASC_TOKENS };
  for (const [name, value] of Object.entries(overrides)) {
    if (value === undefined || !isAscToken(name)) continue;
    const trimmed = value.trim();
    if (trimmed === "" || trimmed.length > 200 || UNSAFE_VALUE.test(trimmed)) continue;
    out[name] = trimmed;
  }
  return out as Record<AscToken, string>;
}

/** A `:root { ... }` rule for a style element. */
export function tokensToCss(tokens: Record<AscToken, string>, selector = ":root"): string {
  const body = Object.entries(tokens)
    .map(([name, value]) => `  ${name}: ${value};`)
    .join("\n");
  return `${selector} {\n${body}\n}\n`;
}

/** Set the tokens as inline custom properties on an element (a webview's `documentElement`). */
export function applyTokens(
  target: { style: { setProperty(name: string, value: string): void } },
  tokens: Record<AscToken, string>,
): void {
  for (const [name, value] of Object.entries(tokens)) target.style.setProperty(name, value);
}
