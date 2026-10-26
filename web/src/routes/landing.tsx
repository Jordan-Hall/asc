// The signed-out front page: what the product is, shown with real interface pictures, and the three
// ways in (sign in, open a repository by name, clone). Host-neutral: it names no forge, and the clone
// URLs use the origin that served this page. Lazy chunk; nothing here calls an RPC.
import { type ReactNode, useState } from "react";
import { Icon, type IconName } from "../components/icons";
import { useAgents } from "../lib/agents";
import { parseRepoName, remember } from "../lib/recent";
import { Link, useNavigate } from "../lib/router";
import { repoHref } from "../lib/paths";
import { useTitle } from "../lib/title";
import { AgentsArt, HeroWindow, InboxArt, ReviewArt, SearchArt, StacksArt } from "./landing-art";
import "../styles/landing.css";

const WRAP = "mx-auto w-full max-w-7xl px-4 sm:px-6";

function origin(): string {
  return typeof location === "undefined" ? "https://git.example.com" : location.origin;
}

/** Calls to action: cyan on the ink bands (a fill, with ink text), the brand colour elsewhere. */
const BTN_CYAN =
  "inline-flex items-center justify-center gap-2 rounded-xl bg-cyan font-semibold text-[#0b1220] no-underline hover:brightness-110";
const BTN_GHOST =
  "inline-flex items-center justify-center rounded-xl border border-hdr-line font-semibold text-hdr-fg no-underline hover:bg-hdr-hover";

function CopyButton(props: { text: string; label: string; dark?: boolean }): ReactNode {
  const [done, setDone] = useState(false);
  return (
    <>
      <button
        type="button"
        aria-label={`Copy ${props.label}`}
        onClick={() => {
          navigator.clipboard
            ?.writeText(props.text)
            .then(() => {
              setDone(true);
              setTimeout(() => setDone(false), 1800);
            })
            .catch(() => undefined);
        }}
        className={`inline-flex h-11 w-11 shrink-0 items-center justify-center rounded-md sm:h-8 sm:w-8 ${
          props.dark
            ? "text-hdr-muted hover:bg-hdr-hover hover:text-hdr-fg"
            : "text-fg-muted hover:bg-subtle-hover hover:text-fg-strong"
        }`}
      >
        <Icon name={done ? "check" : "copy"} />
      </button>
      <span role="status" className="sr-only">
        {done ? "Copied" : ""}
      </span>
    </>
  );
}

function Eyebrow(props: { icon: IconName; children: ReactNode }): ReactNode {
  return (
    <p className="mb-3 inline-flex items-center gap-2 text-sm font-bold uppercase tracking-wider text-link">
      <Icon name={props.icon} size={16} />
      {props.children}
    </p>
  );
}

function Bullets(props: { items: readonly ReactNode[] }): ReactNode {
  return (
    <ul className="mt-6 space-y-3">
      {props.items.map((item, i) => (
        <li key={i} className="flex gap-3 text-[15px] text-fg">
          <span className="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-[rgb(var(--color-accent-emerald)/0.16)] text-signal">
            <Icon name="check" size={12} />
          </span>
          <span>{item}</span>
        </li>
      ))}
    </ul>
  );
}

/** A feature: copy on one side, a picture of the screen on the other. */
function Feature(props: {
  id: string;
  icon: IconName;
  eyebrow: string;
  title: string;
  lead: string;
  bullets: readonly ReactNode[];
  art: ReactNode;
  flip?: boolean;
  tinted?: boolean;
}): ReactNode {
  return (
    <section id={props.id} className={props.tinted ? "border-y border-line bg-subtle/50" : ""}>
      <div className={`${WRAP} grid items-center gap-8 py-12 sm:gap-10 md:py-24 lg:grid-cols-2 lg:gap-16`}>
        <div className={props.flip ? "lg:order-2" : ""}>
          <Eyebrow icon={props.icon}>{props.eyebrow}</Eyebrow>
          <h2 className="text-[1.65rem] font-extrabold leading-tight tracking-tight text-fg-strong sm:text-4xl">
            {props.title}
          </h2>
          <p className="mt-4 text-lg leading-relaxed text-fg-muted">{props.lead}</p>
          <Bullets items={props.bullets} />
        </div>
        <div className={`min-w-0 ${props.flip ? "lg:order-1" : ""}`}>{props.art}</div>
      </div>
    </section>
  );
}

