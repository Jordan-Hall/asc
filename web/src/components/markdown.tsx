// A small, safe Markdown renderer for READMEs. It builds React elements (never HTML strings), so
// hostile input cannot inject markup, and it only links http(s), mailto and relative targets.
// Supported: headings, paragraphs, fenced code, lists, block quotes, rules, `code`, **bold**, *em*,
// [links](url). Images render as their alt text. This module is its own lazy chunk.
import { Fragment, type ReactNode } from "react";

export type Block =
  | { t: "h"; level: number; text: string }
  | { t: "p"; text: string }
  | { t: "code"; lang: string; text: string }
  | { t: "ul" | "ol"; items: string[] }
  | { t: "quote"; text: string }
  | { t: "hr" };

export function parseBlocks(src: string): Block[] {
  const lines = src.replace(/\r\n?/g, "\n").split("\n");
  const out: Block[] = [];
  let para: string[] = [];
  const flush = () => {
    if (para.length > 0) out.push({ t: "p", text: para.join(" ") });
    para = [];
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i] ?? "";
    const fence = /^```\s*([\w+-]*)\s*$/.exec(line);
    if (fence) {
      flush();
      const body: string[] = [];
      for (i++; i < lines.length && !/^```\s*$/.test(lines[i] ?? ""); i++)
        body.push(lines[i] ?? "");
      out.push({ t: "code", lang: fence[1] ?? "", text: body.join("\n") });
      continue;
    }
    const h = /^(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line);
    if (h) {
      flush();
      out.push({ t: "h", level: (h[1] ?? "#").length, text: h[2] ?? "" });
      continue;
    }
    if (/^(-{3,}|\*{3,}|_{3,})\s*$/.test(line)) {
      flush();
      out.push({ t: "hr" });
      continue;
    }
    const li = /^\s*([-*+]|\d+[.)])\s+(.*)$/.exec(line);
    if (li) {
      flush();
      const kind = /\d/.test(li[1] ?? "") ? "ol" : "ul";
      const last = out.at(-1);
      if (last && last.t === kind) last.items.push(li[2] ?? "");
      else out.push({ t: kind, items: [li[2] ?? ""] });
      continue;
    }
    if (/^>\s?/.test(line)) {
      flush();
      const text = line.replace(/^>\s?/, "");
      const last = out.at(-1);
      if (last && last.t === "quote") last.text += ` ${text}`;
      else out.push({ t: "quote", text });
      continue;
    }
    if (line.trim() === "") flush();
    else para.push(line.trim());
  }
  flush();
  return out;
}

export type LinkTarget =
  | { kind: "external"; href: string }
  | { kind: "anchor"; href: string }
  | { kind: "relative"; href: string }
  | { kind: "blocked" };

const SCHEME = /^([a-z][a-z0-9+.-]*):/i;
const EXTERNAL_SCHEMES = new Set(["http", "https", "mailto"]);

// Control, whitespace, zero-width and line-separator characters, which URL parsers skip.
function invisible(c: number): boolean {
  return (
    c <= 0x20 ||
    (c >= 0x7f && c <= 0xa0) ||
    (c >= 0x200b && c <= 0x200f) ||
    c === 0x2028 ||
    c === 0x2029 ||
    c === 0xfeff
  );
}

function stripInvisible(raw: string): string {
  let out = "";
  for (const ch of raw) if (!invisible(ch.codePointAt(0) ?? 0)) out += ch;
  return out;
}
const PROTOCOL_RELATIVE = /^[/\\]{2}/;

/**
 * The one URL policy for everything Markdown can link to. Browsers ignore tabs, newlines and
 * leading control characters inside a URL (a newline in the middle of the word javascript), so
 * control, whitespace and zero-width characters are removed first and the result is what gets
 * judged. Only http, https and mailto leave the app; "#x" stays on the page; a path with no scheme
 * is a relative link into the repository; every other scheme (javascript, data, vbscript, file,
 * blob, ...) and protocol-relative forms are blocked.
 */
