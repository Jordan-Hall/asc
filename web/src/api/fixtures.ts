// A demo repository for the mock transport: a tree, a README, a very long file (to exercise the
// virtualiser), a binary file, and two stacks of changes. Used by tests and by `?mock`.
import { Changes } from "@asc/asc-client/changes";
import { Inbox, InboxSection } from "@asc/asc-client/inbox";
import { Repo } from "@asc/asc-client/repo";
import { MockTransport, notFound } from "./mock";

export const DEMO_REPO = "asc/demo";
const enc = new TextEncoder();

const README = `# Demo

A small repository served by the **mock transport**.

## Layout

- \`src/\` the sources
- \`docs/\` guides

\`\`\`ts
export const hello = (name: string) => \`hello \${name}\`;
\`\`\`

See [the guide](docs/guide.md).
`;

function bigFile(lines: number): string {
  const out: string[] = [];
  for (let i = 1; i <= lines; i++) out.push(`line ${i}: ${"abcdefghij".repeat((i % 7) + 1)}`);
  return `${out.join("\n")}\n`;
}

/** path -> bytes. */
export function demoFiles(): Map<string, Uint8Array> {
  const files = new Map<string, Uint8Array>();
  const put = (p: string, body: string | Uint8Array) =>
    files.set(p, typeof body === "string" ? enc.encode(body) : body);
  put("README.md", README);
  put("package.json", '{\n  "name": "demo",\n  "private": true\n}\n');
  put("src/index.ts", 'export { hello } from "./lib/hello";\n');
  put("src/lib/hello.ts", "export const hello = (name: string) => `hello ${name}`;\n");
  put("src/lib/util.ts", "export const noop = () => undefined;\n");
  put("docs/guide.md", "# Guide\n\nRead the code.\n");
  put("data/big.txt", bigFile(50_000));
  put("assets/logo.bin", new Uint8Array([0x89, 0x50, 0x00, 0x01, 0x02]));
  return files;
}

/** A stable fake id of `len` bytes for a name. */
export function fakeId(name: string, len = 20): Uint8Array {
  let h = 2166136261;
  const out = new Uint8Array(len);
  for (let i = 0; i < len; i++) {
    for (let j = 0; j < name.length; j++) h = Math.imul(h ^ name.charCodeAt(j), 16777619) >>> 0;
    h = Math.imul(h ^ (i + 1), 16777619) >>> 0;
    out[i] = h & 0xff;
  }
  return out;
}

interface DemoChange {
  title: string;
  state: string;
  number: number;
  approvals: number;
  checks: string;
  draft?: boolean;
}

const STACKS: Record<string, readonly DemoChange[]> = {
  "refs/heads/feature-auth": [
    { title: "auth: add token store", state: "open", number: 41, approvals: 2, checks: "success" },
    { title: "auth: sign-in screen", state: "open", number: 42, approvals: 0, checks: "pending" },
    {
      title: "auth: remember for this tab",
      state: "open",
      number: 43,
      approvals: 0,
      checks: "",
      draft: true,
    },
  ],
  "refs/heads/feature-cache": [
    {
      title: "cache: dedupe in-flight reads",
      state: "open",
      number: 44,
      approvals: 1,
      checks: "failure",
    },
  ],
};

export function changeIdBytes(ref: string, i: number): Uint8Array {
  return fakeId(`${ref}#${i}`, 16);
}

const MODE_DIR = 0o040000;
const MODE_FILE = 0o100644;
const NOW = () => Math.floor(Date.now() / 1000);

interface InboxSeed {
  section: InboxSection;
  author: string;
  ageSec: number;
  threads: number;
}

/** What one demo repository looks like: its files, last commit, stacks and inbox. */
interface Profile {
  files: () => Map<string, Uint8Array>;
  subject: string;
  ageSec: number;
  stacks: Record<string, readonly DemoChange[]>;
  inbox: readonly InboxSeed[];
}

/** A small repository: a README plus the named files, each with a line of content. */
function small(...paths: string[]): () => Map<string, Uint8Array> {
  return () => {
    const files = new Map<string, Uint8Array>();
    files.set("README.md", enc.encode("# Readme\n\nA demo repository.\n"));
    for (const p of paths) files.set(p, enc.encode(`${p}\n`));
    return files;
  };
}

