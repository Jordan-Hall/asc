// `--asc-*` design tokens. The host platform maps its own variables onto these (SPEC §24.4.5):
// VS Code from `--vscode-*` and the body theme classes, JetBrains from the LaF colours.
export const ASC_TOKENS = {
  "--asc-bg": "#1e1e1e",
  "--asc-fg": "#cccccc",
  "--asc-fg-muted": "#8b8b8b",
  "--asc-border": "#3c3c3c",
  "--asc-accent": "#3794ff",
  "--asc-accent-fg": "#ffffff",
  "--asc-hover-bg": "#2a2d2e",
  "--asc-selection-bg": "#04395e",
  "--asc-focus-ring": "#007fd4",
  "--asc-added": "#2ea043",
  "--asc-removed": "#f85149",
  "--asc-modified": "#d29922",
  "--asc-conflict": "#f85149",
  "--asc-warning": "#cca700",
  "--asc-error": "#f14c4c",
  "--asc-success": "#89d185",
  "--asc-badge-bg": "#4d4d4d",
  "--asc-badge-fg": "#ffffff",
  "--asc-font-family": "system-ui, sans-serif",
  "--asc-font-mono": "ui-monospace, Consolas, monospace",
  "--asc-font-size": "13px",
  "--asc-radius": "3px",
} as const;

export type AscToken = keyof typeof ASC_TOKENS;

export const ASC_TOKEN_NAMES = Object.keys(ASC_TOKENS) as readonly AscToken[];

export function isAscToken(name: string): name is AscToken {
  return Object.hasOwn(ASC_TOKENS, name);
}
