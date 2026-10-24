// Styles for the commit graph; only `--asc-*` tokens, so each host theme applies.
export const ROW_HEIGHT = 22;
export const LANE_WIDTH = 14;

export const graphCss = `
.asc-graph{display:flex;flex-direction:column;height:100%;font:var(--asc-font-size) var(--asc-font-family);color:var(--asc-fg);background:var(--asc-bg)}
.asc-graph-bar{display:flex;flex-wrap:wrap;gap:8px;align-items:center;padding:4px 8px;border-bottom:1px solid var(--asc-border)}
.asc-graph-bar input,.asc-graph-bar select{background:var(--asc-bg);color:var(--asc-fg);border:1px solid var(--asc-border);border-radius:var(--asc-radius)}
.asc-graph-main{position:relative;display:flex;flex:1 1 auto;min-height:0}
.asc-graph-scroll{position:relative;flex:1 1 auto;overflow-y:auto;overflow-x:hidden;outline:none}
.asc-graph-scroll:focus-visible{outline:1px solid var(--asc-focus-ring)}
.asc-graph-lanes{position:sticky;top:0;left:0;display:block;pointer-events:none;z-index:1;height:0}
.asc-graph-lanes canvas{position:absolute;top:0;left:0}
.asc-grow{position:absolute;left:0;right:0;height:${ROW_HEIGHT}px;display:flex;align-items:center;gap:8px;white-space:nowrap;overflow:hidden;box-sizing:border-box}
.asc-grow:hover{background:var(--asc-hover-bg)}
.asc-grow.asc-selected{background:var(--asc-selection-bg)}
.asc-grow.asc-match .asc-gsubject{text-decoration:underline}
.asc-grow.asc-wip .asc-gsubject{font-style:italic}
.asc-grow.asc-pending{opacity:.4}
.asc-gsubject{flex:1 1 auto;overflow:hidden;text-overflow:ellipsis}
.asc-gmeta{color:var(--asc-fg-muted);flex:0 0 auto}
.asc-gid{font-family:var(--asc-font-mono)}
.asc-gref{background:var(--asc-badge-bg);color:var(--asc-badge-fg);border-radius:var(--asc-radius);padding:0 5px;font-size:.9em;flex:0 0 auto}
.asc-minimap{position:relative;flex:0 0 14px;border-left:1px solid var(--asc-border);cursor:pointer}
.asc-minimap canvas{display:block;width:100%;height:100%}
.asc-gmenu{position:absolute;z-index:5;list-style:none;margin:0;padding:2px;background:var(--asc-bg);border:1px solid var(--asc-border);border-radius:var(--asc-radius);min-width:140px}
.asc-gmenu button{display:block;width:100%;text-align:left;background:transparent;color:var(--asc-fg);border:0;padding:3px 8px;cursor:pointer}
.asc-gmenu button:hover:not(:disabled){background:var(--asc-hover-bg)}
.asc-gmenu button:disabled{opacity:.5;cursor:not-allowed}
.asc-gstatus{color:var(--asc-fg-muted);padding:2px 8px}
`;
