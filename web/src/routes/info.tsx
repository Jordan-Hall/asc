// The product's own pages: install, documentation, licences and what this web app stores in the
// browser. Static content; the install commands are the ones scripts/install.sh and
// scripts/install.ps1 accept (test/info.test.ts reads both scripts to keep them equal).
import type { ReactNode } from "react";
import { Page, Panel } from "../components/bits";
import { Link, useRouteId } from "../lib/router";
import { useTitle } from "../lib/title";

export const INSTALL_SH = `export ASC_DOWNLOAD_BASE_URL=https://downloads.example.org/asc
sh install.sh --dry-run
sh install.sh`;

export const INSTALL_SH_PINNED = `sh install.sh --base-url https://downloads.example.org/asc --version 1.2.3 --prefix "$HOME/.local/share/asc-install" --bin-dir "$HOME/.local/bin"`;

export const INSTALL_PS1 = `$env:ASC_DOWNLOAD_BASE_URL = 'https://downloads.example.org/asc'
./install.ps1 -DryRun
./install.ps1`;

export const INSTALL_PS1_PINNED = `./install.ps1 -BaseUrl https://downloads.example.org/asc -Version 1.2.3 -Prefix "$env:LOCALAPPDATA\\Programs\\asc" -NoModifyPath`;

export const INSTALL_SOURCE = `cargo build --release -p asc`;