const code = "rounded bg-subtle-hover px-1.5 py-0.5 font-mono text-[0.85em] text-fg-strong";

function OpenForm(): ReactNode {
  const navigate = useNavigate();
  const [value, setValue] = useState("");
  const [invalid, setInvalid] = useState(false);
  return (
    <form
      className="w-full max-w-xl"
      onSubmit={(e) => {
        e.preventDefault();
        const p = parseRepoName(value);
        setInvalid(!p);
        if (!p) return;
        remember(`${p.owner}/${p.repo}`);
        navigate(repoHref(p.owner, p.repo));
      }}
    >
      <label htmlFor="open-repo" className="sr-only">
        Open a repository
      </label>
      <div className="flex items-center gap-1 rounded-xl border border-hdr-line bg-hdr-field p-1.5 focus-within:border-cyan">
        <span className="pl-2.5 text-hdr-muted">
          <Icon name="repo" />
        </span>
        <input
          id="open-repo"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder="owner/repo"
          autoComplete="off"
          spellCheck={false}
          aria-invalid={invalid}
          aria-describedby="open-repo-note"
          className="h-11 min-w-0 flex-1 bg-transparent px-1 text-base text-hdr-fg outline-none placeholder:text-hdr-muted"
        />
        <button type="submit" className={`${BTN_CYAN} h-11 px-4 text-sm`}>
          Open
          <Icon name="arrowRight" size={14} />
        </button>
      </div>
      <p
        id="open-repo-note"
        className={`mt-2 min-h-5 text-sm ${invalid ? "text-[#fca5a5]" : "text-hdr-muted"}`}
      >
        {invalid
          ? "Use the form owner/repo."
          : "Public repositories open without an account. Press Ctrl K anywhere to jump."}
      </p>
    </form>
  );
}

/** The ink band at the top: the tagline, the three ways in, and a picture of the product. */
function Hero(): ReactNode {
  const clone = `git clone ${origin()}/acme/payments.git`;
  return (
    <section className="hdr relative overflow-hidden bg-hdr text-hdr-fg">
      <div className="lp-glow" aria-hidden="true" />
      <div className="lp-grid" aria-hidden="true" />
      <div className={`${WRAP} relative pb-14 pt-10 text-center sm:pb-20 sm:pt-16 md:pb-28 md:pt-20`}>
        <p className="mx-auto mb-5 inline-flex max-w-full items-center gap-2 rounded-full border border-hdr-line bg-hdr-field px-3.5 py-1.5 text-left text-[13px] font-semibold text-[#7dd3e8] sm:mb-6">
          <span className="h-2 w-2 shrink-0 rounded-full bg-cyan" aria-hidden="true" />
          Changesets, stacked review, plain git on the wire
        </p>
        <h1 className="mx-auto max-w-4xl text-[2.25rem] font-extrabold leading-[1.06] tracking-[-0.035em] text-white sm:text-6xl lg:text-7xl">
          Source control for people and <span className="text-cyan">their agents.</span>
        </h1>
        <p className="mx-auto mt-5 max-w-2xl text-balance text-base leading-relaxed text-hdr-muted sm:mt-6 sm:text-xl">
          ASC Hub hosts your repositories as stacks of small, reviewable changes. See who or what
          made each one, why, and whose turn it is.
        </p>
        <div className="mt-8 flex flex-col items-center gap-4 sm:mt-9">
          <OpenForm />
          <div className="flex w-full max-w-full flex-col items-stretch justify-center gap-3 sm:w-auto sm:flex-row sm:items-center">
            <Link to="/signin" className={`${BTN_CYAN} h-12 px-6 text-base`}>
              Sign in
            </Link>
            <div className="flex min-w-0 max-w-full items-center gap-1 rounded-xl border border-hdr-line bg-hdr-field py-0.5 pl-4 pr-1">
              <span aria-hidden="true" className="font-mono text-sm text-cyan">
                $
              </span>
              <span
                // Scrolls inside its own box on a narrow screen, so it is reachable by keyboard.
                tabIndex={0}
                role="region"
                aria-label="Clone command text"
                className="scroll-box min-w-0 whitespace-nowrap py-2 text-left font-mono text-sm text-hdr-fg"
              >
                {clone}
              </span>
              <CopyButton text={clone} label="clone command" dark />
            </div>
          </div>
        </div>
        <div className="relative mx-auto mt-12 max-w-6xl sm:mt-16">
          <HeroWindow />
        </div>
      </div>
    </section>
  );
}

