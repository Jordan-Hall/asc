// Path patterns: static segments, `:param` (one segment) and a trailing `*name` (the rest). Pure
// functions so the route table is tested without a DOM.

export type Params = Readonly<Record<string, string>>;

type Seg =
  | { kind: "static"; value: string }
  | { kind: "param"; name: string }
  | { kind: "splat"; name: string };

export interface Pattern {
  readonly source: string;
  readonly segs: readonly Seg[];
  /** Higher is more specific; used to order a route table. */
  readonly score: number;
}

export function compile(source: string): Pattern {
  const parts = source.split("/").filter(Boolean);
  const segs: Seg[] = parts.map((p, i) => {
    if (p.startsWith(":")) return { kind: "param", name: p.slice(1) };
    if (p.startsWith("*")) {
      if (i !== parts.length - 1) throw new Error(`splat must be last in ${source}`);
      return { kind: "splat", name: p.slice(1) || "rest" };
    }
    return { kind: "static", value: p };
  });
  const score = segs.reduce(
    (n, s) => n + (s.kind === "static" ? 3 : s.kind === "param" ? 2 : 1),
    0,
  );
  return { source, segs, score };
}

function decode(segment: string): string | undefined {
  try {
    return decodeURIComponent(segment);
  } catch {
    return undefined;
  }
}

/** Params of `pathname` under `pattern`, or `undefined` when it does not match. */
export function matchPattern(pattern: Pattern, pathname: string): Params | undefined {
  const parts = pathname.split("/").filter(Boolean);
  const params: Record<string, string> = {};
  const { segs } = pattern;
  for (let i = 0; i < segs.length; i++) {
    const seg = segs[i];
    if (!seg) return undefined;
    if (seg.kind === "splat") {
      const rest = parts.slice(i).map(decode);
      if (rest.some((r) => r === undefined)) return undefined;
      params[seg.name] = rest.join("/");
      return params;
    }
    const part = parts[i];
    if (part === undefined) return undefined;
    if (seg.kind === "static") {
      if (part !== seg.value) return undefined;
    } else {
      const value = decode(part);
      if (value === undefined || value === "") return undefined;
      params[seg.name] = value;
    }
  }
  return parts.length === segs.length ? params : undefined;
}

/** Builds a path from a pattern; every value is percent-encoded (a splat keeps its slashes). */
export function buildPath(source: string, params: Params): string {
  const out = compile(source).segs.map((s) => {
    if (s.kind === "static") return s.value;
    const v = params[s.name] ?? "";
    return s.kind === "splat"
      ? v.split("/").map(encodeURIComponent).join("/")
      : encodeURIComponent(v);
  });
  const path = `/${out.join("/")}`;
  return path.replace(/\/+$/, "") || "/";
}
