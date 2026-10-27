// The signed-in frame, as drawn in the design boards: the ink header (desktop) or the compact
// header with a menu button (mobile), the navigation drawer, and the bottom tab bar. What the
// header shows follows the route: the dashboard has global search, a repository page has search in
// that repository, the code view has "go to file", a review has its own shortcuts.
import {
  createContext,
  type ReactNode,
  type RefObject,
  useContext,
  useEffect,
  useRef,
  useState,
} from "react";
import { createPortal } from "react-dom";
import { useHome } from "../api/home";
import { useApp } from "../api/queries";
import { useAgents } from "../lib/agents";
import { useToken } from "../lib/auth";
import { useMobile } from "../lib/media";
import { openPalette } from "../lib/palette";
import { useRecent } from "../lib/recent";
import { Link, useLocation, useNavigate, useParams, useRouteId } from "../lib/router";
import { THEME_CHOICES, useTheme } from "../lib/theme";
import { useViewer } from "../lib/viewer";
import { Icon, LogoMark } from "./icons";
import { Menu, type MenuEntry } from "./menu";

/** Number of things that need the viewer, across the repositories this browser knows. */
export function useInboxTotal(enabled: boolean): number {
  return useHome(enabled).needs.length;
}

interface Spec {
  /** Text after the brand: "Inbox", "acme/payments", "acme/payments › Search". */
  crumb?: ReactNode;
  /** A search box. */
  search?: { placeholder: string; key: string };
  /** Palette chip next to the search box. */
  paletteChip?: boolean;
  /** Plain hint on the right instead of the account cluster. */
  hint?: ReactNode;
  /** Inbox, create and the avatar on the right. */
  account: boolean;
}

function Kbd(props: { children: ReactNode }): ReactNode {
  return <kbd className="hd-kbd">{props.children}</kbd>;
}

/** What the header shows on this route. */
function useSpec(): Spec {
  const id = useRouteId();
  const { owner, repo } = useParams();
  const name = owner && repo ? `${owner}/${repo}` : undefined;
  switch (id) {
    case "home":
      return {
        search: { placeholder: "Search code, changes, people", key: "/" },
        paletteChip: true,
        account: true,
      };
    case "inbox":
      return {
        crumb: (
          <>
            <span className="hd-sep">/</span>
            <span className="hd-title">Inbox</span>
          </>
        ),
        hint: (
          <>
            Press <Kbd>?</Kbd> for shortcuts
          </>
        ),
        account: false,
      };
    case "blob":
    case "blame":
    case "history":
      return {
        crumb: <span className="hd-ctx">{name}</span>,
        search: { placeholder: "Go to file or symbol", key: "t" },
        account: false,
      };
    case "change":
      return {
        crumb: <span className="hd-ctx">{name} › Stacks</span>,
        hint: (
          <>
            <Kbd>n</Kbd> next change <Kbd>a</Kbd> approve <Kbd>c</Kbd> comment
          </>
        ),
        account: false,
      };
    case "repoSearch":
      return { crumb: <span className="hd-ctx">{name} › Search</span>, account: false };
    case "ops":
      return { crumb: <span className="hd-ctx">{name} › Operations</span>, account: false };
    case "actions":
    case "run":
      return { crumb: <span className="hd-ctx">{name} › Checks</span>, account: false };
    case "repoSettings":
      return { crumb: <span className="hd-ctx">{name} › Settings</span>, account: false };
    case "repo":
    case "tree":
    case "changes":
      return {
        search: { placeholder: `Search in ${name}`, key: "/" },
        account: true,
      };
    default:
      return {
        search: { placeholder: "Search code, changes, people", key: "/" },
        paletteChip: true,
        account: true,
      };
  }
}

function Avatar(props: { size?: number }): ReactNode {
  const viewer = useViewer();
  return (
    <span
      className="hd-avatar"
      style={props.size ? { width: props.size, height: props.size } : undefined}
    >
      {viewer ? viewer.initials : <Icon name="user" size={16} />}
    </span>
  );
}

function AgentsPill(props: { short?: boolean }): ReactNode {
  const { on } = useAgents();
  if (!on) return null;
  return (
    <Link to="/agents" className="hd-agents">
      <span className="hd-dot" aria-hidden="true" />
      {props.short ? "running" : "agents running"}
    </Link>
  );
}

