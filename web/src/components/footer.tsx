// The global footer. Slim on app pages (links, shortcuts, theme); the product footer on the
// signed-out front page adds the section columns. Text only, in the entry chunk. Mobile first:
// one column, 44 px targets, and the bottom edge clears the home indicator.
import type { ReactNode } from "react";
import { useAgents } from "../lib/agents";
import { openPalette } from "../lib/palette";
import { Link } from "../lib/router";
import { THEME_CHOICES, useTheme } from "../lib/theme";
import { Icon, LogoMark } from "./icons";
import { sections } from "./sections";

const ICONS = { "asc-auto": "system", "asc-light": "sun", "asc-dark": "moon" } as const;

function ThemeSwitch(): ReactNode {
  const { pref, setPref } = useTheme();
  return (
    <div
      role="radiogroup"
      aria-label="Theme"
      className="inline-flex rounded-lg border border-line p-0.5"
    >
      {THEME_CHOICES.map((t) => (
        <button
          key={t.value}
          type="button"
          role="radio"
          aria-checked={pref === t.value}
          aria-label={t.label}
          title={t.label}
          onClick={() => setPref(t.value)}
          className={`inline-flex h-11 w-11 items-center justify-center rounded-md sm:h-7 sm:w-8 ${
            pref === t.value
              ? "bg-subtle-hover text-fg-strong"
              : "text-fg-muted hover:text-fg-strong"
          }`}
        >
          <Icon name={ICONS[t.value]} size={14} />
        </button>
      ))}
    </div>
  );
}

/** "Show agent features": off by default, kept in this browser. */
function AgentsSwitch(): ReactNode {
  const { on, setOn } = useAgents();
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      onClick={() => setOn(!on)}
      className="inline-flex h-11 items-center gap-2.5 rounded-lg px-1 text-sm text-fg-muted hover:text-fg-strong sm:h-9"
    >
      <span
        aria-hidden="true"
        className={`relative h-5 w-9 shrink-0 rounded-full border ${
          on ? "border-transparent bg-brand" : "border-control bg-subtle"
        }`}
      >
        <span
          className={`absolute top-0.5 h-3.5 w-3.5 rounded-full ${
            on ? "left-[1.1rem] bg-on-brand" : "left-0.5 bg-fg-muted"
          }`}
        />
      </span>
      Show agent features
    </button>
  );
}

const FOOT_LINK =
  "inline-flex min-h-11 items-center text-fg-muted no-underline hover:text-fg-strong hover:underline sm:min-h-8";

export function Footer(props: { rich: boolean; onHelp: () => void }): ReactNode {
  const agents = useAgents();
  const product = sections(agents.on).filter((s) => s.href !== "#clone");
  const columns: readonly { title: string; links: readonly { href: string; label: string }[] }[] = [
    { title: "Product", links: product },
    {
      title: "Build with it",
      links: [
        { href: "#clone", label: "Clone and install" },
        { href: "#keyboard", label: "Keyboard" },
      ],
    },
  ];
  return (
    <footer className="border-t border-line bg-surface pb-[env(safe-area-inset-bottom)]">
      {props.rich ? (
        <div className="mx-auto grid w-full max-w-7xl gap-8 px-4 py-10 sm:grid-cols-2 sm:px-6 md:grid-cols-[1.4fr_1fr_1fr_1fr] md:py-12">
          <div className="max-w-xs sm:col-span-2 md:col-span-1">
            <div className="mb-3 flex items-center gap-2 text-lg font-extrabold text-fg-strong">
              <LogoMark size={28} />
              ASC Hub
            </div>
            <p className="text-sm text-fg-muted">
              Source control for people and their agents. Plain git can still clone it.
            </p>
          </div>
          {columns.map((c) => (
            <nav key={c.title} aria-label={c.title}>
              <h2 className="mb-2 text-sm font-semibold text-fg-strong">{c.title}</h2>
              <ul className="text-sm">
                {c.links.map((l) => (
                  <li key={l.href}>
                    <a href={l.href} className={FOOT_LINK}>
                      {l.label}
                    </a>
                  </li>
                ))}
              </ul>
            </nav>
          ))}
          <nav aria-label="Account">
            <h2 className="mb-2 text-sm font-semibold text-fg-strong">Get started</h2>
            <ul className="text-sm">
              <li>
                <Link to="/signin" className={FOOT_LINK}>
                  Sign in
                </Link>
              </li>
              <li>
                <button
                  type="button"
                  className={`${FOOT_LINK} text-left`}
                  onClick={() => openPalette()}
                >
                  Open a repository
                </button>
              </li>
              <li>
                <button type="button" className={`${FOOT_LINK} text-left`} onClick={props.onHelp}>
                  Keyboard shortcuts
                </button>
              </li>
            </ul>
          </nav>
        </div>
      ) : null}
      <div
        className={`mx-auto flex w-full max-w-7xl flex-wrap items-center gap-x-5 gap-y-1 px-4 text-sm sm:px-6 ${
          props.rich ? "border-t border-line py-4" : "py-3"
        }`}
      >
        <span className="inline-flex items-center gap-2 font-extrabold text-fg-strong">
          <LogoMark size={20} />
          ASC Hub
        </span>
        {props.rich ? null : (
          <nav aria-label="Footer" className="flex flex-wrap gap-x-5">
            <Link to="/" className={FOOT_LINK}>
              Dashboard
            </Link>
            <Link to="/inbox" className={FOOT_LINK}>
              Inbox
            </Link>
            <Link to="/settings" className={FOOT_LINK}>
              Settings
            </Link>
            <button type="button" className={FOOT_LINK} onClick={props.onHelp}>
              Keyboard shortcuts
            </button>
          </nav>
        )}
        <span className="sm:ml-auto" />
        <AgentsSwitch />
        <ThemeSwitch />
      </div>
    </footer>
  );
}