const PROFILES: Record<string, Profile> = {
  [DEMO_REPO]: {
    files: demoFiles,
    subject: "Initial import",
    ageSec: 2 * 3600,
    stacks: STACKS,
    inbox: [
      { section: InboxSection.NEEDS_REVIEW, author: "ada", ageSec: 1800, threads: 0 },
      { section: InboxSection.NEEDS_REVIEW, author: "grace", ageSec: 5 * 3600, threads: 0 },
      { section: InboxSection.RETURNED, author: "grace", ageSec: 26 * 3600, threads: 2 },
      { section: InboxSection.WAITING, author: "linus", ageSec: 3 * 86400, threads: 1 },
    ],
  },
  "acme/payments": {
    files: small(
      "go.mod",
      "cmd/server/main.go",
      "internal/billing/retry.go",
      "internal/billing/charge.go",
    ),
    subject: "billing: retry failed charges with backoff",
    ageSec: 35 * 60,
    stacks: {
      "refs/heads/billing-retry": [
        {
          title: "billing: extract charge client",
          state: "open",
          number: 212,
          approvals: 2,
          checks: "success",
        },
        {
          title: "billing: retry failed charges",
          state: "open",
          number: 213,
          approvals: 1,
          checks: "pending",
        },
      ],
    },
    inbox: [{ section: InboxSection.CHECKS_FAILED, author: "you", ageSec: 3 * 3600, threads: 0 }],
  },
  "acme/web": {
    files: small("package.json", "tsconfig.json", "src/main.tsx", "src/app.tsx"),
    subject: "ui: tighten the sidebar spacing",
    ageSec: 26 * 3600,
    stacks: {},
    inbox: [],
  },
  "acme/ledger": {
    files: small("pyproject.toml", "ledger/__init__.py", "ledger/batch.py"),
    subject: "batch inserts for the nightly import",
    ageSec: 9 * 86400,
    stacks: {
      "refs/heads/batch-inserts": [
        {
          title: "ledger: batch inserts",
          state: "open",
          number: 88,
          approvals: 0,
          checks: "failure",
        },
      ],
    },
    inbox: [],
  },
  "asc/engine": {
    files: small("Cargo.toml", "src/lib.rs", "src/graph.rs", "src/merge.rs"),
    subject: "graph: cache merge bases per generation",
    ageSec: 4 * 86400,
    stacks: {},
    inbox: [],
  },
};

/** The names of the demo repositories, for tests and the dashboard screenshots. */
export const DEMO_REPOS: readonly string[] = Object.keys(PROFILES);

const profile = (name: string | undefined): Profile =>
  PROFILES[name ?? ""] ?? (PROFILES[DEMO_REPO] as Profile);

const refsOf = (p: Profile) => ["refs/heads/main", ...Object.keys(p.stacks), "refs/tags/v0.1.0"];

const signature = (name: string, ageSec: number) => ({
  name: enc.encode(name),
  email: enc.encode("a@b.c"),
  time: BigInt(NOW() - ageSec),
});