function InboxLink(): ReactNode {
  const count = useInboxTotal(true);
  return (
    <Link to="/inbox" className="hd-inbox">
      Inbox
      {count > 0 ? (
        <span className="hd-badge">
          {count > 99 ? "99+" : count}
          <span className="sr-only"> waiting</span>
        </span>
      ) : null}
    </Link>
  );
}

function CreateMenu(): ReactNode {
  const entries: MenuEntry[] = [
    { kind: "heading", id: "h", label: "Create" },
    { kind: "item", id: "stack", label: "New stack", icon: "layers", to: "/new/change" },
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
      buttonClassName="hd-plus"
      button={<Icon name="plus" size={18} />}
      entries={entries}
    />
  );
}

export function AccountMenu(props: { onHelp: () => void }): ReactNode {
  const { auth } = useApp();
  const { pref, setPref } = useTheme();
  const agents = useAgents();
  const entries: MenuEntry[] = [
    { kind: "item", id: "home", label: "Dashboard", icon: "repo", to: "/", hint: "g h" },
    { kind: "item", id: "inbox", label: "Inbox", icon: "inbox", to: "/inbox", hint: "g i" },
    { kind: "item", id: "me", label: "Your profile", icon: "user", to: "/people/me" },
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
    { kind: "item", id: "out", label: "Sign out", icon: "signout", onSelect: () => auth.signOut() },
  ];
  return (
    <Menu label="Account menu" buttonClassName="hd-account" button={<Avatar />} entries={entries} />
  );
}

function SearchBox(props: {
  spec: NonNullable<Spec["search"]>;
  inputRef: RefObject<HTMLInputElement | null>;
  repoPath: string | undefined;
}): ReactNode {
  const navigate = useNavigate();
  return (
    <form
      role="search"
      className="hd-search"
      onSubmit={(e) => {
        e.preventDefault();
        const q = props.inputRef.current?.value.trim();
        if (q) navigate(`${props.repoPath ?? ""}/search?q=${encodeURIComponent(q)}`);
      }}
    >
      <label className="hd-sr" htmlFor="global-search">
        Search
      </label>
      <span aria-hidden="true">⌕</span>
      <input
        id="global-search"
        ref={props.inputRef}
        type="search"
        autoComplete="off"
        placeholder={props.spec.placeholder}
      />
      <Kbd>{props.spec.key}</Kbd>
    </form>
  );
}

function DesktopHeader(props: {
  searchRef: RefObject<HTMLInputElement | null>;
  repoPath: string | undefined;
  onHelp: () => void;
}): ReactNode {
  const spec = useSpec();
  const { auth } = useApp();
  const token = useToken(auth);
  return (
    <header className="hd">
      <div className="hd-in">
        <Link to="/" className="hd-brand" aria-label="ASC Hub home">
          <LogoMark size={26} tone="dark" />
          ASC Hub
        </Link>
        {spec.crumb}
        {spec.search ? (
          <SearchBox spec={spec.search} inputRef={props.searchRef} repoPath={props.repoPath} />
        ) : null}
        {spec.paletteChip ? (
          <button type="button" className="hd-chip" onClick={() => openPalette()}>
            <Kbd>Ctrl K</Kbd>
            <span className="hd-sr">Open the command palette</span>
          </button>
        ) : null}
        {spec.hint ? <span className="hd-hint">{spec.hint}</span> : null}
        {spec.account && !token ? (
          <Link to="/signin" className="hd-signin">
            Sign in
          </Link>
        ) : null}
        {spec.account && token ? (
          <div className="hd-right">
            <AgentsPill />
            <InboxLink />
            <CreateMenu />
            <AccountMenu onHelp={props.onHelp} />
          </div>
        ) : null}
      </div>
    </header>
  );
}

