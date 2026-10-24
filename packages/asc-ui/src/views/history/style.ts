// Styles for the file history; only `--asc-*` tokens, so each host theme applies.
export const historyCss = `
.asc-history{font:var(--asc-font-size) var(--asc-font-family);color:var(--asc-fg);background:var(--asc-bg)}
.asc-note{padding:4px 8px;color:var(--asc-fg-muted)}
.asc-lanes{display:flex;gap:12px;padding:2px 8px}
.asc-lane-label{color:var(--asc-fg-muted)}
.asc-hist-rows{list-style:none;margin:0;padding:0}
.asc-hist-row{display:flex;align-items:center;gap:8px;padding:0 8px;border-radius:var(--asc-radius)}
.asc-hist-row:hover{background:var(--asc-hover-bg)}
.asc-hist-row:focus-visible{outline:1px solid var(--asc-focus-ring)}
.asc-track{position:relative;flex:none;height:100%}
.asc-node{position:absolute;top:50%;transform:translateY(-50%);border-radius:50%;background:var(--asc-accent)}
`;