function Code(props: { label: string; children: string }): ReactNode {
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

function Section(props: { id: string; title: string; children: ReactNode }): ReactNode {
  return (
    <section
      aria-labelledby={props.id}
      className="grid gap-3 border-b border-line p-4 last:border-b-0"
    >
      <h2 id={props.id} className="m-0 text-lg font-semibold text-fg-strong">
        {props.title}
      </h2>
      {props.children}
    </section>
  );
}

function Install(): ReactNode {
  return (
    <Page
      title="Install asc"
      sub="Releases are served from a download directory you choose: your ASC Hub or any static host. Replace the address below with yours."
    >
      <Panel>
        <Section id="in-unix" title="Linux and macOS">
          <p className="m-0 text-sm">
            Run <code>install.sh</code> from <code>scripts/</code> in the repository. It installs to{" "}
            <code>~/.local/share/asc-install</code> and links <code>~/.local/bin/asc</code>.
            <code> --dry-run</code> prints the plan and writes nothing.
          </p>
          <Code label="Install on Linux or macOS">{INSTALL_SH}</Code>
          <p className="m-0 text-sm">Pin a version and choose the directories:</p>
          <Code label="Install a pinned version">{INSTALL_SH_PINNED}</Code>
        </Section>
        <Section id="in-win" title="Windows">
          <p className="m-0 text-sm">
            Run <code>install.ps1</code> from <code>scripts/</code>. It installs to{" "}
            <code>%LOCALAPPDATA%\Programs\asc</code> and adds it to your user <code>PATH</code>;
            <code> -NoModifyPath</code> skips that.
          </p>
          <Code label="Install on Windows">{INSTALL_PS1}</Code>
          <Code label="Install a pinned version on Windows">{INSTALL_PS1_PINNED}</Code>
        </Section>
        <Section id="in-verify" title="Check the download">
          <p className="m-0 text-sm">
            Set <code>ASC_RELEASE_PUBKEY</code> to the release's minisign public key and the
            installer refuses a download whose <code>SHA256SUMS</code> is not signed by it. Without
            it the checksum proves only that the file matches what the host published.
          </p>
        </Section>
        <Section id="in-source" title="From source">
          <p className="m-0 text-sm">Needs the Rust toolchain pinned in the repository.</p>
          <Code label="Build from source">{INSTALL_SOURCE}</Code>
        </Section>
        <Section id="in-after" title="After installing">
          <Code label="Check and update">{`asc doctor
asc self-update`}</Code>
          <p className="m-0 text-sm">
            <code>asc doctor</code> checks the install and your setup. <code>asc self-update</code>{" "}
            updates an install made by these scripts. Next, read{" "}
            <Link to="/docs">the documentation</Link>.
          </p>
        </Section>
      </Panel>
    </Page>
  );
}

const GUIDES: readonly { file: string; what: string }[] = [
  { file: "docs/GETTING-STARTED.md", what: "Install, authenticate, clone and the daily loop." },
  { file: "docs/CLI.md", what: "Every asc command with an example." },
  { file: "docs/CONFIG.md", what: "Every configuration key, its default and an example." },
  { file: "docs/CHEATSHEET.md", what: "Common tasks and the asc command for each." },
  { file: "docs/ONBOARDING.md", what: "Editor settings, asc doctor and fixing problems." },
  { file: "docs/api.md", what: "The RPC services and how clients reach them." },
  { file: "docs/mcp.md", what: "asc mcp for agents." },
  { file: "docs/WEB-UI.md", what: "How this web app is built and what each screen reads." },
];

function Docs(): ReactNode {
  return (
    <Page title="Documentation" sub="The guides live in the docs directory of the repository.">
      <Panel>
        <Section id="doc-quick" title="The daily loop">
          <Code label="The daily loop">{`asc clone https://asc.example.org/acme/payments
asc status
asc add -p
asc create -m "add parser"
asc create -m "use parser"
asc submit -s
asc sync`}</Code>
          <p className="m-0 text-sm">
            <code>asc create</code> makes a change on top of the current one;{" "}
            <code>asc submit -s</code> sends the stack up as draft changes, which appear under
            Changes in this app. Install first: <Link to="/install">Install asc</Link>.
          </p>
        </Section>
        <Section id="doc-guides" title="Guides">
          <ul className="m-0 grid list-none gap-2 p-0">
            {GUIDES.map((g) => (
              <li key={g.file} className="text-sm">
                <code>{g.file}</code> {g.what}
              </li>
            ))}
          </ul>
        </Section>
        <Section id="doc-api" title="API">
          <p className="m-0 text-sm">
            This app talks to the server only through the asc RPC services, the same ones the CLI,
            the editor plugins and <code>asc mcp</code> use. <code>docs/api.md</code> lists them.
          </p>
        </Section>
      </Panel>
    </Page>
  );
}

function Licenses(): ReactNode {
  return (
    <Page title="Licenses" sub="asc is open source under two licenses; you choose either.">
      <Panel>
        <Section id="lic-asc" title="asc">
          <p className="m-0 text-sm">
            MIT or Apache-2.0, at your option. The texts are the files <code>LICENSE-MIT</code> and{" "}
            <code>LICENSE-APACHE</code> at the root of the repository.
          </p>
        </Section>
        <Section id="lic-deps" title="Dependencies">
          <p className="m-0 text-sm">
            The Rust dependencies are checked against the allow list of licenses in{" "}
            <code>deny.toml</code>. The self-hosted fonts are Inter and JetBrains Mono, both under
            the SIL Open Font License 1.1.
          </p>
        </Section>
      </Panel>
    </Page>
  );
}

const STORED: readonly { key: string; where: string; what: string }[] = [
  {
    key: "asc-token",
    where: "sessionStorage",
    what: "Your token, only if you ask to stay signed in for the tab.",
  },
  { key: "asc-viewer", where: "sessionStorage", what: "The display name you typed at sign-in." },
  { key: "asc-recent", where: "localStorage", what: "The repositories you opened, newest first." },
  { key: "asc-theme", where: "localStorage", what: "Your theme choice." },
  { key: "asc-agents", where: "localStorage", what: "Whether agent features are shown." },
];

function Privacy(): ReactNode {
  return (
    <Page title="Privacy" sub="What this web app keeps in your browser.">
      <Panel>
        <Section id="priv-store" title="Stored in your browser">
          <ul className="m-0 grid list-none gap-2 p-0">
            {STORED.map((s) => (
              <li key={s.key} className="text-sm">
                <code>{s.key}</code> in {s.where}: {s.what}
              </li>
            ))}
          </ul>
        </Section>
        <Section id="priv-sent" title="Sent to the server">
          <p className="m-0 text-sm">
            Your token goes to the server as an <code>Authorization</code> header on each call and
            is never put in a URL. The display name and the list of opened repositories stay in the
            browser. The page loads no third-party scripts, fonts or analytics.
          </p>
        </Section>
      </Panel>
    </Page>
  );
}

export default function InfoPage(): ReactNode {
  const id = useRouteId();
  const title =
    { install: "Install asc", docs: "Documentation", licenses: "Licenses", privacy: "Privacy" }[
      id ?? ""
    ] ?? "ASC Hub";
  useTitle(title);
  if (id === "docs") return <Docs />;
  if (id === "licenses") return <Licenses />;
  if (id === "privacy") return <Privacy />;
  return <Install />;
}