function MobileHeader(props: { onMenu: () => void }): ReactNode {
  const { auth } = useApp();
  const token = useToken(auth);
  if (!token) {
    return (
      <header className="hd hd-m">
        <div className="hd-in">
          <Link to="/" className="hd-brand" aria-label="ASC Hub home">
            <LogoMark size={26} tone="dark" />
            ASC Hub
          </Link>
          <Link to="/signin" className="hd-signin hd-signin-m">
            Sign in
          </Link>
        </div>
      </header>
    );
  }
  return (
    <header className="hd hd-m">
      <div className="hd-in">
        <button type="button" className="hd-menu" aria-label="Open menu" onClick={props.onMenu}>
          <Icon name="menu" size={22} />
        </button>
        <Link to="/" className="hd-brand" aria-label="ASC Hub home">
          <LogoMark size={26} tone="dark" />
          ASC Hub
        </Link>
        <AgentsPill short />
        <Link to="/people/me" className="hd-me" aria-label="Your profile">
          <Avatar size={34} />
        </Link>
      </div>
    </header>
  );
}

const REPO_COLORS = ["#125469", "#7c3aed", "#047857", "#b45309", "#be185d", "#475569"];

/** The colour square beside a repository name: by place in the list, as in the boards. */
export function repoColor(index: number): string {
  return REPO_COLORS[index % REPO_COLORS.length] ?? "#125469";
}

/** The "Work", "Repositories" lists, used by the drawer and the dashboard sidebar. */
export function NavLists(props: { onNavigate?: () => void; highlight?: string }): ReactNode {
  const recent = useRecent();
  const { on } = useAgents();
  const home = useHome();
  const total = home.needs.length;
  const { pathname } = useLocation();
  const cur = (p: string) => (pathname === p ? "page" : undefined);
  return (
    <>
      <div className="nl-group">
        <div className="nl-h">Work</div>
        <Link
          to="/inbox"
          aria-current={cur("/inbox")}
          data-active={props.highlight === "/inbox" ? "" : undefined}
          onClick={props.onNavigate}
        >
          Inbox <span className="nl-n">{total > 0 ? total : ""}</span>
        </Link>
        <Link to="/stacks" aria-current={cur("/stacks")} onClick={props.onNavigate}>
          My stacks <span className="nl-n">{home.stacks.length > 0 ? home.stacks.length : ""}</span>
        </Link>
        <Link to="/reviews" aria-current={cur("/reviews")} onClick={props.onNavigate}>
          Review requests{" "}
          <span className="nl-n">{home.reviewRequests > 0 ? home.reviewRequests : ""}</span>
        </Link>
        {on ? (
          <Link to="/agents" aria-current={cur("/agents")} onClick={props.onNavigate}>
            Agent runs
          </Link>
        ) : null}
      </div>
      <div className="nl-group">
        <div className="nl-h">Repositories</div>
        {recent.slice(0, 4).map((r, i) => (
          <Link key={r} to={`/${r}`} onClick={props.onNavigate}>
            <span className="nl-sq" style={{ background: repoColor(i) }} />
            {r}
          </Link>
        ))}
        <Link to="/repos" className="nl-all" onClick={props.onNavigate}>
          All repositories
        </Link>
      </div>
    </>
  );
}

function Drawer(props: { open: boolean; onClose: () => void }): ReactNode {
  const ref = useRef<HTMLDialogElement | null>(null);
  const { pathname } = useLocation();
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (props.open && !d.open) d.showModal();
    if (!props.open && d.open) d.close();
  }, [props.open]);
  // A navigation closes the drawer.
  const first = useRef(true);
  // biome-ignore lint/correctness/useExhaustiveDependencies: pathname is the trigger
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    props.onClose();
  }, [pathname]);
  return (
    <dialog
      ref={ref}
      className="drawer"
      aria-label="Navigation"
      onClose={props.onClose}
      onClick={(e) => {
        if (e.target === ref.current) props.onClose();
      }}
    >
      <div className="drawer-in">
        <div className="drawer-head">
          <Link to="/" className="hd-brand" onClick={props.onClose} aria-label="ASC Hub home">
            <LogoMark size={26} tone="dark" />
            ASC Hub
          </Link>
          <button type="button" className="hd-menu" aria-label="Close menu" onClick={props.onClose}>
            <Icon name="x" size={20} />
          </button>
        </div>
        <nav aria-label="Work and repositories" className="drawer-nav">
          <NavLists onNavigate={props.onClose} />
        </nav>
        <div className="drawer-foot">
          <button
            type="button"
            className="drawer-search"
            onClick={() => {
              props.onClose();
              openPalette();
            }}
          >
            <Icon name="search" size={14} />
            Search or run a command
          </button>
          <Link to="/new/change" className="btn-primary" onClick={props.onClose}>
            New stack
          </Link>
        </div>
      </div>
    </dialog>
  );
}

