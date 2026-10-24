// Styles for the search view; only `--asc-*` tokens, so each host theme applies.
export const searchCss = `
.asc-search{font:var(--asc-font-size) var(--asc-font-family);color:var(--asc-fg);background:var(--asc-bg)}
.asc-search-form{display:flex;flex-wrap:wrap;align-items:center;gap:6px;padding:4px 8px;border-bottom:1px solid var(--asc-border)}
.asc-search-input{flex:1 1 200px;min-width:120px}
.asc-search-modes{display:flex;gap:2px}
.asc-mode,.asc-action{background:transparent;color:var(--asc-fg);border:1px solid var(--asc-border);border-radius:var(--asc-radius);cursor:pointer}
.asc-mode[aria-pressed=true]{background:var(--asc-selection-bg)}
.asc-disabled{opacity:.5;cursor:not-allowed}
.asc-coverage,.asc-status{padding:2px 8px;color:var(--asc-fg-muted)}
.asc-coverage.asc-partial{color:var(--asc-warning)}
.asc-results{list-style:none;margin:0;padding:0}
.asc-hit{display:flex;gap:8px;padding:2px 8px;cursor:pointer;border-radius:var(--asc-radius)}
.asc-hit:hover{background:var(--asc-hover-bg)}
.asc-hit:focus-visible{outline:1px solid var(--asc-focus-ring)}
.asc-hit-loc{font-family:var(--asc-font-mono);color:var(--asc-fg-muted);flex:0 0 auto}
.asc-hit-text{font-family:var(--asc-font-mono);white-space:pre;overflow:hidden;text-overflow:ellipsis}
.asc-hit-text mark{background:var(--asc-selection-bg);color:inherit}
.asc-redacted{font-style:italic;color:var(--asc-fg-muted)}
`;
