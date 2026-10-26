// The signed-out front page: what the product is, shown with real interface pictures, and the three
// ways in (sign in, open a repository by name, clone). Host-neutral: it names no forge, and the clone
// URLs use the origin that served this page. Lazy chunk; nothing here calls an RPC.
import { type ReactNode, useState } from "react";
import { Icon, type IconName } from "../components/icons";
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

function CopyButton(props: { text: string; label: string }): ReactNode {
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
        className="inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-fg-muted hover:bg-subtle-hover hover:text-fg-strong"
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
    <p className="mb-4 inline-flex items-center gap-2 text-sm font-semibold text-link">
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
      <div className={`${WRAP} grid items-center gap-10 py-16 md:py-24 lg:grid-cols-2 lg:gap-16`}>
        <div className={props.flip ? "lg:order-2" : ""}>
          <Eyebrow icon={props.icon}>{props.eyebrow}</Eyebrow>
          <h2 className="text-3xl font-bold tracking-tight text-fg-strong sm:text-4xl">
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
      <div className="flex items-center gap-2 rounded-xl border border-control bg-surface p-1.5 shadow-sm focus-within:border-accent">
        <span className="pl-2.5 text-fg-muted">
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
          className="h-10 min-w-0 flex-1 bg-transparent px-1 text-base text-fg-strong outline-none placeholder:text-fg-muted"
        />
        <button
          type="submit"
          className="inline-flex h-10 items-center gap-2 rounded-lg bg-brand px-4 text-sm font-semibold text-on-brand hover:opacity-90"
        >
          Open
          <Icon name="arrowRight" size={14} />
        </button>
      </div>
      <p
        id="open-repo-note"
        className={`mt-2 min-h-5 text-sm ${invalid ? "text-bad" : "text-fg-muted"}`}
      >
        {invalid
          ? "Use the form owner/repo."
          : "Public repositories open without an account. Press Ctrl K anywhere to jump."}
      </p>
    </form>
  );
}

function Hero(): ReactNode {
  const clone = `git clone ${origin()}/acme/payments.git`;
  return (
    <section className="relative overflow-hidden">
      <div className="lp-glow" aria-hidden="true" />
      <div className="lp-grid" aria-hidden="true" />
      <div className={`${WRAP} relative pb-20 pt-14 text-center md:pb-28 md:pt-20`}>
        <p className="mx-auto mb-6 inline-flex items-center gap-2 rounded-full border border-line bg-surface/80 px-3.5 py-1.5 text-sm text-fg-muted backdrop-blur">
          <span className="h-2 w-2 rounded-full bg-signal" aria-hidden="true" />
          Changesets, stacked review, plain git on the wire
        </p>
        <h1 className="mx-auto max-w-4xl text-[2.6rem] font-bold leading-[1.04] tracking-[-0.035em] text-fg-strong sm:text-6xl lg:text-7xl">
          Ship small changes.
          <br />
          <span className="lp-accent">Review them in order.</span>
        </h1>
        <p className="mx-auto mt-6 max-w-2xl text-balance text-lg leading-relaxed text-fg-muted sm:text-xl">
          A source host built on changesets. Split work into a stack of small changes, keep every
          comment through every revision, and always know whose turn it is.
        </p>
        <div className="mt-9 flex flex-col items-center gap-4">
          <OpenForm />
          <div className="flex w-full max-w-full flex-wrap items-center justify-center gap-3">
            <Link
              to="/signin"
              className="inline-flex h-11 items-center gap-2 rounded-xl bg-brand px-6 text-base font-semibold text-on-brand no-underline hover:opacity-90"
            >
              Sign in
            </Link>
            <div className="flex min-w-0 max-w-full items-center gap-1 rounded-xl border border-line bg-surface/80 py-1 pl-4 pr-1 backdrop-blur">
              <span aria-hidden="true" className="font-mono text-sm text-fg-muted">
                $
              </span>
              <span className="min-w-0 truncate font-mono text-sm text-fg-strong">{clone}</span>
              <CopyButton text={clone} label="clone command" />
            </div>
          </div>
        </div>
        <div className="relative mx-auto mt-16 max-w-6xl">
          <HeroWindow />
        </div>
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
    text: "The web app, the CLI and agents call the same RPC.",
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
      <ul className={`${WRAP} grid gap-px py-10 sm:grid-cols-2 lg:grid-cols-4`}>
        {FACTS.map((f) => (
          <li key={f.title} className="flex gap-3 py-3 lg:px-4 lg:first:pl-0">
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
      <div className={`${WRAP} py-16 md:py-24`}>
        <div className="mx-auto max-w-3xl text-center">
          <Eyebrow icon="terminal">Git-compatible</Eyebrow>
          <h2 className="text-3xl font-bold tracking-tight text-fg-strong sm:text-4xl">
            Clone it with git. Stack it with ASC.
          </h2>
          <p className="mt-4 text-lg leading-relaxed text-fg-muted">
            Every repository answers plain git over smart HTTP and SSH, so editors, CI and scripts
            keep working. Install <code className={code}>asc</code> to get stacks, the operation log
            and lazy working copies.
          </p>
        </div>
        <div className="mt-12 grid gap-6 lg:grid-cols-3">
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
      <div className={`${WRAP} py-16 md:py-24`}>
        <div className="mx-auto max-w-3xl text-center">
          <Eyebrow icon="bolt">Details</Eyebrow>
          <h2 className="text-3xl font-bold tracking-tight text-fg-strong sm:text-4xl">
            Quick, careful and out of your way
          </h2>
        </div>
        <ul className="mt-12 grid gap-5 sm:grid-cols-2 lg:grid-cols-3">
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
  return (
    <section className={`${WRAP} pb-20`}>
      <div className="lp-cta rounded-3xl border border-line px-6 py-14 text-center sm:px-12">
        <h2 className="mx-auto max-w-2xl text-3xl font-bold tracking-tight text-fg-strong sm:text-5xl">
          Start with a stack.
        </h2>
        <p className="mx-auto mt-4 max-w-xl text-lg text-fg-muted">
          Sign in with a token, or open any repository you can read.
        </p>
        <div className="mt-8 flex flex-wrap items-center justify-center gap-3">
          <Link
            to="/signin"
            className="inline-flex h-12 items-center rounded-xl bg-brand px-7 text-base font-semibold text-on-brand no-underline hover:opacity-90"
          >
            Sign in
          </Link>
          <a
            href="#clone"
            className="inline-flex h-12 items-center rounded-xl border border-control bg-surface px-6 text-base font-medium text-fg no-underline hover:bg-subtle-hover"
          >
            Clone and install
          </a>
        </div>
      </div>
    </section>
  );
}

export default function Landing(): ReactNode {
  useTitle("Stacked changes, reviewed in order");
  return (
    <>
      <Hero />
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
      <CloneSection />
      <Grid />
      <Closing />
    </>
  );
}
