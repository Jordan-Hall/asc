// Styles for the stack editor; only `--asc-*` tokens.
export const stackEditorCss = `
.asc-stack-editor{font:var(--asc-font-size) var(--asc-font-family);color:var(--asc-fg);background:var(--asc-bg)}
.asc-se-rows{list-style:none;margin:0;padding:0;display:flex;flex-direction:column-reverse}
.asc-se-row{display:flex;flex-wrap:wrap;align-items:center;gap:6px;padding:3px 8px;border-radius:var(--asc-radius)}
.asc-se-row:hover{background:var(--asc-hover-bg)}
.asc-se-row.asc-se-drop .asc-subject{text-decoration:line-through;opacity:.6}
.asc-se-row.asc-se-fold{margin-left:16px}
.asc-se-row.asc-drop-target{outline:1px dashed var(--asc-accent)}
.asc-subject{flex:1 1 auto}
.asc-action{background:transparent;color:var(--asc-fg);border:1px solid var(--asc-border);border-radius:var(--asc-radius);cursor:pointer}
.asc-action:disabled{opacity:.5;cursor:not-allowed}
.asc-se-msg{flex:1 1 100%;font:inherit;color:var(--asc-fg);background:var(--asc-bg);border:1px solid var(--asc-border)}
.asc-se-bar{display:flex;gap:8px;padding:4px 8px;border-top:1px solid var(--asc-border)}
.asc-se-error{color:var(--asc-error);padding:2px 8px}
`;
