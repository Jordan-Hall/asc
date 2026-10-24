// Inbox styles; only `--asc-*` tokens.
export const inboxCss = `
.asc-inbox{font:var(--asc-font-size) var(--asc-font-family);color:var(--asc-fg);background:var(--asc-bg)}
.asc-toolbar{display:flex;gap:12px;padding:4px 8px;border-bottom:1px solid var(--asc-border)}
.asc-inbox-section h3{margin:8px 8px 2px;font-size:1em;color:var(--asc-fg-muted)}
.asc-inbox-items{list-style:none;margin:0;padding:0}
.asc-inbox-item{display:flex;align-items:center;gap:6px;padding:3px 8px;border-radius:var(--asc-radius)}
.asc-inbox-item:hover{background:var(--asc-hover-bg)}
.asc-inbox-item.asc-snoozed{opacity:.6}
.asc-inbox-title{flex:1 1 auto;cursor:pointer}
.asc-inbox-empty{padding:2px 8px;color:var(--asc-fg-muted)}
.asc-inbox .asc-action{background:transparent;color:var(--asc-fg);border:1px solid var(--asc-border);border-radius:var(--asc-radius);cursor:pointer}
.asc-inbox .asc-badge{background:var(--asc-badge-bg);color:var(--asc-badge-fg);border-radius:var(--asc-radius);padding:0 5px}
`;
