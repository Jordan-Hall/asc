import type { ReactNode } from "react";
import { useApp } from "../api/queries";
import { useToken } from "../lib/auth";
import { type ThemePref, useTheme } from "../lib/theme";
import { useTitle } from "../lib/title";

const CHOICES: readonly { value: ThemePref; label: string }[] = [
  { value: "system", label: "Match system" },
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
];

export default function SettingsPage(): ReactNode {
  useTitle("Settings");
  const { auth } = useApp();
  const token = useToken(auth);
  const { pref, setPref } = useTheme();
  return (
    <div className="mx-auto w-full max-w-2xl p-4">
      <h1 className="mb-6 text-2xl font-semibold text-fg-strong">Settings</h1>
      <fieldset className="mb-8">
        <legend className="mb-2 text-lg font-semibold text-fg-strong">Theme</legend>
        {CHOICES.map((c) => (
          <label key={c.value} className="mb-1 flex items-center gap-2">
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
      <section aria-labelledby="acct-h">
        <h2 id="acct-h" className="mb-2 text-lg font-semibold text-fg-strong">
          Account
        </h2>
        <p className="mb-3 text-fg-muted">{token ? "Signed in with a token." : "Not signed in."}</p>
        {token ? (
          <button
            type="button"
            onClick={() => auth.signOut()}
            className="h-9 rounded-md border border-control px-4"
          >
            Sign out
          </button>
        ) : null}
      </section>
    </div>
  );
}
