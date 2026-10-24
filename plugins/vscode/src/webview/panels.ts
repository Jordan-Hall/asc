// Panel kinds for the asc-ui editor tabs and the sidebar view (SPEC 24.4.5).
export const SMARTLOG_VIEW = "asc.smartlog";

export const PANEL_KINDS = ["graph", "review", "history", "project", "search"] as const;
export type PanelKind = (typeof PANEL_KINDS)[number];

/** The `viewType` of a panel; `onWebviewPanel:<viewType>` activates the extension on restore. */
export function viewType(kind: PanelKind): string {
  return `asc.${kind}`;
}

const TITLES: Record<PanelKind, string> = {
  graph: "asc Graph",
  review: "asc Review",
  history: "asc History",
  project: "asc Project",
  search: "asc Search",
};

export function panelTitle(kind: PanelKind): string {
  return TITLES[kind];
}

/** State a serializer restores from. Only the root; never secrets. */
export interface PanelState {
  readonly root?: string;
}
