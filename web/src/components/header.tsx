// The global header. Two forms of one component: the product header on the signed-out front page
// (sticky, translucent, section links) and the app header everywhere else (search, create, inbox,
// account). It is in the entry chunk, fixed height, so it paints before any RPC returns.
import { type ReactNode, type RefObject, useMemo } from "react";
import { useApp } from "../api/queries";
import { useToken } from "../lib/auth";
import { openPalette } from "../lib/palette";
import { useRecent } from "../lib/recent";
import { useQueries } from "../lib/query";
import { Link, useNavigate } from "../lib/router";
import { type ThemePref, useTheme } from "../lib/theme";
import { Icon, LogoMark } from "./icons";
import { Menu, type MenuEntry } from "./menu";

const NEXT: Record<ThemePref, ThemePref> = { system: "light", light: "dark", dark: "system" };
const ICON_BTN =
  "inline-flex h-9 min-w-9 items-center justify-center gap-2 rounded-lg px-2 text-sm text-fg " +
  "no-underline hover:bg-subtle-hover";

export function Brand(): ReactNode {
  return (
    <Link
      to="/"
      aria-label="asc home"
      className="inline-flex items-center gap-2 rounded-lg text-lg font-bold tracking-tight text-fg-strong no-underline"
    >
      <LogoMark size={28} />
      <span>asc</span>
    </Link>
  );
}

function ThemeCycle(): ReactNode {
  const { pref, setPref } = useTheme();
  return (
    <button
      type="button"
      className={ICON_BTN}
      aria-label={`Theme: ${pref}. Switch to ${NEXT[pref]}`}
      onClick={() => setPref(NEXT[pref])}
    >
      <Icon name={pref === "system" ? "system" : pref === "dark" ? "moon" : "sun"} />
    </button>
  );
}

/** Number of changes whose turn is the viewer's, across the repositories this browser knows. */
function useInboxTotal(enabled: boolean): number {
  const { queries } = useApp();
  const recent = useRecent();
  const qs = useMemo(
    () => (enabled ? recent.slice(0, 5).map((r) => queries.inboxCount(r)) : []),
    [enabled, recent, queries],
  );
  const entries = useQueries(qs);
  // Section 4 is "waiting on others": not the viewer's turn, so it does not count.
  return entries.reduce((n, e) => n + (e.data ? e.data.total - (e.data.bySection.get(4) ?? 0) : 0), 0);
}

function InboxLink(): ReactNode {
  const count = useInboxTotal(true);
  return (
    <Link to="/inbox" className={`${ICON_BTN} relative`} aria-label={count > 0 ? `Inbox, ${count} waiting` : "Inbox"}>
      <Icon name="inbox" size={18} />
      {count > 0 ? (
        <span
          aria-hidden="true"
          className="absolute -right-0.5 -top-0.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-brand px-1 text-[0.65rem] font-semibold leading-none text-on-brand"
        >
          {count > 99 ? "99+" : count}
        </span>
      ) : null}
    </Link>
  );
}

function CreateMenu(): ReactNode {
  const entries: MenuEntry[] = [
    { kind: "heading", id: "h", label: "Create" },
    {
      kind: "item",
      id: "open",
      label: "Open a repository",
      icon: "repo",
      hint: "Ctrl K",
      onSelect: () => openPalette(),
    },
    { kind: "item", id: "new", label: "New repository", icon: "plus", to: "/new" },
  ];
  return (
    <Menu
      label="Create"
      buttonClassName={`${ICON_BTN} border border-line`}
      button={
        <>
          <Icon name="plus" />
          <Icon name="chevronDown" size={12} />
        </>
      }
      entries={entries}
    />
  );
}

function AccountMenu(props: { onHelp: () => void }): ReactNode {
  const { auth } = useApp();
  const { pref, setPref } = useTheme();
  const entries: MenuEntry[] = [
    { kind: "item", id: "home", label: "Dashboard", icon: "repo", to: "/", hint: "g h" },
    { kind: "item", id: "inbox", label: "Inbox", icon: "inbox", to: "/inbox", hint: "g i" },
    { kind: "item", id: "settings", label: "Settings", icon: "gear", to: "/settings", hint: "g s" },
    { kind: "separator", id: "s1" },
    { kind: "heading", id: "theme", label: "Theme" },
    ...(["system", "light", "dark"] as const).map(
      (p): MenuEntry => ({
        kind: "radio",
        id: `theme-${p}`,
        label: p === "system" ? "Match system" : p === "light" ? "Light" : "Dark",
        icon: p === "system" ? "system" : p === "light" ? "sun" : "moon",
        checked: pref === p,
        onSelect: () => setPref(p),
      }),
    ),
    { kind: "separator", id: "s2" },
    {
      kind: "item",
      id: "palette",
      label: "Command palette",
      icon: "search",
      hint: "Ctrl K",
      onSelect: () => openPalette(),
    },
    {
      kind: "item",
      id: "help",
      label: "Keyboard shortcuts",
      icon: "keyboard",
      hint: "?",
      onSelect: props.onHelp,
    },
    { kind: "separator", id: "s3" },
    {
      kind: "item",
      id: "out",
      label: "Sign out",
      icon: "signout",
      onSelect: () => auth.signOut(),
    },
  ];
  return (
    <Menu
      label="Account menu"
      buttonClassName="inline-flex h-9 w-9 items-center justify-center rounded-full bg-brand text-on-brand ring-2 ring-transparent hover:ring-[rgb(var(--color-text-link-focus-ring))]"
      button={<Icon name="user" size={18} />}
      entries={entries}
    />
  );
}