const TOOLS = ["git", "VS Code", "JetBrains"];
const AGENT_TOOLS = ["Claude Code", "Cursor", "MCP"];

/** Which tools it works with. Agent tools are listed only when agent features are on. */
function Tools(): ReactNode {
  const { on } = useAgents();
  const list = on ? [...TOOLS, ...AGENT_TOOLS] : TOOLS;
  return (
    <section aria-label="Works with" className="border-b border-line bg-surface">
      <div className={`${WRAP} flex flex-col gap-3 py-6 sm:flex-row sm:items-center sm:justify-between sm:gap-10 sm:py-7`}>
        <p className="max-w-xs text-sm text-fg-muted">Works with the tools your team already uses</p>
        <ul className="flex flex-wrap gap-x-7 gap-y-2 text-base font-bold text-fg-muted sm:text-[17px]">
          {list.map((t) => (
            <li key={t}>{t}</li>
          ))}
        </ul>
      </div>
    </section>
  );
}

const FACTS: readonly { icon: IconName; title: string; text: string }[] = [
  {
    icon: "terminal",
    title: "git clone just works",
    text: "Smart HTTP and SSH for every plain git client.",
  },
  {
    icon: "layers",
    title: "Stable change ids",
    text: "A change keeps its id through amend, rebase and restack.",
  },
  {
    icon: "bolt",
    title: "One API for everything",
    text: "The web app, the CLI and your own tools call the same RPC.",
  },
  {
    icon: "shield",
    title: "Immutable history",
    text: "Content-addressed objects, so reads are cached for good.",
  },
];

function Facts(): ReactNode {
  return (
    <section aria-label="Why it works" className="border-y border-line bg-surface">
      <ul className={`${WRAP} grid gap-px py-6 sm:grid-cols-2 sm:py-10 lg:grid-cols-4`}>
        {FACTS.map((f) => (
          <li key={f.title} className="flex min-w-0 gap-3 py-3 lg:px-4 lg:first:pl-0">
            <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-[rgb(var(--color-primary)/0.12)] text-link">
              <Icon name={f.icon} size={18} />
            </span>
            <div>
              <h2 className="text-[15px] font-semibold text-fg-strong">{f.title}</h2>
              <p className="mt-1 text-sm leading-relaxed text-fg-muted">{f.text}</p>
            </div>
          </li>
        ))}
      </ul>
    </section>
  );
}

type Line = { kind: "cmd" | "note"; text: string };

function Terminal(props: { title: string; lines: readonly Line[]; label: string }): ReactNode {
  const plain = props.lines
    .filter((l) => l.kind === "cmd")
    .map((l) => l.text)
    .join("\n");
  return (
    <div className="overflow-hidden rounded-xl border border-line bg-[rgb(var(--color-background-rgb))] shadow-sm">
      <div
        className="flex items-center gap-2 border-b border-line bg-subtle px-4"
        style={{ height: 40 }}
      >
        <Icon name="terminal" size={14} className="text-fg-muted" />
        <span className="flex-1 text-sm font-medium text-fg-strong">{props.title}</span>
        <CopyButton text={plain} label={props.label} />
      </div>
      <pre
        // Scrollable on a narrow screen, so it must be reachable by keyboard.
        tabIndex={0}
        role="region"
        aria-label={props.label}
        className="overflow-x-auto p-4 font-mono text-[13px] leading-6 text-fg"
      >
        {props.lines.map((l, i) =>
          l.kind === "note" ? (
            <div key={i} className="tk-c">
              {l.text}
            </div>
          ) : (
            <div key={i}>
              <span aria-hidden="true" className="select-none text-fg-muted">
                ${" "}
              </span>
              {l.text}
            </div>
          ),
        )}
      </pre>
    </div>
  );
}

