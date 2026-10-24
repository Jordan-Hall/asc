// Styles for the review panel; only `--asc-*` tokens, so each host theme applies.
export const reviewCss = `
.asc-review{font:var(--asc-font-size) var(--asc-font-family);color:var(--asc-fg);background:var(--asc-bg);display:grid;gap:8px;padding:8px}
.asc-review h3{margin:4px 0;font-size:1em;color:var(--asc-fg-muted)}
.asc-nav,.asc-files,.asc-threads{list-style:none;margin:0;padding:0}
.asc-nav-row{display:flex;flex-wrap:wrap;align-items:center;gap:6px;padding:3px 8px;border-radius:var(--asc-radius);cursor:pointer}
.asc-nav-row:hover{background:var(--asc-hover-bg)}
.asc-nav-row.asc-selected{background:var(--asc-selection-bg)}
.asc-badge{background:var(--asc-badge-bg);color:var(--asc-badge-fg);border-radius:var(--asc-radius);padding:0 5px;font-size:.9em}
.asc-badge.asc-conflicts,.asc-badge.asc-restack{background:var(--asc-conflict)}
.asc-badge.asc-overlap{background:var(--asc-warning)}
.asc-picker{display:flex;flex-wrap:wrap;gap:8px;align-items:center}
.asc-action{background:transparent;color:var(--asc-fg);border:1px solid var(--asc-border);border-radius:var(--asc-radius);cursor:pointer}
.asc-action.asc-disabled{opacity:.5;cursor:not-allowed}
.asc-file{display:flex;gap:6px;padding:2px 8px;cursor:pointer;font-family:var(--asc-font-mono)}
.asc-file:hover{background:var(--asc-hover-bg)}
.asc-thread{padding:2px 8px}
.asc-thread.asc-resolved{opacity:.6}
.asc-error{color:var(--asc-error)}
`;
