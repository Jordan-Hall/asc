// Styles for the submit dialog; only `--asc-*` tokens.
export const submitCss = `
.asc-submit{font:var(--asc-font-size) var(--asc-font-family);color:var(--asc-fg);background:var(--asc-bg);padding:4px 8px}
.asc-su-row{display:flex;flex-wrap:wrap;gap:8px;align-items:center;padding:3px 0}
.asc-su-chip{border:1px solid var(--asc-border);border-radius:var(--asc-radius);padding:0 6px;display:inline-flex;gap:4px;align-items:center}
.asc-su-change{border:1px solid var(--asc-border);border-radius:var(--asc-radius);margin:6px 0;padding:4px 6px}
.asc-su-field{display:block;box-sizing:border-box;width:100%;margin:2px 0;font:inherit;color:var(--asc-fg);background:var(--asc-bg);border:1px solid var(--asc-border)}
.asc-action{background:transparent;color:var(--asc-fg);border:1px solid var(--asc-border);border-radius:var(--asc-radius);cursor:pointer}
.asc-action:disabled{opacity:.5;cursor:not-allowed}
.asc-su-bar{display:flex;gap:8px;padding:4px 0;border-top:1px solid var(--asc-border)}
.asc-su-error{color:var(--asc-error);padding:2px 0}
`;
