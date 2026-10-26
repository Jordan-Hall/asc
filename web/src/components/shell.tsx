// The app shell: skip link, global header, repo tabs, footer, shortcut help, the command palette and
// the live region that announces each navigation. It is in the entry chunk, so it paints before any
// RPC returns.
import { lazy, type ReactNode, Suspense, useCallback, useEffect, useRef, useState } from "react";
import { useApp, useQ } from "../api/queries";
import { useToken } from "../lib/auth";
import { BINDINGS, useShortcuts } from "../lib/keys";
import { openPalette, usePalette } from "../lib/palette";
import { remember } from "../lib/recent";
import { Link, useLocation, useNavigate, useParams, useRouteId } from "../lib/router";
import { Footer } from "./footer";
import { Header } from "./header";

const Palette = lazy(() => import("./palette"));

function Tab(props: { to: string; current: boolean; children: ReactNode }): ReactNode {
  return (
    <Link
      to={props.to}
      aria-current={props.current ? "page" : undefined}
      className={`inline-flex h-11 items-center border-b-2 px-3 text-sm no-underline ${
        props.current
          ? "border-accent font-semibold text-fg-strong"
          : "border-transparent text-fg-muted hover:text-fg-strong"
      }`}
    >
      {props.children}
    </Link>
  );
}

function RepoTabs(props: { owner: string; repo: string }): ReactNode {
  const { pathname } = useLocation();
  const base = `/${props.owner}/${props.repo}`;
  const section = pathname.slice(base.length).split("/")[1] ?? "";
  const is = (...names: string[]) => names.includes(section);
  return (
    <nav aria-label="Repository" className="border-b border-line bg-surface">
      <div
        className="mx-auto flex w-full max-w-7xl items-center gap-1 overflow-x-auto px-4 sm:px-6"
        style={{ minHeight: 44 }}
      >
        <span className="mr-3 whitespace-nowrap text-sm font-semibold text-fg-strong">
          {props.owner} / {props.repo}
        </span>
        <Tab to={base} current={is("", "tree", "blob", "blame", "commits", "commit")}>
          Code
        </Tab>
        <Tab to={`${base}/changes`} current={is("changes")}>
          Changes
        </Tab>
        <Tab to={`${base}/actions`} current={is("actions")}>
          Actions
        </Tab>
        <Tab to={`${base}/search`} current={is("search")}>
          Search
        </Tab>
        <Tab to={`${base}/settings`} current={is("settings")}>
          Settings
        </Tab>
      </div>
    </nav>
  );
}

/** Adds a repository to the recent list once it has loaded (so a 404 is never remembered). */
function RememberRepo(props: { name: string }): ReactNode {
  const { queries } = useApp();
  const refs = useQ(queries.refs(props.name));
  const ok = refs.status === "ready";
  useEffect(() => {
    if (ok) remember(props.name);
  }, [ok, props.name]);
  return null;
}

function HelpDialog(props: { dialog: React.RefObject<HTMLDialogElement | null> }): ReactNode {
  const kbd = "rounded border border-control bg-subtle px-1.5 font-mono text-xs";
  return (
    <dialog
      ref={props.dialog}
      aria-labelledby="help-title"
      className="m-auto w-[min(28rem,calc(100vw-2rem))] rounded-2xl border border-line bg-surface p-5 text-fg shadow-2xl backdrop:bg-black/50"
    >
      <h2 id="help-title" className="mb-3 text-lg font-semibold text-fg-strong">
        Keyboard shortcuts
      </h2>
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
        <div className="contents">
          <dt className="whitespace-nowrap">
            <kbd className={kbd}>Ctrl</kbd> <kbd className={kbd}>K</kbd>
          </dt>
          <dd>Command palette</dd>
        </div>
        {BINDINGS.map((b) => (
          <div key={b.id} className="contents">
            <dt className="whitespace-nowrap">
              {b.keys.map((k, i) => (
                <span key={k}>
                  {i > 0 ? " then " : ""}
                  <kbd className={kbd}>{k}</kbd>
                </span>
              ))}
            </dt>
            <dd>{b.label}</dd>
          </div>
        ))}
      </dl>
      <form method="dialog" className="mt-4 text-right">
        <button
          type="submit"
          className="h-9 rounded-lg bg-brand px-4 text-sm font-medium text-on-brand"
        >
          Close
        </button>
      </form>
    </dialog>
  );
}