function CloneSection(): ReactNode {
  const base = origin();
  return (
    <section id="clone" className="border-y border-line bg-subtle/50">
      <div className={`${WRAP} py-12 md:py-24`}>
        <div className="mx-auto max-w-3xl text-center">
          <Eyebrow icon="terminal">Git-compatible</Eyebrow>
          <h2 className="text-[1.65rem] font-extrabold leading-tight tracking-tight text-fg-strong sm:text-4xl">
            Clone it with git. Stack it with ASC.
          </h2>
          <p className="mt-4 text-lg leading-relaxed text-fg-muted">
            Every repository answers plain git over smart HTTP and SSH, so editors, CI and scripts
            keep working. Install <code className={code}>asc</code> to get stacks, the operation log
            and lazy working copies.
          </p>
        </div>
        <div className="mt-8 grid gap-6 sm:mt-12 lg:grid-cols-3">
          <Terminal
            title="Clone"
            label="Clone commands"
            lines={[
              { kind: "note", text: "# plain git, nothing to install" },
              { kind: "cmd", text: `git clone ${base}/acme/payments.git` },
              { kind: "note", text: "# or with asc" },
              { kind: "cmd", text: `asc clone ${base}/acme/payments` },
            ]}
          />
          <Terminal
            title="Stack"
            label="Stacking commands"
            lines={[
              { kind: "cmd", text: 'asc create -m "auth: add token store"' },
              { kind: "cmd", text: 'asc create -m "auth: sign-in screen"' },
              { kind: "cmd", text: "asc submit -s" },
              { kind: "note", text: "# after the first one lands" },
              { kind: "cmd", text: "asc sync" },
            ]}
          />
          <Terminal
            title="Install"
            label="Install commands"
            lines={[
              { kind: "note", text: "# Windows" },
              { kind: "cmd", text: "winget install asc.asc" },
              { kind: "note", text: "# Linux (verifies the signature)" },
              { kind: "cmd", text: "sh install.sh --tarball asc-<version>.tar.gz" },
              { kind: "cmd", text: "asc doctor" },
            ]}
          />
        </div>
      </div>
    </section>
  );
}

const GRID: readonly { icon: IconName; title: string; text: string }[] = [
  {
    icon: "keyboard",
    title: "Keyboard first",
    text: "Press / to search, t to find a file, g then c to jump to code, ? for every shortcut.",
  },
  {
    icon: "bolt",
    title: "Fast by construction",
    text: "Lists and files are virtualised, reads under a commit are cached for good, and links prefetch on intent.",
  },
  {
    icon: "eye",
    title: "Accessible",
    text: "Built to WCAG 2.2 AA with light, dark and reduced-motion modes that follow your system.",
  },
  {
    icon: "shield",
    title: "Strict by default",
    text: "A locked-down content policy, no inline scripts, tokens never in URLs and no ambient cookies.",
  },
  {
    icon: "layers",
    title: "Content-addressed",
    text: "Changesets and trees are immutable objects, so history is cheap to cache and safe to share.",
  },
  {
    icon: "repo",
    title: "Yours to run",
    text: "The app is static files served by your own server. It talks to one origin and nothing else.",
  },
];

function Grid(): ReactNode {
  return (
    <section id="keyboard">
      <div className={`${WRAP} py-12 md:py-24`}>
        <div className="mx-auto max-w-3xl text-center">
          <Eyebrow icon="bolt">Details</Eyebrow>
          <h2 className="text-[1.65rem] font-extrabold leading-tight tracking-tight text-fg-strong sm:text-4xl">
            Quick, careful and out of your way
          </h2>
        </div>
        <ul className="mt-8 grid gap-4 sm:mt-12 sm:grid-cols-2 sm:gap-5 lg:grid-cols-3">
          {GRID.map((g) => (
            <li key={g.title} className="lp-card rounded-2xl border border-line bg-surface p-6">
              <span className="mb-4 flex h-10 w-10 items-center justify-center rounded-xl bg-[rgb(var(--color-primary)/0.12)] text-link">
                <Icon name={g.icon} size={20} />
              </span>
              <h3 className="text-lg font-semibold text-fg-strong">{g.title}</h3>
              <p className="mt-2 text-[15px] leading-relaxed text-fg-muted">{g.text}</p>
            </li>
          ))}
        </ul>
      </div>
    </section>
  );
}

