// "Open a repository by name". asc has no repository list RPC yet, so a name typed here (or in the
// command palette) is how a repository joins the list this browser remembers.
import { type ReactNode, useId, useState } from "react";
import { repoHref } from "../lib/paths";
import { parseRepoName, remember } from "../lib/recent";
import { useNavigate } from "../lib/router";

export function OpenRepo(): ReactNode {
  const navigate = useNavigate();
  const id = useId();
  const [value, setValue] = useState("");
  const [invalid, setInvalid] = useState(false);
  return (
    <form
      className="open-repo"
      onSubmit={(e) => {
        e.preventDefault();
        const p = parseRepoName(value);
        setInvalid(!p);
        if (!p) return;
        remember(`${p.owner}/${p.repo}`);
        navigate(repoHref(p.owner, p.repo));
      }}
    >
      <label htmlFor={id} className="open-repo-label">
        Repository name
      </label>
      <div className="open-repo-row">
        <input
          id={id}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder="owner/repo"
          autoComplete="off"
          spellCheck={false}
          aria-invalid={invalid}
          aria-describedby={`${id}-err`}
        />
        <button type="submit">Open</button>
      </div>
      <p id={`${id}-err`} className={invalid ? "open-repo-err" : "open-repo-err open-repo-ok"}>
        {invalid ? "Use the form owner/repo." : " "}
      </p>
    </form>
  );
}
