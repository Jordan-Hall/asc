// Small pieces the repository screens share: status badges, the page frame and empty states.
import type { ReactNode } from "react";
import { Link } from "../lib/router";

export type Tone = "good" | "bad" | "warn" | "note" | "plain";

const TONES: Record<Tone, string> = {
  good: "bg-good-bg text-good",
  bad: "bg-danger-bg text-bad",
  warn: "bg-warn-bg text-warn",
  note: "bg-note-bg text-note",
  plain: "bg-subtle text-fg-muted",
};

export function Badge(props: { tone: Tone; children: ReactNode }): ReactNode {
  return (
    <span
      className={`whitespace-nowrap rounded-full px-2 py-0.5 text-xs font-medium ${TONES[props.tone]}`}
    >
      {props.children}
    </span>
  );
}

/** The page column: one h1, an optional line under it, then the content. */
export function Page(props: {
  title: string;
  sub?: ReactNode;
  wide?: boolean;
  children: ReactNode;
}): ReactNode {
  return (
    <div className={`mx-auto w-full p-4 ${props.wide ? "max-w-6xl" : "max-w-3xl"}`}>
      <h1 className="mb-1 text-xl font-semibold text-fg-strong" style={{ lineHeight: "32px" }}>
        {props.title}
      </h1>
      {props.sub ? (
        <p className="mb-4 text-sm text-fg-muted">{props.sub}</p>
      ) : (
        <div className="mb-3" />
      )}
      {props.children}
    </div>
  );
}

export function Panel(props: { children: ReactNode; label?: string }): ReactNode {
  return (
    <section
      aria-label={props.label}
      className="overflow-hidden rounded-lg border border-line bg-surface"
    >
      {props.children}
    </section>
  );
}

export function Empty(props: { children: ReactNode }): ReactNode {
  return <p className="p-8 text-center text-fg-muted">{props.children}</p>;
}

/** Repository sections as links, the current one marked. */
export function Tabs(props: {
  label: string;
  items: readonly { to: string; label: string; current: boolean }[];
}): ReactNode {
  return (
    <nav aria-label={props.label} className="mb-4 flex flex-wrap gap-1 text-sm">
      {props.items.map((i) => (
        <Link
          key={i.to}
          to={i.to}
          aria-current={i.current ? "page" : undefined}
          className={`inline-flex min-h-9 items-center rounded-md px-3 no-underline ${
            i.current ? "bg-subtle font-semibold text-fg-strong" : "text-fg-muted hover:bg-subtle"
          }`}
        >
          {i.label}
        </Link>
      ))}
    </nav>
  );
}
