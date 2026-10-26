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
const planned = () => import("./routes/planned");

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
    { id: "inbox", path: PATTERNS.inbox, component: planned },
    { id: "search", path: PATTERNS.search, component: planned },
    { id: "newRepo", path: PATTERNS.newRepo, component: planned },
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
    { id: "blame", path: PATTERNS.blame, component: planned },
    { id: "history", path: PATTERNS.history, component: planned },
    { id: "commit", path: PATTERNS.commit, component: planned },
    { id: "change", path: PATTERNS.change, component: planned },
    { id: "actions", path: PATTERNS.actions, component: planned },
    { id: "run", path: PATTERNS.run, component: planned },
    { id: "repoSearch", path: PATTERNS.repoSearch, component: planned },
    { id: "repoSettings", path: PATTERNS.repoSettings, component: planned },
  ];
}