/** A transport wired with the demo repositories. */
export function demoTransport(latencyMs = 0): MockTransport {
  const t = new MockTransport(latencyMs);
  const filesCache = new Map<string, Map<string, Uint8Array>>();
  const filesOf = (name: string | undefined) => {
    const key = PROFILES[name ?? ""] ? (name as string) : DEMO_REPO;
    let f = filesCache.get(key);
    if (!f) filesCache.set(key, (f = profile(key).files()));
    return f;
  };

  t.onUnary(Repo.method.listRefs, (req) => ({
    refs: refsOf(profile(req.repo?.name)).map((name) => ({
      name,
      target: { t: { case: "changeset" as const, value: fakeId(name) } },
      version: 1n,
    })),
    head: "refs/heads/main",
  }));

  t.onUnary(Repo.method.resolve, (req) => {
    const refs = refsOf(profile(req.repo?.name));
    return {
      ids: req.revspecs.map((r) => {
        const name = refs.find((n) => n === r || n.endsWith(`/${r}`));
        return name ? fakeId(name) : r === "HEAD" ? fakeId("refs/heads/main") : new Uint8Array();
      }),
    };
  });

  t.onStream(Repo.method.readTree, function* (req) {
    const files = filesOf(req.repo?.name);
    const dir = new TextDecoder().decode(req.path).replace(/\/$/, "");
    const prefix = dir ? `${dir}/` : "";
    const seen = new Map<string, boolean>();
    for (const p of files.keys()) {
      if (!p.startsWith(prefix)) continue;
      const rest = p.slice(prefix.length);
      const slash = rest.indexOf("/");
      seen.set(slash < 0 ? rest : rest.slice(0, slash), slash >= 0);
    }
    if (seen.size === 0 && dir) throw notFound(dir);
    const entries = [...seen].map(([name, isDir]) => ({
      name: enc.encode(name),
      path: enc.encode(prefix + name),
      mode: isDir ? MODE_DIR : MODE_FILE,
      size: BigInt(isDir ? 0 : (files.get(prefix + name)?.length ?? 0)),
      id: fakeId(prefix + name),
    }));
    for (let i = 0; i < entries.length || i === 0; i += 50)
      yield { entries: entries.slice(i, i + 50) };
  });

  t.onStream(Repo.method.readFile, function* (req) {
    const files = filesOf(req.repo?.name);
    const spec = req.at.case === "revPath" ? req.at.value : "";
    const path = spec.slice(spec.indexOf(":") + 1);
    const body = files.get(path);
    if (!body) throw notFound(path);
    const CHUNK = 16 * 1024;
    for (let off = 0; off < Math.max(body.length, 1); off += CHUNK) {
      yield { data: body.slice(off, off + CHUNK), totalSize: BigInt(body.length) };
    }
  });

  t.onStream(Repo.method.log, function* (req) {
    const prof = profile(req.repo?.name);
    const include = req.include[0] ?? "refs/heads/main";
    const stack = prof.stacks[include];
    if (stack && req.exclude.length > 0) {
      yield {
        entries: stack.map((c, i) => ({
          id: fakeId(`${include}@${i}`),
          message: enc.encode(`${c.title}\n\nBody text.`),
          author: signature("Ada", prof.ageSec + i * 600),
          change: changeIdBytes(include, i),
        })),
      };
      return;
    }
    yield {
      entries: [
        {
          id: fakeId(include),
          message: enc.encode(`${prof.subject}\n\nBody.`),
          author: signature("Ada", prof.ageSec),
        },
      ],
    };
  });

  t.onUnary(Changes.method.stack, (req) => {
    const ref = req.at.case === "rev" ? req.at.value : "";
    const stack = profile(req.repo?.name).stacks[ref];
    if (!stack) return { trunk: req.target, changes: [] };
    return {
      trunk: req.target,
      changes: stack.map((c, i) => ({
        change: changeIdBytes(ref, i),
        head: fakeId(`${ref}@${i}`),
        version: 1 + i,
        parentChange: i > 0 ? changeIdBytes(ref, i - 1) : new Uint8Array(),
        parentCurrent: true,
        review: {
          state: c.state,
          number: BigInt(c.number),
          draft: c.draft ?? false,
          approvals: c.approvals,
          checks: c.checks,
          url: "",
        },
      })),
    };
  });

  t.onUnary(Inbox.method.count, (req) => {
    const seeds = profile(req.repo?.name).inbox;
    const counts = new Map<InboxSection, number>();
    for (const s of seeds) counts.set(s.section, (counts.get(s.section) ?? 0) + 1);
    return {
      total: seeds.length,
      etag: `demo-${seeds.length}`,
      counts: [...counts].map(([section, count]) => ({ section, count })),
    };
  });

  t.onUnary(Inbox.method.list, (req) => {
    const name = req.repo?.name ?? DEMO_REPO;
    const seeds = profile(name).inbox;
    return {
      total: seeds.length,
      items: seeds.map((s, i) => ({
        change: changeIdBytes(`inbox:${name}`, i),
        section: s.section,
        latestVersion: 1 + (i % 3),
        author: s.author,
        updated: BigInt(NOW() - s.ageSec),
        openThreads: s.threads,
        requestedBy: s.section === InboxSection.NEEDS_REVIEW ? s.author : "",
      })),
    };
  });

  return t;
}
