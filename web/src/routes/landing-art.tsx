// Interface illustrations for the product page, drawn in markup and CSS (no images). Each is a
// decorative picture of a real screen: the wrapper is role="img" with a text description, and the
// surrounding copy says everything the picture shows.
import type { ReactNode } from "react";
import { Icon } from "../components/icons";

type Tone = "good" | "bad" | "warn" | "note" | "plain" | "turn" | "brand";

const TONE: Record<Tone, string> = {
  good: "bg-good-bg text-good",
  bad: "bg-danger-bg text-bad",
  warn: "bg-warn-bg text-warn",
  note: "bg-note-bg text-note dark:text-fg-strong",
  plain: "bg-subtle-hover text-fg",
  turn: "bg-[rgb(var(--color-accent-emerald)/0.16)] text-signal",
  brand: "bg-brand text-on-brand",
};

export function Chip(props: { tone: Tone; children: ReactNode }): ReactNode {
  return (
    <span
      className={`inline-flex items-center gap-1 whitespace-nowrap rounded-full px-2 py-0.5 text-[11px] font-semibold leading-4 ${TONE[props.tone]}`}
    >
      {props.children}
    </span>
  );
}

const AVATAR: Record<string, string> = {
  AL: "#0e7490",
  GH: "#6d28d9",
  LT: "#065f46",
  YOU: "#334155",
};

function Avatar(props: { who: string; size?: number }): ReactNode {
  const size = props.size ?? 24;
  return (
    <span
      aria-hidden="true"
      className="inline-flex shrink-0 items-center justify-center rounded-full font-semibold text-white"
      style={{
        width: size,
        height: size,
        fontSize: size * 0.4,
        background: AVATAR[props.who] ?? "#334155",
      }}
    >
      {props.who === "YOU" ? "Y" : props.who}
    </span>
  );
}

function Frame(props: {
  label: string;
  url: string;
  children: ReactNode;
  className?: string;
}): ReactNode {
  return (
    <div role="img" aria-label={props.label} className={`lp-win ${props.className ?? ""}`}>
      <div className="lp-bar" aria-hidden="true">
        <span className="lp-dots">
          <i />
          <i />
          <i />
        </span>
        <span className="lp-url">{props.url}</span>
      </div>
      {props.children}
    </div>
  );
}

function Running(): ReactNode {
  return (
    <svg
      className="lp-running text-warn"
      width="12"
      height="12"
      viewBox="0 0 12 12"
      fill="none"
      aria-hidden="true"
    >
      <circle cx="6" cy="6" r="4.5" stroke="currentColor" strokeOpacity=".3" strokeWidth="1.6" />
      <path d="M6 1.5A4.5 4.5 0 0 1 10.5 6" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
    </svg>
  );
}

function Pass(): ReactNode {
  return <Icon name="check" size={12} className="text-good" />;
}

// ---------------------------------------------------------------------------------------------
// The stack panel: four changes, the bottom one already landed. Used in the hero and in "stacks".

interface StackItem {
  id: string;
  title: string;
  version: number;
  state: ReactNode;
  current?: boolean;
  node: "done" | "current" | "open" | "draft";
}

const STACK: readonly StackItem[] = [
  {
    id: "t4w",
    title: "auth: session timeout",
    version: 1,
    state: <Chip tone="plain">Waiting on Grace</Chip>,
    node: "open",
  },
  {
    id: "p9d",
    title: "auth: remember for this tab",
    version: 1,
    state: <Chip tone="warn">Draft</Chip>,
    node: "draft",
  },
  {
    id: "m2x",
    title: "auth: sign-in screen",
    version: 3,
    state: <Chip tone="turn">Your turn</Chip>,
    current: true,
    node: "current",
  },
  {
    id: "k7q",
    title: "auth: add token store",
    version: 2,
    state: <Chip tone="good">Merged</Chip>,
    node: "done",
  },
];

function Node(props: { kind: StackItem["node"] }): ReactNode {
  const base = "relative z-10 mt-0.5 flex h-[18px] w-[18px] shrink-0 items-center justify-center rounded-full";
  switch (props.kind) {
    case "done":
      return (
        <span className={`${base} bg-signal text-on-brand`}>
          <Icon name="check" size={11} />
        </span>
      );
    case "current":
      return (
        <span className={`${base} border-2 border-accent bg-surface`}>
          <span className="h-2 w-2 rounded-full bg-accent" />
        </span>
      );
    case "draft":
      return <span className={`${base} border-2 border-dashed border-control bg-surface`} />;
    default:
      return <span className={`${base} border-2 border-control bg-surface`} />;
  }
}