export function Shell(props: { children: ReactNode }): ReactNode {
  const { auth } = useApp();
  const token = useToken(auth);
  const navigate = useNavigate();
  const { owner, repo } = useParams();
  const { pathname } = useLocation();
  const routeId = useRouteId();
  const palette = usePalette();
  const dialog = useRef<HTMLDialogElement | null>(null);
  const search = useRef<HTMLInputElement | null>(null);
  const main = useRef<HTMLElement | null>(null);
  const [announce, setAnnounce] = useState("");
  const first = useRef(true);
  const repoPath = owner && repo ? `/${owner}/${repo}` : undefined;
  // The signed-out front page is the product page; its header and footer are the marketing forms.
  const marketing = routeId === "home" && !token;
  const showHelp = useCallback(() => dialog.current?.showModal(), []);

  useShortcuts(BINDINGS, (id) => {
    switch (id) {
      case "help":
        showHelp();
        break;
      case "search":
        // The header search box is absent on the product page and hidden on small screens.
        if (search.current && search.current.offsetParent !== null) search.current.focus();
        else openPalette();
        break;
      case "find-file":
        if (repoPath) navigate(`${repoPath}/search?mode=files`);
        break;
      case "go-home":
        navigate("/");
        break;
      case "go-code":
        if (repoPath) navigate(repoPath);
        break;
      case "go-changes":
        if (repoPath) navigate(`${repoPath}/changes`);
        break;
      case "go-actions":
        navigate(repoPath ? `${repoPath}/actions` : "/");
        break;
      case "go-inbox":
        navigate("/inbox");
        break;
      case "go-settings":
        navigate("/settings");
        break;
      default:
        break;
    }
  });

  // Fetch the palette chunk once the page is idle, so Ctrl+K never waits on the network and the
  // first keystrokes after it are not lost.
  useEffect(() => {
    const load = () => void import("./palette");
    if ("requestIdleCallback" in window) {
      const h = window.requestIdleCallback(load, { timeout: 2000 });
      return () => window.cancelIdleCallback(h);
    }
    const h = setTimeout(load, 300);
    return () => clearTimeout(h);
  }, []);

  // Ctrl+K / Cmd+K opens the palette from anywhere, including from inside a text field.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "k") {
        e.preventDefault();
        openPalette();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // On navigation: move focus to the page and announce its title.
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    main.current?.focus({ preventScroll: true });
    setAnnounce(document.title);
  }, [pathname]);

  return (
    <div className="flex min-h-dvh flex-col">
      <a
        href="#main"
        className="sr-only-focusable left-2 top-2 z-50 rounded bg-brand px-3 py-2 text-on-brand"
      >
        Skip to content
      </a>
      <Header marketing={marketing} searchRef={search} repoPath={repoPath} onHelp={showHelp} />
      {owner && repo ? (
        <>
          <RepoTabs owner={owner} repo={repo} />
          <RememberRepo name={`${owner}/${repo}`} />
        </>
      ) : null}
      <main id="main" ref={main} tabIndex={-1} className="flex-1 outline-none">
        {props.children}
      </main>
      <Footer rich={marketing} onHelp={showHelp} />
      <div role="status" aria-live="polite" className="sr-only">
        {announce}
      </div>
      <HelpDialog dialog={dialog} />
      {palette.open ? (
        <Suspense fallback={null}>
          <Palette onHelp={showHelp} />
        </Suspense>
      ) : null}
    </div>
  );
}
