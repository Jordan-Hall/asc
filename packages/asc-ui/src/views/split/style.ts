// Styles for the split dialog; only `--asc-*` tokens.
export const splitCss = `
.asc-split{font:var(--asc-font-size) var(--asc-font-family);color:var(--asc-fg);background:var(--asc-bg)}
.asc-sp-pieces{display:flex;flex-wrap:wrap;gap:6px;padding:4px 8px;align-items:center}
.asc-sp-piece[aria-pressed=true]{outline:2px solid var(--asc-accent)}
.asc-sp-msg{display:block;box-sizing:border-box;width:calc(100% - 16px);margin:4px 8px;font:inherit;color:var(--asc-fg);background:var(--asc-bg);border:1px solid var(--asc-border)}
.asc-sp-file{margin:6px 8px;border:1px solid var(--asc-border);border-radius:var(--asc-radius)}
.asc-sp-file-head,.asc-sp-hunk-head{display:flex;gap:6px;align-items:center;padding:2px 6px;background:var(--asc-hover-bg)}
.asc-sp-path{flex:1 1 auto;font-weight:600}
.asc-sp-line{display:flex;gap:6px;padding:0 6px;font-family:monospace;white-space:pre}
.asc-sp-badge{min-width:2ch;text-align:right;opacity:.7}
.asc-action{background:transparent;color:var(--asc-fg);border:1px solid var(--asc-border);border-radius:var(--asc-radius);cursor:pointer}
.asc-action:disabled{opacity:.5;cursor:not-allowed}
.asc-sp-note{padding:2px 8px;opacity:.8}
.asc-sp-bar{display:flex;gap:8px;padding:4px 8px;border-top:1px solid var(--asc-border)}
.asc-sp-error{color:var(--asc-error);padding:2px 8px}
`;