const SECTIONS: readonly { href: string; label: string }[] = [
  { href: "#stacks", label: "Stacked changes" },
  { href: "#review", label: "Review" },
  { href: "#inbox", label: "Inbox" },
  { href: "#search", label: "Search" },
  { href: "#agents", label: "Agents" },
  { href: "#clone", label: "Clone" },
];

function MarketingHeader(): ReactNode {
  return (
    <header className="sticky top-0 z-40 border-b border-line/70 bg-[rgb(var(--color-background-rgb)/0.82)] backdrop-blur-md">
      <div
        className="mx-auto flex w-full max-w-7xl items-center gap-2 px-4 sm:px-6"
        style={{ height: 64 }}
      >
        <Brand />
        <nav aria-label="Primary" className="ml-6 hidden items-center gap-1 lg:flex">
          {SECTIONS.map((s) => (
            <a
              key={s.href}
              href={s.href}
              className="rounded-lg px-3 py-2 text-sm text-fg-muted no-underline hover:bg-subtle-hover hover:text-fg-strong"
            >
              {s.label}
            </a>
          ))}
        </nav>
        <span className="flex-1" />
        <div className="lg:hidden">
          <Menu
            label="Sections"
            buttonClassName={ICON_BTN}
            button={<Icon name="menu" size={18} />}
            entries={SECTIONS.map(
              (s): MenuEntry => ({ kind: "item", id: s.href, label: s.label, href: s.href }),
            )}
          />
        </div>
        <ThemeCycle />
        <Link
          to="/signin"
          className="inline-flex h-9 items-center rounded-lg bg-brand px-4 text-sm font-semibold text-on-brand no-underline hover:opacity-90"
        >
          Sign in
        </Link>
      </div>
    </header>
  );
}

export function Header(props: {
  marketing: boolean;
  searchRef: RefObject<HTMLInputElement | null>;
  repoPath: string | undefined;
  onHelp: () => void;
}): ReactNode {
  const { auth } = useApp();
  const token = useToken(auth);
  const navigate = useNavigate();
  if (props.marketing) return <MarketingHeader />;
  return (
    <header className="border-b border-line bg-surface">
      <div
        className="mx-auto flex w-full max-w-7xl items-center gap-2 px-4 sm:gap-3 sm:px-6"
        style={{ height: 60 }}
      >
        <Brand />
        <form
          role="search"
          className="ml-2 hidden min-w-0 flex-1 sm:block"
          onSubmit={(e) => {
            e.preventDefault();
            const q = props.searchRef.current?.value.trim();
            if (q) navigate(`${props.repoPath ?? ""}/search?q=${encodeURIComponent(q)}`);
          }}
        >
          <div className="relative max-w-xl">
            <label className="sr-only" htmlFor="global-search">
              Search
            </label>
            <span className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-fg-muted">
              <Icon name="search" />
            </span>
            <input
              id="global-search"
              ref={props.searchRef}
              type="search"
              autoComplete="off"
              placeholder={props.repoPath ? "Search this repository" : "Search or jump to…"}
              className="h-9 w-full rounded-lg border border-control bg-page pl-9 pr-10 text-sm text-fg-strong placeholder:text-fg-muted"
            />
            <kbd className="pointer-events-none absolute right-2 top-1/2 hidden -translate-y-1/2 rounded border border-control px-1.5 font-mono text-xs text-fg-muted md:block">
              /
            </kbd>
          </div>
        </form>
        <span className="flex-1 sm:hidden" />
        <button
          type="button"
          aria-label="Search"
          className={`${ICON_BTN} sm:hidden`}
          onClick={() => openPalette()}
        >
          <Icon name="search" size={18} />
        </button>
        {token ? (
          <>
            <CreateMenu />
            <InboxLink />
            <AccountMenu onHelp={props.onHelp} />
          </>
        ) : (
          <>
            <ThemeCycle />
            <Link
              to="/signin"
              className="inline-flex h-9 items-center rounded-lg bg-brand px-4 text-sm font-semibold text-on-brand no-underline hover:opacity-90"
            >
              Sign in
            </Link>
          </>
        )}
      </div>
    </header>
  );
}
