import type { ReactNode } from "react";
import { useApp } from "../api/queries";
import { useAgents } from "../lib/agents";
import { useToken } from "../lib/auth";
import { THEME_CHOICES, useTheme } from "../lib/theme";
import { useTitle } from "../lib/title";

const ROW = "mb-1 flex min-h-11 items-center gap-3 sm:min-h-9";

export default function SettingsPage(): ReactNode {
  useTitle("Settings");
  const { auth } = useApp();
  const token = useToken(auth);
  const { pref, setPref } = useTheme();
  const agents = useAgents();
  return (
    <div className="mx-auto w-full max-w-2xl p-4 sm:p-6">
      <h1 className="mb-6 text-2xl font-semibold text-fg-strong">Settings</h1>
      <fieldset className="mb-8">
        <legend className="mb-2 text-lg font-semibold text-fg-strong">Theme</legend>
        {THEME_CHOICES.map((c) => (
          <label key={c.value} className={ROW}>
            <input
              type="radio"
              name="theme"
              checked={pref === c.value}
              onChange={() => setPref(c.value)}
            />
            {c.label}
          </label>
        ))}
      </fieldset>
      <section aria-labelledby="agents-h" className="mb-8">
        <h2 id="agents-h" className="mb-2 text-lg font-semibold text-fg-strong">
          Agents
        </h2>
        <label className={ROW}>
          <input
            type="checkbox"
            checked={agents.on}
            onChange={(e) => agents.setOn(e.target.checked)}
          />
          Show agent features
        </label>
        <p className="text-sm text-fg-muted">
          Off by default. When on, the product page describes agents and MCP, and the dashboard
          shows where agent work appears. This choice stays in this browser.
        </p>
      </section>
      <section aria-labelledby="acct-h">
        <h2 id="acct-h" className="mb-2 text-lg font-semibold text-fg-strong">
          Account
        </h2>
        <p className="mb-3 text-fg-muted">{token ? "Signed in with a token." : "Not signed in."}</p>
        {token ? (
          <button
            type="button"
            onClick={() => auth.signOut()}
            className="h-11 rounded-md border border-control px-4 sm:h-9"
          >
            Sign out
          </button>
        ) : null}
      </section>
    </div>
  );
}
