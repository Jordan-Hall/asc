// Styles for commit details and compare; only `--asc-*` tokens, so each host theme applies.
export const detailsCss = `
.asc-details,.asc-compare{font:var(--asc-font-size) var(--asc-font-family);color:var(--asc-fg);background:var(--asc-bg);padding:8px}
.asc-details h2{margin:0 0 4px;font-size:1.15em}
.asc-details .asc-body{white-space:pre-wrap;margin:6px 0;color:var(--asc-fg)}
.asc-meta{color:var(--asc-fg-muted);display:flex;flex-wrap:wrap;gap:8px;margin:4px 0}
.asc-details h3,.asc-compare h3{margin:10px 0 4px;font-size:1em;color:var(--asc-fg-muted)}
.asc-files,.asc-versions,.asc-commits{list-style:none;margin:0;padding:0}
.asc-file,.asc-version,.asc-commit{display:flex;gap:8px;align-items:center;padding:2px 4px;border-radius:var(--asc-radius)}
.asc-file:hover,.asc-version:hover,.asc-commit:hover{background:var(--asc-hover-bg)}
.asc-file:focus-visible,.asc-version:focus-visible,.asc-commit:focus-visible{outline:1px solid var(--asc-focus-ring)}
.asc-status{width:1.4em;text-align:center;font-weight:600}
.asc-status[data-status="deleted"]{color:var(--asc-conflict)}
.asc-path{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.asc-count{color:var(--asc-fg-muted);font-variant-numeric:tabular-nums}
.asc-link{color:var(--asc-accent);cursor:pointer;background:none;border:0;padding:0;font:inherit;text-decoration:underline}
.asc-note{padding:4px 8px;color:var(--asc-fg-muted)}
.asc-badge{background:var(--asc-badge-bg);color:var(--asc-badge-fg);border-radius:var(--asc-radius);padding:0 5px;font-size:.9em}
.asc-action{background:transparent;color:var(--asc-fg);border:1px solid var(--asc-border);border-radius:var(--asc-radius);cursor:pointer}
`;