function Closing(): ReactNode {
  const { on } = useAgents();
  return (
    <section className={`${WRAP} py-12 md:py-24`}>
      <div className="hdr rounded-2xl bg-hdr px-5 py-12 text-center text-hdr-fg sm:rounded-3xl sm:px-12 sm:py-16">
        <h2 className="mx-auto max-w-2xl text-balance text-[1.75rem] font-extrabold leading-tight tracking-tight text-white sm:text-5xl">
          Ship more, and still understand every line.
        </h2>
        <p className="mx-auto mt-4 max-w-xl text-base text-hdr-muted sm:text-lg">
          Import a repository in a minute. Keep your git history.
          {on ? " Bring your agents." : " Sign in with a token, or open any repository you can read."}
        </p>
        <div className="mt-8 flex flex-col items-stretch justify-center gap-3 sm:flex-row sm:items-center">
          <Link to="/signin" className={`${BTN_CYAN} h-12 px-7 text-base`}>
            Sign in
          </Link>
          <a href="#clone" className={`${BTN_GHOST} h-12 px-6 text-base`}>
            Clone and install
          </a>
        </div>
      </div>
    </section>
  );
}

export default function Landing(): ReactNode {
  useTitle("Source control for people and their agents");
  const { on: agents } = useAgents();
  return (
    <>
      <Hero />
      <Tools />
      <Facts />
      <Feature
        id="stacks"
        icon="layers"
        eyebrow="Stacked changes"
        title="Break big work into a stack. Land it bottom first."
        lead="Each change is one idea with an id that never changes. Reviewers read a small diff against its parent, not a four thousand line branch."
        bullets={[
          "Change ids survive amend, rebase and restack.",
          <>
            Each change is reviewed against its parent, not against{" "}
            <code className={code}>main</code>.
          </>,
          <>
            <code className={code}>asc sync</code> drops what landed and restacks what is left.
          </>,
          "Land the whole stack in order, or one change at a time.",
        ]}
        art={<StacksArt />}
      />
      <Feature
        id="review"
        icon="comment"
        eyebrow="Review"
        title="Review that follows the change"
        lead="Comments belong to the change, not to a commit. Push a new version and threads move with the code, resolved ones stay resolved, and reviewers see only what changed since they last looked."
        bullets={[
          "Threads survive new versions and rebases.",
          "Compare any two versions with an interdiff.",
          "Approvals, checks and merge readiness in one place.",
        ]}
        art={<ReviewArt />}
        flip
        tinted
      />
      <Feature
        id="inbox"
        icon="inbox"
        eyebrow="Inbox"
        title="An inbox that says whose turn it is"
        lead="Four plain sections and no notification noise: reviews waiting on you, changes returned with feedback, checks you need to fix, and threads waiting on someone else."
        bullets={[
          "Anything that is not your turn stays out of your way.",
          "Pin what matters and snooze what does not.",
          "One count across every repository you work in.",
        ]}
        art={<InboxArt />}
      />
      <Feature
        id="search"
        icon="search"
        eyebrow="Code search"
        title="Find the definition, not just the string"
        lead="Search runs on the server against the exact commit you are reading, so results match what is on screen. Jump to definitions and references, or open any path with a few keystrokes."
        bullets={[
          "Symbols, definitions and references.",
          <>
            A file finder over huge trees: press <code className={code}>t</code>.
          </>,
          "Scope it to one repository, one ref, or everything you can read.",
        ]}
        art={<SearchArt />}
        flip
        tinted
      />
      {agents ? (
      <Feature
        id="agents"
        icon="bot"
        eyebrow="Agents and MCP"
        title="Built for agents as well as people"
        lead="ASC ships an MCP server on the same API the web app uses. Give an agent a scoped token and it can read a stack, review a change or prepare a submit, with write tools off until you turn them on."
        bullets={[
          "Remote MCP over HTTP with OAuth, or local over stdio.",
          <>
            Ready-made prompts such as <code className={code}>review_change</code> and{" "}
            <code className={code}>prepare_submit</code>.
          </>,
          "Write tools need an explicit scope, and every call is audited.",
        ]}
        art={<AgentsArt />}
      />
      ) : null}
      <CloneSection />
      <Grid />
      <Closing />
    </>
  );
}