export function classifyHref(raw: string): LinkTarget {
  const href = stripInvisible(raw);
  if (href === "") return { kind: "blocked" };
  const scheme = SCHEME.exec(href);
  if (scheme) {
    const name = (scheme[1] ?? "").toLowerCase();
    if (!EXTERNAL_SCHEMES.has(name)) return { kind: "blocked" };
    try {
      const url = new URL(href);
      return EXTERNAL_SCHEMES.has(url.protocol.slice(0, -1))
        ? { kind: "external", href: url.href }
        : { kind: "blocked" };
    } catch {
      return { kind: "blocked" };
    }
  }
  if (PROTOCOL_RELATIVE.test(href) || href.startsWith("\\")) return { kind: "blocked" };
  if (href.startsWith("#")) return { kind: "anchor", href };
  return { kind: "relative", href };
}

const INLINE = /(`[^`]+`)|(\*\*[^*]+\*\*)|(\*[^*\s][^*]*\*)|(!?\[[^\]]*\]\([^)\s]+\))/g;

export interface LinkRenderer {
  (target: Exclude<LinkTarget, { kind: "blocked" }>, children: ReactNode, key: number): ReactNode;
}

export function renderInline(text: string, link: LinkRenderer): ReactNode[] {
  const out: ReactNode[] = [];
  let last = 0;
  let k = 0;
  for (const m of text.matchAll(INLINE)) {
    const at = m.index ?? 0;
    if (at > last) out.push(text.slice(last, at));
    const tok = m[0];
    k += 1;
    if (tok.startsWith("`")) {
      out.push(
        <code key={k} className="rounded bg-subtle px-1 font-mono text-[0.9em]">
          {tok.slice(1, -1)}
        </code>,
      );
    } else if (tok.startsWith("**")) {
      out.push(<strong key={k}>{renderInline(tok.slice(2, -2), link)}</strong>);
    } else if (tok.startsWith("*")) {
      out.push(<em key={k}>{renderInline(tok.slice(1, -1), link)}</em>);
    } else {
      const parsed = /^(!?)\[([^\]]*)\]\(([^)\s]+)\)$/.exec(tok);
      const label = parsed?.[2] ?? "";
      const target = classifyHref(parsed?.[3] ?? "");
      // Images render as their alt text: no markdown-derived URL is ever loaded as a resource.
      if (parsed?.[1] === "!") out.push(label);
      else if (target.kind === "blocked") out.push(<span key={k}>{label}</span>);
      else out.push(link(target, label, k));
    }
    last = at + tok.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

const HEADING_CLASS = ["", "text-2xl", "text-xl", "text-lg", "text-base", "text-sm", "text-sm"];

export interface MarkdownProps {
  source: string;
  /** Renders a link; the default is a plain anchor. */
  link?: LinkRenderer;
}

const plainLink: LinkRenderer = (target, children, key) => (
  <a
    key={key}
    href={target.href}
    rel="noreferrer noopener"
    target={target.kind === "external" ? "_blank" : undefined}
  >
    {children}
  </a>
);

export default function Markdown({ source, link = plainLink }: MarkdownProps): ReactNode {
  const blocks = parseBlocks(source);
  return (
    <div className="space-y-3 leading-relaxed">
      {blocks.map((b, i) => {
        switch (b.t) {
          case "h": {
            const Tag = `h${Math.min(6, b.level)}` as "h1";
            return (
              <Tag
                key={i}
                className={`${HEADING_CLASS[b.level] ?? "text-sm"} mt-4 font-semibold text-fg-strong`}
              >
                {renderInline(b.text, link)}
              </Tag>
            );
          }
          case "p":
            return <p key={i}>{renderInline(b.text, link)}</p>;
          case "code":
            return (
              <pre
                key={i}
                className="overflow-x-auto rounded-md border border-line bg-subtle p-3 font-mono text-sm"
              >
                <code>{b.text}</code>
              </pre>
            );
          case "ul":
          case "ol": {
            const Tag = b.t;
            return (
              <Tag key={i} className={`${b.t === "ul" ? "list-disc" : "list-decimal"} pl-6`}>
                {b.items.map((it, j) => (
                  <li key={j}>{renderInline(it, link)}</li>
                ))}
              </Tag>
            );
          }
          case "quote":
            return (
              <blockquote key={i} className="border-l-4 border-line pl-3 text-fg-muted">
                {renderInline(b.text, link)}
              </blockquote>
            );
          case "hr":
            return <hr key={i} className="border-line" />;
          default:
            return <Fragment key={i} />;
        }
      })}
    </div>
  );
}
