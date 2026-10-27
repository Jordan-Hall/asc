// New repository and new stack. Both are made with the CLI: the browser has no call that creates a
// repository (Repo.RepoAdmin is not in the browser allowlist) and changes are made from a working
// copy. These pages say so and give the commands.
import type { ReactNode } from "react";
import { Page, Panel } from "../components/bits";
import { Link, useRouteId } from "../lib/router";
import { useTitle } from "../lib/title";

function Command(props: { children: string; label: string }): ReactNode {
  return (
    <pre
      className="m-0 overflow-x-auto rounded-md border border-line bg-subtle p-3 font-mono text-[13px]"
      tabIndex={0}
      aria-label={props.label}
    >
      {props.children}
    </pre>
  );
}

function NewRepo(): ReactNode {
  return (
    <Page
      title="New repository"
      sub="Repositories are created from the command line, not in the browser."
    >
      <Panel>
        <div className="grid gap-4 p-4">
          <section aria-labelledby="new-local">
            <h2 id="new-local" className="mb-2 text-lg font-semibold text-fg-strong">
              Start a repository on your machine
            </h2>
            <Command label="Create a local repository">{`asc init my-project
cd my-project`}</Command>
          </section>
          <section aria-labelledby="new-import">
            <h2 id="new-import" className="mb-2 text-lg font-semibold text-fg-strong">
              Create it on an asc server from an existing repository
            </h2>
            <p className="mb-2 text-sm">
              A server administrator runs this on the server. It creates the repository in mirror
              mode and imports the history.
            </p>
            <Command label="Import an existing repository">{`asc serve admin import --repo acme/payments --init --source https://git.example.org/acme/payments.git`}</Command>
          </section>
          <section aria-labelledby="new-clone">
            <h2 id="new-clone" className="mb-2 text-lg font-semibold text-fg-strong">
              Then clone it and open it here
            </h2>
            <Command label="Clone a repository">{`asc clone https://asc.example.org/acme/payments`}</Command>
            <p className="mt-2 text-sm">
              Open <Link to="/">the dashboard</Link> and enter <code>acme/payments</code> to browse
              it.
            </p>
          </section>
        </div>
      </Panel>
    </Page>
  );
}

function NewStack(): ReactNode {
  return (
    <Page
      title="New stack"
      sub="A stack is made in a working copy. When you submit it, it appears under Changes."
    >
      <Panel>
        <div className="grid gap-4 p-4">
          <Command label="Create and submit a stack">{`asc add -p
asc create -m "add parser"
asc create -m "use parser"
asc submit -s`}</Command>
          <p className="text-sm">
            <code>asc create</code> turns the staged changes into a change on top of the current
            one. <code>asc submit -s</code> sends the whole stack to the server as draft changes.
            Use <code>asc submit --publish</code> to make them reviewable.
          </p>
        </div>
      </Panel>
    </Page>
  );
}

export default function NewPage(): ReactNode {
  const stack = useRouteId() === "newChange";
  useTitle(stack ? "New stack" : "New repository");
  return stack ? <NewStack /> : <NewRepo />;
}
