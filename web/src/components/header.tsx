// The global header. Two forms of one component: the product header on the signed-out front page
// (sticky, section links) and the app header everywhere else (search, create, inbox, account). It is
// ink in every theme, as in the design boards, with the dark tone of the mark. It is in the entry
// chunk, fixed height, so it paints before any RPC returns. Mobile first: 44 px targets, the
// product links collapse into a menu below lg, the search box becomes a button below sm.
import { type ReactNode, type RefObject, useMemo } from "react";
import { useApp } from "../api/queries";
import { useAgents } from "../lib/agents";
import { useToken } from "../lib/auth";
import { openPalette } from "../lib/palette";
import { useRecent } from "../lib/recent";
import { useQueries } from "../lib/query";
import { Link, useNavigate } from "../lib/router";
import { THEME_CHOICES, type ThemePref, useTheme } from "../lib/theme";
import { Icon, LogoMark } from "./icons";
import { Menu, type MenuEntry } from "./menu";
import { sections } from "./sections";

const NEXT: Record<ThemePref, ThemePref> = {
  "asc-auto": "asc-light",
  "asc-light": "asc-dark",
  "asc-dark": "asc-auto",
};
const NAME: Record<ThemePref, string> = {
  "asc-auto": "match system",
  "asc-light": "light",
  "asc-dark": "dark",
};
const ICON_BTN =
  "inline-flex h-11 min-w-11 items-center justify-center gap-2 rounded-lg px-2 text-sm text-hdr-fg " +
  "no-underline hover:bg-hdr-hover sm:h-9 sm:min-w-9";
const CTA =
  "inline-flex h-11 items-center justify-center rounded-lg bg-cyan px-4 text-sm font-semibold " +
  "text-[#0b1220] no-underline hover:brightness-110 sm:h-9";

export function Brand(): ReactNode {
  return (
    <Link
      to="/"
      aria-label="ASC Hub home"
      className="inline-flex h-11 shrink-0 items-center gap-2.5 rounded-lg pr-1 text-lg font-extrabold tracking-tight text-hdr-fg no-underline sm:h-9"
    >
      <LogoMark size={28} tone="dark" />
      <span className="hidden min-[26rem]:inline">ASC Hub</span>
    </Link>
  );
}

function ThemeCycle(): ReactNode {
  const { pref, setPref } = useTheme();
  return (
    <button
      type="button"
      className={ICON_BTN}
      aria-label={`Theme: ${NAME[pref]}. Switch to ${NAME[NEXT[pref]]}`}
      onClick={() => setPref(NEXT[pref])}
    >
      <Icon name={pref === "asc-auto" ? "system" : pref === "asc-dark" ? "moon" : "sun"} />
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
  return entries.reduce(
    (n, e) => n + (e.data ? e.data.total - (e.data.bySection.get(4) ?? 0) : 0),
    0,
  );
}

function InboxLink(): ReactNode {
  const count = useInboxTotal(true);
  return (
    <Link
      to="/inbox"
      className={`${ICON_BTN} relative`}
      aria-label={count > 0 ? `Inbox, ${count} waiting` : "Inbox"}
    >
      <Icon name="inbox" size={18} />
      {count > 0 ? (
        <span
          aria-hidden="true"
          className="absolute right-0 top-1 flex h-4 min-w-4 items-center justify-center rounded-full bg-[#f59e0b] px-1 text-[0.65rem] font-extrabold leading-none text-[#1c1100] sm:-right-0.5 sm:-top-0.5"
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
      buttonClassName={`${ICON_BTN} border border-hdr-line`}
      button={
        <>
          <Icon name="plus" />
          <Icon name="chevronDown" size={12} className="hidden sm:block" />
        </>
      }
      entries={entries}
    />
  );
}

function AccountMenu(props: { onHelp: () => void }): ReactNode {
  const { auth } = useApp();
  const { pref, setPref } = useTheme();
  const agents = useAgents();
  const entries: MenuEntry[] = [
    { kind: "item", id: "home", label: "Dashboard", icon: "repo", to: "/", hint: "g h" },
    { kind: "item", id: "inbox", label: "Inbox", icon: "inbox", to: "/inbox", hint: "g i" },
    { kind: "item", id: "settings", label: "Settings", icon: "gear", to: "/settings", hint: "g s" },
    { kind: "separator", id: "s1" },
    { kind: "heading", id: "theme", label: "Theme" },
    ...THEME_CHOICES.map(
      (c): MenuEntry => ({
        kind: "radio",
        id: `theme-${c.value}`,
        label: c.label,
        icon: c.value === "asc-auto" ? "system" : c.value === "asc-light" ? "sun" : "moon",
        checked: pref === c.value,
        onSelect: () => setPref(c.value),
      }),
    ),
    { kind: "separator", id: "s2" },
    {
      kind: "radio",
      id: "agents",
      label: "Show agent features",
      icon: "bot",
      checked: agents.on,
      onSelect: () => agents.setOn(!agents.on),
    },
    { kind: "separator", id: "s2b" },
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
      buttonClassName="inline-flex h-11 w-11 items-center justify-center rounded-full bg-[#2394b2] text-[#0b1220] ring-2 ring-transparent hover:ring-[#22d3ee] sm:h-9 sm:w-9"
      button={<Icon name="user" size={18} />}
      entries={entries}
    />
  );
}

function MarketingHeader(): ReactNode {
  const agents = useAgents();
  const list = sections(agents.on);
  return (
    <header className="hdr sticky top-0 z-40 border-b border-hdr-line bg-hdr pt-[env(safe-area-inset-top)] text-hdr-fg">
      <div className="mx-auto flex h-14 w-full max-w-7xl items-center gap-1 px-4 sm:h-16 sm:gap-2 sm:px-6">
        <Brand />
        <nav aria-label="Primary" className="ml-6 hidden items-center gap-1 lg:flex">
          {list.map((s) => (
            <a
              key={s.href}
              href={s.href}
              className="rounded-lg px-3 py-2 text-sm font-medium text-hdr-muted no-underline hover:bg-hdr-hover hover:text-hdr-fg"
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
            entries={list.map(
              (s): MenuEntry => ({ kind: "item", id: s.href, label: s.label, href: s.href }),
            )}
          />
        </div>
        <span className="hidden sm:block">
          <ThemeCycle />
        </span>
        <Link to="/signin" className={CTA}>
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
    <header className="hdr sticky top-0 z-40 border-b border-hdr-line bg-hdr pt-[env(safe-area-inset-top)] text-hdr-fg">
      <div className="mx-auto flex h-14 w-full max-w-7xl items-center gap-1 px-4 sm:h-[60px] sm:gap-3 sm:px-6">
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
            <span className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-hdr-muted">
              <Icon name="search" />
            </span>
            <input
              id="global-search"
              ref={props.searchRef}
              type="search"
              autoComplete="off"
              placeholder={props.repoPath ? "Search this repository" : "Search or jump to…"}
              className="h-9 w-full rounded-lg border border-hdr-line bg-hdr-field pl-9 pr-10 text-sm text-hdr-fg placeholder:text-hdr-muted"
            />
            <kbd className="pointer-events-none absolute right-2 top-1/2 hidden -translate-y-1/2 rounded border border-hdr-line px-1.5 font-mono text-xs text-hdr-muted md:block">
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
            <Link to="/signin" className={CTA}>
              Sign in
            </Link>
          </>
        )}
      </div>
    </header>
  );
}
