// Token sign-in (docs/WEB-UI.md section 9). The token goes to the auth store and nowhere else.
import { type ReactNode, useState } from "react";
import { useApp } from "../api/queries";
import { useNavigate } from "../lib/router";
import { useTitle } from "../lib/title";

export default function SignInPage(): ReactNode {
  useTitle("Sign in");
  const { auth } = useApp();
  const navigate = useNavigate();
  const [token, setToken] = useState("");
  const [remember, setRemember] = useState(false);
  return (
    <div className="mx-auto w-full max-w-md p-4">
      <h1 className="mb-2 text-2xl font-semibold text-fg-strong">Sign in</h1>
      <p className="mb-6 text-fg-muted">
        Paste an ASC token. Create one with{" "}
        <code className="font-mono">asc serve token create</code>. It is kept in memory and sent
        only to this server.
      </p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (!token.trim()) return;
          auth.signIn(token, remember);
          setToken("");
          navigate("/", { replace: true });
        }}
      >
        <label htmlFor="token" className="mb-1 block text-sm font-medium">
          Token
        </label>
        <input
          id="token"
          type="password"
          autoComplete="off"
          spellCheck={false}
          value={token}
          onChange={(e) => setToken(e.target.value)}
          className="mb-3 h-10 w-full rounded-md border border-control bg-surface px-3 font-mono"
        />
        <label className="mb-4 flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={remember}
            onChange={(e) => setRemember(e.target.checked)}
          />
          Stay signed in for this tab
        </label>
        <button type="submit" className="h-10 w-full rounded-md bg-brand font-medium text-on-brand">
          Sign in
        </button>
      </form>
    </div>
  );
}
