// The app shell: skip link, top bar, repo tabs, theme switch, shortcut help and the live region that
// announces each navigation. It is in the entry chunk, so it paints before any RPC returns.
import { type ReactNode, useEffect, useRef, useState } from "react";
import { useApp } from "../api/queries";
import { useToken } from "../lib/auth";
import { BINDINGS, useShortcuts } from "../lib/keys";
import { Link, useLocation, useNavigate, useParams } from "../lib/router";
import { type ThemePref, useTheme } from "../lib/theme";
import { Icon } from "./icons";

const NEXT: Record<ThemePref, ThemePref> = { system: "light", light: "dark", dark: "system" };

function ThemeButton(): ReactNode {
  const { pref, setPref } = useTheme();
  const next = NEXT[pref];
  return (
    <button
      type="button"
      className="inline-flex h-9 items-center gap-2 rounded-md border border-control px-3 text-sm hover:bg-subtle-hover"
      aria-label={`Theme: ${pref}. Switch to ${next}`}
      onClick={() => setPref(next)}
    >
      <Icon name={pref === "dark" ? "moon" : "sun"} />
      <span className="hidden sm:inline">{pref}</span>
    </button>
  );
}

function Tab(props: { to: string; current: boolean; children: ReactNode }): ReactNode {
  return (
    <Link
      to={props.to}
      aria-current={props.current ? "page" : undefined}
      className={`inline-flex h-10 items-center border-b-2 px-3 text-sm no-underline ${
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
        className="mx-auto flex w-full max-w-6xl items-center gap-1 overflow-x-auto px-4"
        style={{ minHeight: 40 }}
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

function HelpDialog(props: { dialog: React.RefObject<HTMLDialogElement | null> }): ReactNode {
  return (
    <dialog
      ref={props.dialog}
      aria-labelledby="help-title"
      className="m-auto w-[min(28rem,calc(100vw-2rem))] rounded-lg border border-line bg-surface p-5 text-fg backdrop:bg-black/50"
    >
      <h2 id="help-title" className="mb-3 text-lg font-semibold text-fg-strong">
        Keyboard shortcuts
      </h2>
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
        {BINDINGS.map((b) => (
          <div key={b.id} className="contents">
            <dt className="whitespace-nowrap">
              {b.keys.map((k, i) => (
                <span key={k}>
                  {i > 0 ? " then " : ""}
                  <kbd className="rounded border border-control bg-subtle px-1.5 font-mono text-xs">
                    {k}
                  </kbd>
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
          className="h-9 rounded-md bg-brand px-4 text-sm font-medium text-on-brand"
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
  const dialog = useRef<HTMLDialogElement | null>(null);
  const search = useRef<HTMLInputElement | null>(null);
  const main = useRef<HTMLElement | null>(null);
  const [announce, setAnnounce] = useState("");
  const first = useRef(true);
  const repoPath = owner && repo ? `/${owner}/${repo}` : undefined;

  useShortcuts(BINDINGS, (id) => {
    switch (id) {
      case "help":
        dialog.current?.showModal();
        break;
      case "search":
        search.current?.focus();
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
      <header className="border-b border-line bg-surface">
        <div
          className="mx-auto flex w-full max-w-6xl items-center gap-3 px-4"
          style={{ height: 56 }}
        >
          <Link
            to="/"
            aria-label="asc dashboard"
            className="inline-flex h-8 w-8 items-center justify-center rounded-md bg-brand text-sm font-bold text-on-brand no-underline"
          >
            asc
          </Link>
          <form
            role="search"
            className="ml-2 hidden min-w-0 flex-1 sm:block"
            onSubmit={(e) => {
              e.preventDefault();
              const q = search.current?.value.trim();
              if (q) navigate(`${repoPath ?? ""}/search?q=${encodeURIComponent(q)}`);
            }}
          >
            <label className="sr-only" htmlFor="global-search">
              Search
            </label>
            <input
              id="global-search"
              ref={search}
              type="search"
              placeholder="Search  ( / )"
              className="h-9 w-full max-w-md rounded-md border border-control bg-page px-3 text-sm"
            />
          </form>
          <span className="flex-1 sm:hidden" />
          <Link
            to={`${repoPath ?? ""}/search`}
            aria-label="Search"
            className="inline-flex h-9 w-9 items-center justify-center rounded-md text-fg no-underline hover:bg-subtle-hover sm:hidden"
          >
            <Icon name="search" />
          </Link>
          <Link
            to="/inbox"
            className="inline-flex h-9 items-center gap-2 rounded-md px-2 text-sm text-fg no-underline hover:bg-subtle-hover"
          >
            <Icon name="inbox" />
            <span className="sr-only sm:not-sr-only">Inbox</span>
          </Link>
          <ThemeButton />
          {token ? (
            <button
              type="button"
              className="h-9 rounded-md border border-control px-3 text-sm hover:bg-subtle-hover"
              onClick={() => auth.signOut()}
            >
              Sign out
            </button>
          ) : (
            <Link
              to="/signin"
              className="inline-flex h-9 items-center rounded-md bg-brand px-3 text-sm font-medium text-on-brand no-underline"
            >
              Sign in
            </Link>
          )}
        </div>
      </header>
      {owner && repo ? <RepoTabs owner={owner} repo={repo} /> : null}
      <main id="main" ref={main} tabIndex={-1} className="flex-1 outline-none">
        {props.children}
      </main>
      <div role="status" aria-live="polite" className="sr-only">
        {announce}
      </div>
      <HelpDialog dialog={dialog} />
    </div>
  );
}