const BarCtx = createContext<HTMLElement | null>(null);

/** Puts `children` in the bar above the tab bar on a phone (a primary action). Nothing on desktop. */
export function MobileCta(props: { children: ReactNode }): ReactNode {
  const slot = useContext(BarCtx);
  const mobile = useMobile();
  if (!slot || !mobile) return null;
  return createPortal(props.children, slot);
}

function Tab(props: {
  to: string;
  label: string;
  icon: ReactNode;
  current: boolean;
  badge?: number | undefined;
}): ReactNode {
  return (
    <Link to={props.to} aria-current={props.current ? "page" : undefined} className="tab">
      <span className="tab-ic">
        {props.icon}
        {props.badge ? <span className="tab-badge">{props.badge}</span> : null}
      </span>
      {props.label}
    </Link>
  );
}

const svg = (d: ReactNode): ReactNode => (
  <svg
    width="22"
    height="22"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.8"
    strokeLinecap="round"
    strokeLinejoin="round"
    aria-hidden="true"
  >
    {d}
  </svg>
);

function BottomBar(props: { setSlot: (el: HTMLElement | null) => void }): ReactNode {
  const { pathname } = useLocation();
  const total = useInboxTotal(true);
  return (
    <div className="bottom">
      <div ref={props.setSlot} className="bottom-cta" role="region" aria-label="Primary action" />
      <nav aria-label="Primary" className="tabs">
        <Tab
          to="/"
          label="Home"
          current={pathname === "/"}
          icon={svg(<path d="M4 11l8-7 8 7v9a1 1 0 0 1-1 1h-4v-6H9v6H5a1 1 0 0 1-1-1z" />)}
        />
        <Tab
          to="/inbox"
          label="Inbox"
          current={pathname === "/inbox"}
          badge={total || undefined}
          icon={svg(
            <>
              <path d="M4 13l2-8h12l2 8v6a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1z" />
              <path d="M4 13h5l1 2h4l1-2h5" />
            </>,
          )}
        />
        <Tab
          to="/search"
          label="Search"
          current={pathname === "/search" || pathname.endsWith("/search")}
          icon={svg(
            <>
              <circle cx="11" cy="11" r="6.5" />
              <path d="M16 16l4.5 4.5" />
            </>,
          )}
        />
        <Tab
          to="/people/me"
          label="Me"
          current={pathname.startsWith("/people/")}
          icon={svg(
            <>
              <circle cx="12" cy="8" r="4" />
              <path d="M4 20c1-4 4-6 8-6s7 2 8 6" />
            </>,
          )}
        />
      </nav>
    </div>
  );
}

/** The frame of a signed-in page: header, drawer and tab bar around `children`. */
export function AppFrame(props: {
  searchRef: RefObject<HTMLInputElement | null>;
  repoPath: string | undefined;
  onHelp: () => void;
  children: ReactNode;
  mainRef: RefObject<HTMLElement | null>;
}): ReactNode {
  const { auth } = useApp();
  const token = useToken(auth);
  const mobile = useMobile();
  const [drawer, setDrawer] = useState(false);
  const [slot, setSlot] = useState<HTMLElement | null>(null);
  return (
    <BarCtx.Provider value={slot}>
      {mobile ? (
        <MobileHeader onMenu={() => setDrawer(true)} />
      ) : (
        <DesktopHeader
          searchRef={props.searchRef}
          repoPath={props.repoPath}
          onHelp={props.onHelp}
        />
      )}
      <main id="main" ref={props.mainRef} tabIndex={-1} className="app-main">
        {props.children}
      </main>
      {mobile && token ? (
        <>
          <BottomBar setSlot={setSlot} />
          <Drawer open={drawer} onClose={() => setDrawer(false)} />
        </>
      ) : null}
    </BarCtx.Provider>
  );
}
