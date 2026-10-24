// Styles shared by the CI views (actions, affected, projects); only `--asc-*` tokens.
export const ciCss = `
.asc-actions,.asc-affected,.asc-projects{font:var(--asc-font-size) var(--asc-font-family);color:var(--asc-fg);background:var(--asc-bg)}
.asc-toolbar{display:flex;gap:12px;padding:4px 8px;border-bottom:1px solid var(--asc-border)}
.asc-status{padding:4px 8px;color:var(--asc-fg-muted)}
.asc-rows{list-style:none;margin:0;padding:0}
.asc-row{display:flex;align-items:center;gap:6px;padding:3px 8px;border-radius:var(--asc-radius)}
.asc-row:hover{background:var(--asc-hover-bg)}
.asc-row.asc-selected{background:var(--asc-selection-bg)}
.asc-subject{flex:1 1 auto}
.asc-dot{font-family:var(--asc-font-mono);color:var(--asc-fg-muted)}
.asc-badge{background:var(--asc-badge-bg);color:var(--asc-badge-fg);border-radius:var(--asc-radius);padding:0 5px}
.asc-badge.asc-restack,.asc-tone-running,.asc-tone-pending{background:var(--asc-warning)}
.asc-tone-fail{background:var(--asc-error)}
.asc-action{background:transparent;color:var(--asc-fg);border:1px solid var(--asc-border);border-radius:var(--asc-radius);cursor:pointer}
.asc-action.asc-disabled{opacity:.5;cursor:not-allowed}
.asc-log{font-family:var(--asc-font-mono);white-space:pre-wrap;overflow:auto;max-height:50vh;margin:0;padding:8px}
`;
