// CSP and page shell for the asc-ui webviews (SPEC 24.4.5). The webview makes no network request:
// connect-src is 'none', and fonts, scripts and styles come only from the asc-ui dist.

export function buildCsp(cspSource: string): string {
  // `cspSource` is supplied by VS Code; refuse anything that could break out of the directive.
  if (cspSource === "" || /[;\r\n'"]/.test(cspSource)) throw new Error("invalid cspSource");
  return [
    "default-src 'none'",
    `script-src ${cspSource}`,
    `style-src ${cspSource}`,
    `font-src ${cspSource}`,
    `img-src ${cspSource} data:`,
    "connect-src 'none'",
  ].join("; ");
}

export function escapeAttr(s: string): string {
  return s
    .replaceAll("&", "&amp;")
    .replaceAll('"', "&quot;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

export interface PageOptions {
  cspSource: string;
  /** Webview URI of the bundled script (`index.js` in the asc-ui dist). */
  scriptUri: string;
  /** Webview URI of the bundled stylesheet, when the dist has one. */
  styleUri?: string;
  /** Which view the bundle mounts: smartlog, graph, review, history, project or search. */
  view: string;
  /** Workspace root shown by the view; passed as data, never executed. */
  root: string;
}

export function pageHtml(o: PageOptions): string {
  const style = o.styleUri ? `<link rel="stylesheet" href="${escapeAttr(o.styleUri)}">` : "";
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${escapeAttr(buildCsp(o.cspSource))}">
<meta name="viewport" content="width=device-width, initial-scale=1">
${style}
</head>
<body data-asc-view="${escapeAttr(o.view)}" data-asc-root="${escapeAttr(o.root)}">
<div id="root"></div>
<script type="module" src="${escapeAttr(o.scriptUri)}"></script>
</body>
</html>
`;
}

/** Maps `--vscode-*` variables onto the `--asc-*` tokens (body theme classes stay on the body). */
export const THEME_MAP: Readonly<Record<string, string>> = {
  "--asc-bg": "--vscode-sideBar-background",
  "--asc-fg": "--vscode-foreground",
  "--asc-fg-muted": "--vscode-descriptionForeground",
  "--asc-border": "--vscode-panel-border",
  "--asc-accent": "--vscode-textLink-foreground",
  "--asc-accent-fg": "--vscode-button-foreground",
  "--asc-hover-bg": "--vscode-list-hoverBackground",
  "--asc-selection-bg": "--vscode-list-activeSelectionBackground",
  "--asc-focus-ring": "--vscode-focusBorder",
  "--asc-added": "--vscode-gitDecoration-addedResourceForeground",
  "--asc-removed": "--vscode-gitDecoration-deletedResourceForeground",
  "--asc-modified": "--vscode-gitDecoration-modifiedResourceForeground",
  "--asc-conflict": "--vscode-gitDecoration-conflictingResourceForeground",
  "--asc-warning": "--vscode-editorWarning-foreground",
  "--asc-error": "--vscode-errorForeground",
  "--asc-success": "--vscode-testing-iconPassed",
  "--asc-badge-bg": "--vscode-badge-background",
  "--asc-badge-fg": "--vscode-badge-foreground",
  "--asc-font-family": "--vscode-font-family",
  "--asc-font-mono": "--vscode-editor-font-family",
  "--asc-font-size": "--vscode-font-size",
};

/** Reads each mapped `--vscode-*` value through `read`; empty values are left out (defaults apply). */
export function mapTheme(read: (variable: string) => string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [token, source] of Object.entries(THEME_MAP)) {
    const v = read(source).trim();
    if (v !== "") out[token] = v;
  }
  return out;
}
