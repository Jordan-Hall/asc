// A demo repository for the mock transport: a tree, a README, a very long file (to exercise the
// virtualiser), a binary file, and two stacks of changes. Used by tests and by `?mock`.
import { changeIdToString, toHex } from "@asc/asc-client";
import { AgentRunState, Agents } from "@asc/asc-client/agents";
import { Changes } from "@asc/asc-client/changes";
import { Ci, CiStatus } from "@asc/asc-client/ci";
import { CodeIntel } from "@asc/asc-client/codeintel";
import { Inbox, InboxSection } from "@asc/asc-client/inbox";
import { Repo } from "@asc/asc-client/repo";
import { Review, ThreadStatus } from "@asc/asc-client/review";
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
  /** Why the change exists (the description between the title and the trailers). */
  intent?: string;
  risk?: string;
  riskArea?: string;
  /** Agent run that wrote it, a `Agent:` trailer. */
  agent?: string;
  author?: string;
  /** Age of the newest version. */
  ageSec?: number;
  version?: number;
  added?: number;
  removed?: number;
  files?: number;
  /** The parent moved: the change needs a restack. */
  restack?: boolean;
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
const MIN = 60;
const HOUR = 3600;
const DAY = 86400;

interface InboxSeed {
  section: InboxSection;
  author: string;
  ageSec: number;
  threads: number;
  /** The stack the change belongs to, and its index (bottom is 0). Without one the item stands alone. */
  stack?: string;
  index?: number;
  /** The newest comment of the change, for returned items. */
  quote?: { author: string; body: string };
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

const RETRY = "refs/heads/retry-with-backoff";
const CURSOR = "refs/heads/cursor-pagination";
const RATE = "refs/heads/rate-limit-headers";
const CONTRAST = "refs/heads/contrast-tokens";
const FOCUS = "refs/heads/focus-ring";

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
      "Cargo.toml",
      ".asc/policy.toml",
      "src/client.rs",
      "src/refund.rs",
      "src/retry.rs",
      "tests/retry.rs",
      "docs/retries.md",
    ),
    subject: "Add idempotency keys to refunds",
    ageSec: 3 * HOUR,
    stacks: {
      [RETRY]: [
        {
          title: "Add retry policy type",
          state: "open",
          number: 301,
          approvals: 1,
          checks: "success",
          author: "Jordan",
          ageSec: 2 * DAY,
          version: 2,
          added: 38,
          removed: 2,
          files: 2,
          intent:
            "A RetryPolicy value says how many attempts, how long between them and how much jitter.",
        },
        {
          title: "Backoff with jitter",
          state: "open",
          number: 302,
          approvals: 1,
          checks: "success",
          author: "coder-1",
          agent: "coder-1",
          ageSec: DAY,
          version: 3,
          added: 52,
          removed: 4,
          files: 2,
          intent:
            "Delays double each attempt with random jitter so retries do not arrive together.",
        },
        {
          title: "Wire retry policy into the client",
          state: "open",
          number: 303,
          approvals: 0,
          checks: "success",
          author: "coder-2",
          agent: "coder-2",
          ageSec: 12 * MIN,
          version: 4,
          added: 84,
          removed: 12,
          files: 3,
          intent:
            "Every outbound payment call goes through the retry policy so a flaky provider cannot fail a charge. Refunds use the same policy.",
          risk: "touches 3 call sites in billing.",
          riskArea: "billing",
        },
        {
          title: "Docs and metrics",
          state: "open",
          number: 304,
          approvals: 0,
          checks: "pending",
          draft: true,
          author: "coder-2",
          agent: "coder-2",
          ageSec: 3 * HOUR,
          version: 1,
          added: 31,
          removed: 0,
          files: 2,
          restack: true,
          intent: "Document the retry policy and count retries.",
        },
      ],
    },
    inbox: [
      {
        section: InboxSection.NEEDS_REVIEW,
        author: "coder-2",
        ageSec: 12 * MIN,
        threads: 0,
        stack: RETRY,
        index: 2,
      },
    ],
  },
  "acme/web-app": {
    files: small("package.json", "tsconfig.json", "src/main.tsx", "src/pagination.ts"),
    subject: "Paginate the order list",
    ageSec: 26 * HOUR,
    stacks: {
      [CURSOR]: [
        {
          title: "Cursor type for the list API",
          state: "open",
          number: 88,
          approvals: 1,
          checks: "success",
          author: "Jordan",
          ageSec: 2 * DAY,
          added: 44,
          removed: 3,
          files: 2,
        },
        {
          title: "Pagination cursor",
          state: "open",
          number: 89,
          approvals: 0,
          checks: "success",
          author: "Jordan",
          ageSec: 41 * MIN,
          added: 120,
          removed: 40,
          files: 5,
          intent: "Lists page by an opaque cursor instead of an offset.",
        },
        {
          title: "Cursor in the order list",
          state: "open",
          number: 90,
          approvals: 0,
          checks: "",
          draft: true,
          author: "Jordan",
          ageSec: 3 * HOUR,
          added: 18,
          removed: 2,
          files: 1,
        },
      ],
    },
    inbox: [
      {
        section: InboxSection.RETURNED,
        author: "Jordan",
        ageSec: 41 * MIN,
        threads: 1,
        stack: CURSOR,
        index: 1,
        quote: { author: "Priya", body: "Can we keep the old cursor format for one release?" },
      },
    ],
  },
  "acme/infra": {
    files: small("main.tf", "modules/edge/main.tf", "modules/edge/limits.tf"),
    subject: "Rate limit headers on the edge",
    ageSec: 2 * HOUR,
    stacks: {
      [RATE]: [
        {
          title: "Rate limit headers",
          state: "open",
          number: 51,
          approvals: 2,
          checks: "success",
          author: "Jordan",
          ageSec: HOUR,
          added: 30,
          removed: 4,
          files: 2,
        },
        {
          title: "Infra rollout window",
          state: "open",
          number: 52,
          approvals: 2,
          checks: "success",
          author: "Jordan",
          ageSec: HOUR,
          added: 12,
          removed: 0,
          files: 1,
        },
      ],
    },
    inbox: [
      {
        section: InboxSection.RETURNED,
        author: "Jordan",
        ageSec: 3 * HOUR,
        threads: 1,
        stack: RATE,
        index: 1,
        quote: {
          author: "Grace",
          body: "@jordan can you move the infra rollout window to Thursday?",
        },
      },
    ],
  },
  "acme/design-system": {
    files: small("package.json", "tokens/color.json", "src/button.tsx"),
    subject: "Fix focus ring contrast",
    ageSec: 6 * HOUR,
    stacks: {
      [CONTRAST]: [
        {
          title: "Contrast tokens",
          state: "open",
          number: 12,
          approvals: 0,
          checks: "success",
          author: "linus",
          ageSec: DAY + HOUR,
          added: 22,
          removed: 22,
          files: 2,
        },
      ],
      [FOCUS]: [
        {
          title: "Button focus ring",
          state: "open",
          number: 13,
          approvals: 0,
          checks: "success",
          author: "linus",
          ageSec: 6 * HOUR,
          added: 14,
          removed: 3,
          files: 1,
        },
        {
          title: "Input focus ring",
          state: "open",
          number: 14,
          approvals: 0,
          checks: "success",
          author: "linus",
          ageSec: 90 * MIN,
          added: 9,
          removed: 2,
          files: 1,
        },
      ],
    },
    inbox: [
      {
        section: InboxSection.NEEDS_REVIEW,
        author: "linus",
        ageSec: DAY + HOUR,
        threads: 0,
        stack: CONTRAST,
        index: 0,
      },
      {
        section: InboxSection.NEEDS_REVIEW,
        author: "linus",
        ageSec: 6 * HOUR,
        threads: 0,
        stack: FOCUS,
        index: 0,
      },
    ],
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

/** The commit message of a demo change: title, intent, then the trailers. */
function messageOf(c: DemoChange): string {
  const parts = [c.title];
  if (c.intent) parts.push("", c.intent);
  const trailers = [
    c.risk ? `Risk: ${c.risk}` : "",
    c.riskArea ? `Risk-area: ${c.riskArea}` : "",
    c.agent ? `Agent: ${c.agent}` : "",
  ].filter(Boolean);
  if (trailers.length > 0) parts.push("", ...trailers);
  return parts.join("\n");
}

/** The per-file stat of a demo change, split over its files. */
function statOf(c: DemoChange) {
  const n = c.files ?? 0;
  if (n === 0) return undefined;
  const files = Array.from({ length: n }, (_, i) => ({
    path: enc.encode(`src/file${i + 1}.rs`),
    added: Math.floor((c.added ?? 0) / n) + (i === 0 ? (c.added ?? 0) % n : 0),
    removed: Math.floor((c.removed ?? 0) / n) + (i === 0 ? (c.removed ?? 0) % n : 0),
    binary: false,
  }));
  return { files };
}

/** The change id an inbox item points at: a stack change when it names one, else its own. */
function inboxChange(name: string, s: InboxSeed, i: number): Uint8Array {
  return s.stack !== undefined
    ? changeIdBytes(s.stack, s.index ?? 0)
    : changeIdBytes(`inbox:${name}`, i);
}

const SUBJECTS = [
  "Wire the retry policy into the client",
  "Cache merge bases per generation",
  "Tighten the sidebar spacing",
  "Add the token store",
  "Fix the empty README heading",
  "Initial commit",
];

/** The demo history of a repository, newest first; the first entry is the tip. */
function historyOf(name: string | undefined) {
  const prof = profile(name);
  const repo = name ?? DEMO_REPO;
  return SUBJECTS.map((subject, i) => ({
    id: fakeId(`${repo}:history:${i}`),
    parents: i < SUBJECTS.length - 1 ? [fakeId(`${repo}:history:${i + 1}`)] : [],
    message: enc.encode(i === 0 ? `${prof.subject}\n\nBody.` : `${subject}\n\nWhy: demo history.`),
    author: signature(i % 2 === 0 ? "Ada" : "Linus", prof.ageSec + i * 2 * DAY),
  }));
}

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
      if (req.recursive) seen.set(rest, false);
      else seen.set(slash < 0 ? rest : rest.slice(0, slash), slash >= 0);
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
    const history = historyOf(req.repo?.name);
    if (/^[0-9a-f]{6,}$/.test(include)) {
      // A commit id: the demo history knows its own.
      const found = history.find((c) => toHex(c.id).startsWith(include));
      if (found) yield { entries: [found] };
      return;
    }
    const stack = prof.stacks[include];
    if (stack && req.exclude.length > 0) {
      yield {
        entries: stack.map((c, i) => ({
          id: fakeId(`${include}@${i}`),
          message: enc.encode(messageOf(c)),
          author: signature(c.author ?? "Ada", c.ageSec ?? prof.ageSec + i * 600),
          change: changeIdBytes(include, i),
          stats: statOf(c),
        })),
      };
      return;
    }
    if (req.limit > 1) {
      yield { entries: history.slice(0, req.limit) };
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

  t.onStream(Repo.method.diff, function* (req) {
    yield {
      changes: [
        { path: enc.encode("src/index.ts"), status: "M" },
        { path: enc.encode("docs/guide.md"), status: "A" },
      ],
      text: enc.encode(
        [
          "diff --git a/src/index.ts b/src/index.ts",
          "--- a/src/index.ts",
          "+++ b/src/index.ts",
          "@@ -1,2 +1,3 @@",
          " export const hello = (name: string) => `hello ${name}`;",
          "-export { hello };",
          "+export { hello };",
          "+export const bye = (name: string) => `bye ${name}`;",
          "diff --git a/docs/guide.md b/docs/guide.md",
          "new file mode 100644",
          "--- /dev/null",
          "+++ b/docs/guide.md",
          "@@ -0,0 +1 @@",
          `+Changed in ${req.head.slice(0, 10)}`,
          "",
        ].join("\n"),
      ),
    };
  });

  t.onStream(Repo.method.blame, function* (req) {
    const body = filesOf(req.repo?.name).get(new TextDecoder().decode(req.path));
    if (!body) throw notFound("file");
    const lines = new TextDecoder().decode(body).split("\n").length;
    const history = historyOf(req.repo?.name);
    const third = Math.max(1, Math.floor(lines / 3));
    const cuts = [1, 1 + third, 1 + 2 * third].filter((c) => c <= lines);
    yield {
      commits: history.slice(0, cuts.length),
      hunks: cuts.map((start, i) => ({
        origin: history[i]?.id ?? new Uint8Array(),
        start,
        len: (cuts[i + 1] ?? lines + 1) - start,
      })),
    };
  });

  t.onStream(CodeIntel.method.search, function* (req) {
    const files = filesOf(req.scope?.repo?.name);
    const want = req.pattern.toLowerCase();
    const dec = new TextDecoder();
    const hits = [];
    for (const [path, body] of files) {
      if (path.includes("big.txt") || body.includes(0)) continue;
      const matches = dec
        .decode(body)
        .split("\n")
        .flatMap((text, i) =>
          text.toLowerCase().includes(want) ? [{ line: i + 1, text: enc.encode(text) }] : [],
        );
      if (matches.length > 0) hits.push({ path: enc.encode(path), lang: "", matches });
    }
    yield { files: hits.slice(0, 100), truncated: false };
  });

  t.onUnary(Changes.method.versions, (req) => {
    const id = toHex(req.change);
    return {
      versions: [2, 1].map((v) => ({
        v,
        cs: fakeId(`cs:${id}:${v}`),
        at: BigInt(NOW() - v * 3 * HOUR),
        actor: v === 2 ? "Ada" : "Linus",
      })),
    };
  });

  t.onUnary(Repo.method.readChangeset, () => ({
    changes: [
      { path: enc.encode("src/index.ts"), status: "M" },
      { path: enc.encode("src/token.ts"), status: "A" },
      { path: enc.encode("src/old.ts"), status: "D" },
    ],
  }));

  t.onUnary(Ci.method.listRuns, () => ({
    runs: [
      {
        id: 3n,
        name: "build",
        status: CiStatus.RUNNING,
        created: BigInt(NOW() - 4 * MIN),
        jobs: [
          { id: 31n, name: "compile", status: CiStatus.SUCCESS, logSize: 120n },
          { id: 32n, name: "test", status: CiStatus.RUNNING, logSize: 64n },
        ],
      },
      {
        id: 2n,
        name: "build",
        status: CiStatus.FAILURE,
        created: BigInt(NOW() - 3 * HOUR),
        version: 1,
        change: changeIdBytes("refs/heads/feature-cache", 0),
        jobs: [{ id: 21n, name: "test", status: CiStatus.FAILURE, logSize: 90n }],
      },
      {
        id: 1n,
        name: "lint",
        status: CiStatus.SUCCESS,
        created: BigInt(NOW() - DAY),
        jobs: [{ id: 11n, name: "oxlint", status: CiStatus.SUCCESS, logSize: 30n }],
      },
    ],
  }));

  t.onUnary(Ci.method.jobLog, (req) => {
    const data = enc.encode(
      req.jobId === 21n
        ? "running 3 tests\ntest dedupe_reads ... FAILED\n\nfailures:\n    dedupe_reads: left 2, right 1\n"
        : "step 1: fetch\nstep 2: build\nstep 3: done\n",
    );
    return { data, size: BigInt(data.length), offset: 0n, nextOffset: BigInt(data.length) };
  });

  t.onUnary(Repo.method.repoConfig, () => ({
    visibility: "private",
    branchless: true,
    cloud: true,
    sharedRefs: [],
    autolinks: [{ prefix: "ASC-", url: "https://tracker.example.org/browse/ASC-" }],
  }));

  t.onUnary(Repo.method.getPolicy, () => ({
    present: true,
    version: 1,
    mergeStrategy: "rebase",
    trunk: { branch: "main", requireApprovals: 1, requireChecks: ["build"], allowForce: false },
    roles: [{ name: "maintainers", members: ["ada", "linus"] }],
    paths: [{ glob: "docs/**", write: ["maintainers"] }],
  }));

  t.onUnary(Agents.method.listRuns, (req) => ({
    runs:
      req.repo?.name === DEMO_REPO
        ? [
            {
              id: 7n,
              repo: DEMO_REPO,
              agent: "assistant",
              title: "Add retry policy",
              state: AgentRunState.SUCCEEDED,
              target: "main",
              createdBy: "ada",
              created: BigInt(NOW() - 2 * HOUR),
              changes: [changeIdBytes("refs/heads/feature-auth", 0)],
            },
          ]
        : [],
  }));

  t.onUnary(Changes.method.stack, (req) => {
    const ref = req.at.case === "rev" ? req.at.value : "";
    const stack = profile(req.repo?.name).stacks[ref];
    if (!stack) return { trunk: req.target, changes: [] };
    return {
      trunk: req.target,
      changes: stack.map((c, i) => ({
        change: changeIdBytes(ref, i),
        head: fakeId(`${ref}@${i}`),
        version: c.version ?? 1 + i,
        parentChange: i > 0 ? changeIdBytes(ref, i - 1) : new Uint8Array(),
        parentCurrent: !c.restack,
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
        change: inboxChange(name, s, i),
        section: s.section,
        latestVersion: 1 + (i % 3),
        author: s.author,
        updated: BigInt(NOW() - s.ageSec),
        openThreads: s.threads,
        requestedBy: s.section === InboxSection.NEEDS_REVIEW ? s.author : "",
      })),
    };
  });

  t.onUnary(Review.method.listThreads, (req) => {
    const name = req.repo?.name ?? DEMO_REPO;
    const seeds = profile(name).inbox;
    const id = changeIdToString(req.change);
    const seed = seeds.find((s, i) => changeIdToString(inboxChange(name, s, i)) === id);
    if (!seed?.quote) return { threads: [], openThreads: 0, nextPageToken: "" };
    return {
      openThreads: 1,
      nextPageToken: "",
      threads: [
        {
          id: 1n,
          change: req.change,
          status: ThreadStatus.OPEN,
          openedBy: seed.quote.author,
          created: BigInt(NOW() - seed.ageSec - 60),
          updated: BigInt(NOW() - seed.ageSec),
          comments: [
            {
              id: 1n,
              author: seed.quote.author,
              body: seed.quote.body,
              created: BigInt(NOW() - seed.ageSec),
            },
          ],
        },
      ],
    };
  });

  return t;
}
