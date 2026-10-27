// The route table. Each screen is a lazy chunk; `load` warms the queries it will need so they run
// while the chunk downloads (docs/WEB-UI.md section 4.4).
import { type AppCtx, warm } from "./api/queries";
import type { Params } from "./lib/match";
import type { RouteDef } from "./lib/router";
import { PATTERNS } from "./lib/paths";

const signin = () => import("./routes/signin");
const settings = () => import("./routes/settings");
const repoPage = () => import("./routes/repo");
const blob = () => import("./routes/blob");
const changes = () => import("./routes/changes");
const info = () => import("./routes/info");
const lists = () => import("./routes/lists");
const inbox = () => import("./routes/inbox");
const search = () => import("./routes/search");
const newPage = () => import("./routes/new");
const blame = () => import("./routes/blame");
const history = () => import("./routes/history");
const commit = () => import("./routes/commit");
const change = () => import("./routes/change");
const actions = () => import("./routes/actions");
const run = () => import("./routes/run");
const repoSettings = () => import("./routes/repo-settings");

const nameOf = (p: Params) => `${p.owner ?? ""}/${p.repo ?? ""}`;

export function createRoutes(app: AppCtx): RouteDef[] {
  const q = app.queries;

  /** Home and the screen it will show (landing or dashboard) arrive together. */
  const home = async () => {
    const [mod] = await Promise.all([
      import("./routes/home"),
      app.auth.token() ? import("./routes/dashboard") : import("./routes/landing"),
    ]);
    return mod;
  };

  /** Repo home: refs first, then the default branch's tree and last commit. */
  const loadTree = (p: Params) => {
    const name = nameOf(p);
    const path = p.path ?? "";
    const refs = q.refs(name);
    if (p.ref) {
      warm(app, q.tree(name, p.ref, path));
      warm(app, q.lastCommit(name, p.ref, path));
      warm(app, refs);
      return;
    }
    void app.cache
      .fetch(refs.key, refs.run, refs.opts)
      .then((r) => {
        warm(app, q.tree(name, r.defaultBranch, path));
        warm(app, q.lastCommit(name, r.defaultBranch, path));
      })
      .catch(() => undefined);
  };

  return [
    { id: "home", path: PATTERNS.home, component: home },
    { id: "signin", path: PATTERNS.signin, component: signin },
    { id: "settings", path: PATTERNS.settings, component: settings },
    { id: "inbox", path: PATTERNS.inbox, component: inbox },
    { id: "search", path: PATTERNS.search, component: search },
    { id: "stacks", path: PATTERNS.stacks, component: lists },
    { id: "repos", path: PATTERNS.repos, component: lists },
    { id: "me", path: PATTERNS.me, component: lists },
    { id: "agents", path: PATTERNS.agents, component: lists },
    { id: "agentsNew", path: PATTERNS.agentsNew, component: lists },
    { id: "reviews", path: PATTERNS.reviews, component: inbox },
    { id: "install", path: PATTERNS.install, component: info },
    { id: "docs", path: PATTERNS.docs, component: info },
    { id: "licenses", path: PATTERNS.licenses, component: info },
    { id: "privacy", path: PATTERNS.privacy, component: info },
    { id: "newChange", path: PATTERNS.newChange, component: newPage },
    { id: "newRepo", path: PATTERNS.newRepo, component: newPage },
    { id: "repo", path: PATTERNS.repo, component: repoPage, load: loadTree },
    { id: "tree", path: PATTERNS.tree, component: repoPage, load: loadTree },
    {
      id: "blob",
      path: PATTERNS.blob,
      component: blob,
      load: (p) => warm(app, q.file(nameOf(p), p.ref ?? "", p.path ?? "")),
    },
    {
      id: "changes",
      path: PATTERNS.changes,
      component: changes,
      load: (p) => warm(app, q.changes(nameOf(p))),
    },
    { id: "blame", path: PATTERNS.blame, component: blame },
    { id: "history", path: PATTERNS.history, component: history },
    { id: "commit", path: PATTERNS.commit, component: commit },
    { id: "change", path: PATTERNS.change, component: change },
    { id: "actions", path: PATTERNS.actions, component: actions },
    { id: "run", path: PATTERNS.run, component: run },
    { id: "repoSearch", path: PATTERNS.repoSearch, component: search },
    { id: "repoSettings", path: PATTERNS.repoSettings, component: repoSettings },
  ];
}
