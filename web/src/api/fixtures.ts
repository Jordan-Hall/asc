// A demo repository for the mock transport: a tree, a README, a very long file (to exercise the
// virtualiser), a binary file, and two stacks of changes. Used by tests and by `?mock`.
import { Changes } from "@asc/asc-client/changes";
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

const REFS = [
  "refs/heads/main",
  "refs/heads/feature-auth",
  "refs/heads/feature-cache",
  "refs/tags/v0.1.0",
];
const MODE_DIR = 0o040000;
const MODE_FILE = 0o100644;

const signature = (name: string) => ({
  name: enc.encode(name),
  email: enc.encode("a@b.c"),
  time: 1_760_000_000n,
});

/** A transport wired with the demo repository. */
export function demoTransport(latencyMs = 0): MockTransport {
  const files = demoFiles();
  const t = new MockTransport(latencyMs);

  t.onUnary(Repo.method.listRefs, () => ({
    refs: REFS.map((name) => ({
      name,
      target: { t: { case: "changeset" as const, value: fakeId(name) } },
      version: 1n,
    })),
    head: "refs/heads/main",
  }));

  t.onUnary(Repo.method.resolve, (req) => ({
    ids: req.revspecs.map((r) => {
      const name = REFS.find((n) => n === r || n.endsWith(`/${r}`));
      return name ? fakeId(name) : r === "HEAD" ? fakeId("refs/heads/main") : new Uint8Array();
    }),
  }));

  t.onStream(Repo.method.readTree, function* (req) {
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
    const include = req.include[0] ?? "refs/heads/main";
    const stack = STACKS[include];
    if (stack && req.exclude.length > 0) {
      yield {
        entries: stack.map((c, i) => ({
          id: fakeId(`${include}@${i}`),
          message: enc.encode(`${c.title}\n\nBody text.`),
          author: signature("Ada"),
          change: changeIdBytes(include, i),
        })),
      };
      return;
    }
    yield {
      entries: [
        {
          id: fakeId(include),
          message: enc.encode("Initial import\n\nBody."),
          author: signature("Ada"),
        },
      ],
    };
  });

  t.onUnary(Changes.method.stack, (req) => {
    const ref = req.at.case === "rev" ? req.at.value : "";
    const stack = STACKS[ref];
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

  return t;
}