export function StackPanel(props: { compact?: boolean }): ReactNode {
  return (
    <div className="p-3">
      <div className="mb-2 flex items-center justify-between px-1 text-[11px] font-semibold uppercase tracking-wide text-fg-muted">
        <span>Stack · 4 changes</span>
        <span className="inline-flex items-center gap-1 normal-case tracking-normal">
          <Icon name="branch" size={12} /> feature-auth
        </span>
      </div>
      <ol className="lp-rail space-y-1.5">
        {STACK.map((c) => (
          <li
            key={c.id}
            className={`relative flex gap-2.5 rounded-lg border px-2.5 py-2 ${
              c.current ? "border-accent bg-surface shadow-sm" : "border-transparent"
            }`}
          >
            <Node kind={c.node} />
            <div className="min-w-0 flex-1">
              <div className="truncate text-[13px] font-medium text-fg-strong">{c.title}</div>
              <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-[11px] text-fg-muted">
                <span className="font-mono">{c.id}</span>
                <span>v{c.version}</span>
                {props.compact ? null : c.state}
              </div>
            </div>
          </li>
        ))}
      </ol>
      <div className="mt-2 flex items-center gap-2 px-1 text-[11px] text-fg-muted">
        <Icon name="merge" size={12} />
        <span>
          lands onto <span className="font-mono text-fg">main</span>, bottom first
        </span>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// The review: a diff with a thread anchored to a line.

function Ln(props: {
  n: string;
  sign?: " " | "+" | "-";
  kind?: "add" | "del";
  children: ReactNode;
}): ReactNode {
  return (
    <div className={`lp-line ${props.kind === "add" ? "lp-add" : props.kind === "del" ? "lp-del" : ""}`}>
      <span>{props.n}</span>
      <span>{props.sign ?? " "}</span>
      <span>{props.children}</span>
    </div>
  );
}

function SignInDiff(): ReactNode {
  return (
    <div className="lp-code">
      <Ln n="40">
        <span className="tk-k">const</span> [token, setToken] = <span className="tk-f">useState</span>(
        <span className="tk-s">""</span>);
      </Ln>
      <Ln n="41" sign="-" kind="del">
        {"  "}
        <span className="tk-k">const</span> <span className="tk-f">onSubmit</span> = () =&gt;
        auth.<span className="tk-f">signIn</span>(token);
      </Ln>
      <Ln n="41" sign="+" kind="add">
        {"  "}
        <span className="tk-k">const</span> <span className="tk-f">onSubmit</span> = () =&gt; {"{"}
      </Ln>
      <Ln n="42" sign="+" kind="add">
        {"    "}auth.<span className="tk-f">signIn</span>(token, remember);
      </Ln>
      <Ln n="43" sign="+" kind="add">
        {"    "}<span className="tk-f">setToken</span>(<span className="tk-s">""</span>);
      </Ln>
      <Ln n="44" sign="+" kind="add">
        {"  };"}
      </Ln>
      <Ln n="45">
        {"  "}
        <span className="tk-k">return</span> &lt;<span className="tk-f">form</span> onSubmit={"{onSubmit}"}&gt;…
      </Ln>
    </div>
  );
}

function Thread(props: { resolved?: boolean }): ReactNode {
  return (
    <div className="mx-3 my-2.5 overflow-hidden rounded-xl border border-line bg-surface text-[13px] shadow-sm">
      <div className="flex gap-2.5 p-3">
        <Avatar who="AL" />
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-x-2 text-[12px]">
            <span className="font-semibold text-fg-strong">Ada</span>
            <span className="text-fg-muted">on line 43 · v2</span>
          </div>
          <p className="mt-1 text-fg">Clear the field here so the token does not sit in state after submit.</p>
        </div>
      </div>
      <div className="flex gap-2.5 border-t border-line bg-subtle/60 p-3">
        <Avatar who="YOU" />
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-x-2 text-[12px]">
            <span className="font-semibold text-fg-strong">You</span>
            <span className="text-fg-muted">v3</span>
            {props.resolved ? <Chip tone="good">Resolved in v3</Chip> : null}
          </div>
          <p className="mt-1 text-fg">Done, and the form resets on failure too.</p>
        </div>
      </div>
    </div>
  );
}

function ChangeHeader(): ReactNode {
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-line px-4 py-3">
      <div className="min-w-0">
        <div className="truncate text-[15px] font-semibold text-fg-strong">auth: sign-in screen</div>
        <div className="mt-1 flex flex-wrap items-center gap-1.5">
          <Chip tone="note">Open</Chip>
          <Chip tone="turn">Your turn</Chip>
          <Chip tone="good">
            <Pass /> Checks passing
          </Chip>
          <span className="text-[11px] text-fg-muted">m2x · v3</span>
        </div>
      </div>
      <div className="ml-auto hidden items-center gap-2 sm:flex">
        <span className="inline-flex h-8 items-center rounded-lg border border-control px-3 text-[12px] font-medium text-fg">
          Request changes
        </span>
        <span className="inline-flex h-8 items-center rounded-lg bg-brand px-3 text-[12px] font-semibold text-on-brand">
          Approve
        </span>
      </div>
    </div>
  );
}

function FileHeader(props: { path: string; stat: string }): ReactNode {
  return (
    <div className="flex items-center gap-2 border-y border-line bg-subtle px-4 py-1.5 text-[12px]">
      <Icon name="file" size={13} className="text-fg-muted" />
      <span className="truncate font-mono text-fg-strong">{props.path}</span>
      <span className="ml-auto whitespace-nowrap font-mono text-fg-muted">{props.stat}</span>
    </div>
  );
}

export function HeroWindow(): ReactNode {
  return (
    <div className="relative">
      <Frame
        label="The review screen. A stack of four changes sits on the left. The selected change shows a diff and a review thread that Ada started and you resolved."
        url="acme/payments / changes / m2x"
      >
        <div className="grid md:grid-cols-[17.5rem_minmax(0,1fr)]" aria-hidden="true">
          <aside className="hidden border-r border-line bg-subtle/70 md:block">
            <StackPanel />
          </aside>
          <div className="min-w-0">
            <ChangeHeader />
            <div className="flex items-center gap-2 px-4 py-2 text-[12px] text-fg-muted">
              <Icon name="clock" size={13} />
              <span>
                Showing <span className="font-semibold text-fg-strong">v2 → v3</span>: only what changed since you last
                looked
              </span>
            </div>
            <FileHeader path="src/routes/signin.tsx" stat="+5 −1" />
            <SignInDiff />
            <Thread resolved />
          </div>
        </div>
      </Frame>
      <div
        aria-hidden="true"
        className="absolute -bottom-6 -right-3 hidden w-60 rounded-xl border border-line bg-surface p-3 text-[12px] shadow-xl lg:block"
      >
        <div className="mb-2 flex items-center justify-between font-semibold text-fg-strong">
          Checks <Chip tone="warn">1 running</Chip>
        </div>
        <ul className="space-y-1.5 text-fg">
          <li className="flex items-center gap-2">
            <Pass /> lint <span className="ml-auto text-fg-muted">18 s</span>
          </li>
          <li className="flex items-center gap-2">
            <Pass /> test <span className="ml-auto text-fg-muted">2 m 04 s</span>
          </li>
          <li className="flex items-center gap-2">
            <Running /> build <span className="ml-auto text-fg-muted">running</span>
          </li>
        </ul>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------

export function StacksArt(): ReactNode {
  return (
    <Frame
      label="A stack of four changes onto main: session timeout, remember for this tab (draft), sign-in screen (your turn) and add token store (merged)."
      url="acme/payments / changes"
    >
      <div aria-hidden="true">
        <div className="border-b border-line px-4 py-3">
          <div className="text-[15px] font-semibold text-fg-strong">feature-auth</div>
          <div className="mt-0.5 text-[12px] text-fg-muted">4 changes onto main · 1 landed · restacked just now</div>
        </div>
        <StackPanel />
        <div className="mx-3 mb-3 flex items-center gap-2 rounded-lg border border-line bg-subtle px-3 py-2 font-mono text-[12px] text-fg">
          <Icon name="terminal" size={13} className="text-fg-muted" />
          <span>
            asc sync <span className="text-fg-muted">→ dropped k7q, restacked 3 changes</span>
          </span>
        </div>
      </div>
    </Frame>
  );
}

export function ReviewArt(): ReactNode {
  return (
    <Frame
      label="Version picker with v1, v2 and v3, the diff between v2 and v3, and a thread that moved with the code and was resolved in v3."
      url="acme/payments / changes / m2x / v2…v3"
    >
      <div aria-hidden="true">
        <div className="flex items-center gap-1.5 border-b border-line px-4 py-3 text-[12px]">
          <span className="mr-1 text-fg-muted">Versions</span>
          {["v1", "v2", "v3"].map((v, i) => (
            <span
              key={v}
              className={`rounded-md border px-2 py-1 font-mono font-semibold ${
                i === 1 || i === 2
                  ? "border-accent bg-[rgb(var(--color-primary)/0.12)] text-fg-strong"
                  : "border-line text-fg-muted"
              }`}
            >
              {v}
            </span>
          ))}
          <span className="ml-auto hidden text-fg-muted sm:inline">since your last review</span>
        </div>
        <FileHeader path="src/routes/signin.tsx" stat="+5 −1" />
        <SignInDiff />
        <Thread resolved />
        <div className="flex items-center gap-2 border-t border-line px-4 py-2.5 text-[12px] text-fg-muted">
          <Icon name="comment" size={13} />
          <span>1 thread resolved · 0 open · 2 approvals</span>
        </div>
      </div>
    </Frame>
  );
}

interface InboxRow {
  who: string;
  title: string;
  meta: string;
  chip: ReactNode;
  section: "turn" | "wait";
}

const INBOX: readonly InboxRow[] = [
  {
    who: "AL",
    title: "auth: sign-in screen",
    meta: "acme/payments · m2x · Ada asked for your review",
    chip: <Chip tone="turn">Review requested</Chip>,
    section: "turn",
  },
  {
    who: "YOU",
    title: "billing: retry failed charges",
    meta: "acme/payments · r5c · 2 open threads from Grace",
    chip: <Chip tone="turn">Feedback to address</Chip>,
    section: "turn",
  },
  {
    who: "YOU",
    title: "ledger: batch inserts",
    meta: "acme/ledger · b8e · the newest check run failed",
    chip: <Chip tone="bad">Checks failed</Chip>,
    section: "turn",
  },
  {
    who: "GH",
    title: "docs: webhook signing",
    meta: "acme/docs · w1n · you left 1 open thread",
    chip: <Chip tone="plain">Waiting on Grace</Chip>,
    section: "wait",
  },
];

export function InboxArt(): ReactNode {
  return (
    <Frame
      label="Inbox with two groups: three changes that are your turn (review requested, feedback to address, checks failed) and one change waiting on someone else."
      url="inbox"
    >
      <div aria-hidden="true">
        <div className="flex items-center gap-2 border-b border-line px-4 py-3 text-[13px]">
          <span className="rounded-lg bg-subtle-hover px-3 py-1.5 font-semibold text-fg-strong">
            Your turn <span className="ml-1 text-signal">3</span>
          </span>
          <span className="rounded-lg px-3 py-1.5 text-fg-muted">
            Waiting <span className="ml-1">1</span>
          </span>
          <span className="ml-auto hidden items-center gap-1 text-[12px] text-fg-muted sm:flex">
            <kbd className="rounded border border-control px-1 font-mono">j</kbd>
            <kbd className="rounded border border-control px-1 font-mono">k</kbd> move
          </span>
        </div>
        <ul className="divide-y divide-line">
          {INBOX.map((r, i) => (
            <li
              key={r.title}
              className={`flex items-center gap-3 px-4 py-3 ${i === 0 ? "bg-[rgb(var(--color-accent-emerald)/0.07)]" : ""} ${
                r.section === "wait" ? "opacity-90" : ""
              }`}
            >
              <Avatar who={r.who} size={28} />
              <div className="min-w-0 flex-1">
                <div className="truncate text-[13px] font-semibold text-fg-strong">{r.title}</div>
                <div className="truncate text-[12px] text-fg-muted">{r.meta}</div>
              </div>
              <span className="hidden sm:block">{r.chip}</span>
            </li>
          ))}
        </ul>
      </div>
    </Frame>
  );
}

export function SearchArt(): ReactNode {
  return (
    <Frame
      label="Code search for parseToken with a definition, three references and matching files."
      url="search?q=parseToken"
    >
      <div aria-hidden="true">
        <div className="flex items-center gap-2 border-b border-line px-4 py-3">
          <Icon name="search" className="text-fg-muted" />
          <span className="text-[14px] text-fg-strong">parseToken</span>
          <span className="ml-auto flex gap-1.5">
            <Chip tone="plain">repo:acme/payments</Chip>
            <span className="hidden sm:inline">
              <Chip tone="plain">lang:ts</Chip>
            </span>
          </span>
        </div>
        <div className="px-4 pb-1 pt-3 text-[11px] font-semibold uppercase tracking-wide text-fg-muted">Definition</div>
        <div className="mx-3 overflow-hidden rounded-lg border border-line">
          <div className="flex items-center gap-2 bg-subtle px-3 py-1.5 text-[12px]">
            <Icon name="file" size={13} className="text-fg-muted" />
            <span className="font-mono text-fg-strong">src/auth/token.ts</span>
            <span className="ml-auto">
              <Chip tone="brand">function</Chip>
            </span>
          </div>
          <div className="lp-code">
            <Ln n="18">
              <span className="tk-k">export function</span> <span className="lp-hit tk-f">parseToken</span>(raw:{" "}
              <span className="tk-f">string</span>) {"{"}
            </Ln>
          </div>
        </div>
        <div className="px-4 pb-1 pt-4 text-[11px] font-semibold uppercase tracking-wide text-fg-muted">
          References · 3
        </div>
        <div className="mx-3 mb-3 overflow-hidden rounded-lg border border-line">
          {[
            ["src/auth/session.ts", "29", "const t = ", "(header);"],
            ["src/routes/signin.tsx", "37", "if (!", "(input)) return;"],
            ["test/token.test.ts", "12", "expect(", "('a.b')).toBeNull();"],
          ].map(([path, n, pre, post]) => (
            <div key={path} className="border-b border-line last:border-b-0">
              <div className="flex items-center gap-2 bg-subtle px-3 py-1 text-[12px]">
                <span className="font-mono text-fg-strong">{path}</span>
              </div>
              <div className="lp-code">
                <Ln n={n ?? ""}>
                  {pre}
                  <span className="lp-hit tk-f">parseToken</span>
                  {post}
                </Ln>
              </div>
            </div>
          ))}
        </div>
      </div>
    </Frame>
  );
}

export function AgentsArt(): ReactNode {
  const row = "grid grid-cols-[4.5rem_minmax(0,1fr)] gap-3 px-4 py-1.5";
  return (
    <Frame
      label="An agent session: it runs review_change and prepare_submit, then a submit is refused because no write scope was granted."
      url="mcp · scoped token: read, review"
    >
      <div aria-hidden="true">
        <div className="flex flex-wrap items-center gap-1.5 border-b border-line px-4 py-3 text-[12px]">
          <span className="mr-1 text-fg-muted">Token scope</span>
          <Chip tone="good">read</Chip>
          <Chip tone="good">review</Chip>
          <Chip tone="plain">write off</Chip>
        </div>
        <div className="lp-code py-2" style={{ lineHeight: "22px" }}>
          <div className={row}>
            <span className="text-fg-muted">you</span>
            <span className="text-fg-strong">Review the stack on feature-auth.</span>
          </div>
          <div className={row}>
            <span className="text-signal">agent</span>
            <span>
              <span className="tk-f">review_change</span> change=<span className="tk-s">m2x</span>
            </span>
          </div>
          <div className={row}>
            <span className="text-fg-muted">←</span>
            <span className="text-fg-muted">3 files · +86 −12 · 1 open thread · checks passing</span>
          </div>
          <div className={row}>
            <span className="text-signal">agent</span>
            <span>
              <span className="tk-f">prepare_submit</span>
            </span>
          </div>
          <div className={row}>
            <span className="text-fg-muted">←</span>
            <span className="text-fg-muted">stack is clean · 4 changes · no restack needed</span>
          </div>
          <div className={row}>
            <span className="text-signal">agent</span>
            <span>
              <span className="tk-f">submit</span> stack=<span className="tk-s">feature-auth</span>
            </span>
          </div>
          <div className={row}>
            <span className="text-bad">✕</span>
            <span className="text-bad">refused: this token has no write scope</span>
          </div>
        </div>
        <div className="flex items-center gap-2 border-t border-line bg-subtle px-4 py-2.5 text-[12px] text-fg-muted">
          <Icon name="shield" size={13} />
          Every call is audited with the token that made it.
        </div>
      </div>
    </Frame>
  );
}
