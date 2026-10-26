// The global footer. Slim on app pages (links, shortcuts, theme); the product footer on the
// signed-out front page adds the section columns. Text only, in the entry chunk.
import type { ReactNode } from "react";
import { openPalette } from "../lib/palette";
import { Link } from "../lib/router";
import { type ThemePref, useTheme } from "../lib/theme";
import { Icon, LogoMark } from "./icons";

const THEMES: readonly { value: ThemePref; label: string; icon: "system" | "sun" | "moon" }[] = [
  { value: "system", label: "Match system", icon: "system" },
  { value: "light", label: "Light", icon: "sun" },
  { value: "dark", label: "Dark", icon: "moon" },
];

function ThemeSwitch(): ReactNode {
  const { pref, setPref } = useTheme();
  return (
    <div
      role="radiogroup"
      aria-label="Theme"
      className="inline-flex rounded-lg border border-line p-0.5"
    >
      {THEMES.map((t) => (
        <button
          key={t.value}
          type="button"
          role="radio"
          aria-checked={pref === t.value}
          aria-label={t.label}
          title={t.label}
          onClick={() => setPref(t.value)}
          className={`inline-flex h-7 w-8 items-center justify-center rounded-md ${
            pref === t.value
              ? "bg-subtle-hover text-fg-strong"
              : "text-fg-muted hover:text-fg-strong"
          }`}
        >
          <Icon name={t.icon} size={14} />
        </button>
      ))}
    </div>
  );
}

const FOOT_LINK = "text-fg-muted no-underline hover:text-fg-strong hover:underline";

const COLUMNS: readonly { title: string; links: readonly { href: string; label: string }[] }[] = [
  {
    title: "Product",
    links: [
      { href: "#stacks", label: "Stacked changes" },
      { href: "#review", label: "Review" },
      { href: "#inbox", label: "Inbox" },
      { href: "#search", label: "Code search" },
    ],
  },
  {
    title: "Build with it",
    links: [
      { href: "#agents", label: "Agents and MCP" },
      { href: "#clone", label: "Clone and install" },
      { href: "#keyboard", label: "Keyboard" },
    ],
  },
];

export function Footer(props: { rich: boolean; onHelp: () => void }): ReactNode {
  return (
    <footer className="border-t border-line bg-surface">
      {props.rich ? (
        <div className="mx-auto grid w-full max-w-7xl gap-8 px-4 py-12 sm:px-6 md:grid-cols-[1.4fr_1fr_1fr_1fr]">
          <div className="max-w-xs">
            <div className="mb-3 flex items-center gap-2 text-lg font-bold text-fg-strong">
              <LogoMark size={28} />
              ASC
            </div>
            <p className="text-sm text-fg-muted">
              Source control built on changesets and stacked review. Plain git can still clone it.
            </p>
          </div>
          {COLUMNS.map((c) => (
            <nav key={c.title} aria-label={c.title}>
              <h2 className="mb-3 text-sm font-semibold text-fg-strong">{c.title}</h2>
              <ul className="space-y-2 text-sm">
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
            <h2 className="mb-3 text-sm font-semibold text-fg-strong">Get started</h2>
            <ul className="space-y-2 text-sm">
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
        className={`mx-auto flex w-full max-w-7xl flex-wrap items-center gap-x-5 gap-y-2 px-4 text-sm sm:px-6 ${
          props.rich ? "border-t border-line py-5" : "py-4"
        }`}
      >
        <span className="inline-flex items-center gap-2 font-semibold text-fg-strong">
          <LogoMark size={20} />
          ASC
        </span>
        {props.rich ? null : (
          <nav aria-label="Footer" className="flex flex-wrap gap-x-5 gap-y-2">
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
        <span className="ml-auto" />
        <ThemeSwitch />
      </div>
    </footer>
  );
}
