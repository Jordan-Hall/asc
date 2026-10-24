// Styles for the smartlog; only `--asc-*` tokens, so each host theme applies.
export const smartlogCss = `
.asc-smartlog{font:var(--asc-font-size) var(--asc-font-family);color:var(--asc-fg);background:var(--asc-bg)}
.asc-toolbar{display:flex;gap:12px;padding:4px 8px;border-bottom:1px solid var(--asc-border)}
.asc-rows,.asc-files{list-style:none;margin:0;padding:0}
.asc-row{display:flex;flex-wrap:wrap;align-items:center;gap:6px;padding:3px 8px;border-radius:var(--asc-radius)}
.asc-row:hover{background:var(--asc-hover-bg)}
.asc-row.asc-selected{background:var(--asc-selection-bg)}
.asc-row:focus-visible{outline:1px solid var(--asc-focus-ring)}
.asc-row.asc-hidden{opacity:.6}
.asc-row.asc-current .asc-subject{font-weight:600}
.asc-uncommitted{flex-direction:column;align-items:flex-start}
.asc-dot{font-family:var(--asc-font-mono);color:var(--asc-fg-muted)}
.asc-subject{flex:1 1 auto}
.asc-badge{background:var(--asc-badge-bg);color:var(--asc-badge-fg);border-radius:var(--asc-radius);padding:0 5px;font-size:.9em}
.asc-badge.asc-restack,.asc-badge.asc-divergent{background:var(--asc-warning)}
.asc-badge.asc-conflict{background:var(--asc-conflict)}
.asc-action{background:transparent;color:var(--asc-fg);border:1px solid var(--asc-border);border-radius:var(--asc-radius);cursor:pointer}
.asc-action.asc-disabled{opacity:.5;cursor:not-allowed}
.asc-drop-ok{outline:1px dashed var(--asc-accent)}
.asc-drop-blocked{outline:1px dashed var(--asc-error)}
`;
