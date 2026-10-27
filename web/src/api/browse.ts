// Reads behind history, commit, blame, search, Actions, change review and repository settings.
// Like repo.ts this is one of the few modules that know service and method names.
import {
  type BridgeTransport,
  callStream,
  callUnary,
  type CallParams,
  changeIdFromString,
  toHex,
} from "@asc/asc-client";
import { Agents, type AgentRunState } from "@asc/asc-client/agents";
import { Changes } from "@asc/asc-client/changes";
import { Ci } from "@asc/asc-client/ci";
import { CodeIntel, SearchMode } from "@asc/asc-client/codeintel";
import { Repo } from "@asc/asc-client/repo";
import { shortId } from "./repo";

const text = new TextDecoder();
const enc = new TextEncoder();

export interface HistoryRow {
  id: string;
  short: string;
  subject: string;
  author: string;
  /** Seconds since the epoch. */
  time: number;
  /** Change id in k-z form, empty when the commit has none. */
  change: string;
}

export interface HistoryView {
  rows: readonly HistoryRow[];
  /** More commits exist beyond `limit`. */
  more: boolean;
}

export async function loadHistory(
  t: BridgeTransport,
  repo: string,
  rev: string,
  path: string,
  limit: number,
  p?: CallParams,
): Promise<HistoryView> {
  const rows: HistoryRow[] = [];
  const stream = callStream(
    t,
    Repo.method.log,
    { repo: { name: repo }, include: [rev], paths: path ? [path] : [], limit: limit + 1 },
    p,
  );
  for await (const batch of stream) {
    for (const e of batch.entries) {
      const id = toHex(e.id);
      rows.push({
        id,
        short: shortId(id),
        subject: text.decode(e.message).split("\n", 1)[0] ?? "",
        author: text.decode(e.author?.name ?? new Uint8Array()),
        time: Number(e.author?.time ?? 0n),
        change: e.change.length > 0 ? toHex(e.change) : "",
      });
    }
  }
  return { rows: rows.slice(0, limit), more: rows.length > limit };
}

export interface FileChange {
  path: string;
  /** Git status letter or word: A, M, D, R. */
  status: string;
  oldPath: string;
}

export interface CommitDetail {
  id: string;
  short: string;
  subject: string;
  body: string;
  author: string;
  time: number;
  parents: readonly string[];
  files: readonly FileChange[];
  /** The patch in git format; cut off by the server when `truncated`. */
  patch: string;
  truncated: boolean;
}

export async function loadCommit(
  t: BridgeTransport,
  repo: string,
  rev: string,
  p?: CallParams,
): Promise<CommitDetail | undefined> {
  const log = callStream(t, Repo.method.log, { repo: { name: repo }, include: [rev], limit: 1 }, p);
  const head = await firstEntry(log);
  if (!head) return undefined;
  const id = toHex(head.id);
  const files: FileChange[] = [];
  let patch = "";
  let truncated = false;
  const diff = callStream(
    t,
    Repo.method.diff,
    { repo: { name: repo }, base: `${id}^`, head: id, format: 4, renames: true, context: 3 },
    p,
  );
  for await (const d of diff) {
    for (const c of d.changes) {
      files.push({
        path: text.decode(c.path),
        status: c.status,
        oldPath: text.decode(c.oldPath),
      });
    }
    patch += text.decode(d.text);
    truncated = truncated || d.truncated;
  }
  const message = text.decode(head.message);
  const nl = message.indexOf("\n");
  return {
    id,
    short: shortId(id),
    subject: nl < 0 ? message : message.slice(0, nl),
    body: nl < 0 ? "" : message.slice(nl + 1).trim(),
    author: text.decode(head.author?.name ?? new Uint8Array()),
    time: Number(head.author?.time ?? 0n),
    parents: head.parents.map(toHex),
    files,
    patch,
    truncated,
  };
}

async function firstEntry<E>(
  stream: AsyncIterable<{ entries: readonly E[] }>,
): Promise<E | undefined> {
  for await (const batch of stream) {
    const e = batch.entries[0];
    if (e) return e;
  }
  return undefined;
}

export interface BlameGroup {
  /** 1-based first line of the hunk in the file at the blamed revision. */
  start: number;
  len: number;
  id: string;
  short: string;
  author: string;
  time: number;
  subject: string;
}

export async function loadBlame(
  t: BridgeTransport,
  repo: string,
  rev: string,
  path: string,
  p?: CallParams,
): Promise<BlameGroup[]> {
  const commits = new Map<string, { author: string; time: number; subject: string }>();
  const hunks: { origin: string; start: number; len: number }[] = [];
  const stream = callStream(
    t,
    Repo.method.blame,
    { repo: { name: repo }, rev, path: enc.encode(path) },
    p,
  );
  for await (const b of stream) {
    for (const c of b.commits) {
      commits.set(toHex(c.id), {
        author: text.decode(c.author?.name ?? new Uint8Array()),
        time: Number(c.author?.time ?? 0n),
        subject: text.decode(c.message).split("\n", 1)[0] ?? "",
      });
    }
    for (const h of b.hunks) hunks.push({ origin: toHex(h.origin), start: h.start, len: h.len });
  }
  return hunks
    .toSorted((a, b) => a.start - b.start)
    .map((h) => {
      const c = commits.get(h.origin);
      return {
        start: h.start,
        len: h.len,
        id: h.origin,
        short: shortId(h.origin),
        author: c?.author ?? "",
        time: c?.time ?? 0,
        subject: c?.subject ?? "",
      };
    });
}

export interface SearchLine {
  line: number;
  text: string;
}

export interface SearchFile {
  path: string;
  lang: string;
  matches: readonly SearchLine[];
}

export interface SearchView {
  files: readonly SearchFile[];
  truncated: boolean;
}

export type SearchKind = "literal" | "regex" | "word";

const MODES: Record<SearchKind, SearchMode> = {
  literal: SearchMode.LITERAL,
  regex: SearchMode.REGEX,
  word: SearchMode.WORD,
};

export async function searchCode(
  t: BridgeTransport,
  repo: string,
  rev: string,
  pattern: string,
  kind: SearchKind,
  p?: CallParams,
): Promise<SearchView> {
  const files: SearchFile[] = [];
  let truncated = false;
  const stream = callStream(
    t,
    CodeIntel.method.search,
    {
      scope: { repo: { name: repo }, at: { case: "rev", value: rev } },
      pattern,
      mode: MODES[kind],
      budget: { maxResults: 100 },
    },
    p,
  );
  for await (const r of stream) {
    for (const f of r.files) {
      files.push({
        path: text.decode(f.path),
        lang: f.lang,
        matches: f.matches.map((m) => ({ line: m.line, text: text.decode(m.text) })),
      });
    }
    truncated = truncated || r.truncated;
  }
  return { files, truncated };
}

/** Paths of the whole tree at `rev` that contain `needle` (case-insensitive), for the file finder. */
export async function findFiles(
  t: BridgeTransport,
  repo: string,
  rev: string,
  needle: string,
  p?: CallParams,
): Promise<{ paths: readonly string[]; truncated: boolean }> {
  const want = needle.toLowerCase();
  const paths: string[] = [];
  const stream = callStream(
    t,
    Repo.method.readTree,
    { repo: { name: repo }, rev, path: new Uint8Array(), recursive: true, limit: 0 },
    p,
  );
  for await (const b of stream) {
    for (const e of b.entries) {
      if ((e.mode & 0o170000) === 0o040000) continue;
      const path = text.decode(e.path);
      if (path.toLowerCase().includes(want)) paths.push(path);
    }
  }
  return { paths: paths.slice(0, 200), truncated: paths.length > 200 };
}

export interface VersionRow {
  v: number;
  actor: string;
  /** Seconds since the epoch. */
  at: number;
  /** The version's changeset id in hex. */
  cs: string;
}

export interface ChangeFiles {
  versions: readonly VersionRow[];
  /** Files of the newest version. */
  files: readonly FileChange[];
}

export async function loadChangeFiles(
  t: BridgeTransport,
  repo: string,
  change: string,
  p?: CallParams,
): Promise<ChangeFiles> {
  const res = await callUnary(
    t,
    Changes.method.versions,
    { repo: { name: repo }, change: changeIdFromString(change) },
    p,
  );
  const versions = res.versions
    .map((v) => ({ v: v.v, actor: v.actor, at: Number(v.at), cs: toHex(v.cs) }))
    .toSorted((a, b) => b.v - a.v);
  const newest = versions[0];
  if (!newest) return { versions, files: [] };
  const cs = await callUnary(
    t,
    Repo.method.readChangeset,
    { repo: { name: repo }, rev: newest.cs },
    p,
  );
  return {
    versions,
    files: cs.changes.map((c) => ({
      path: text.decode(c.path),
      status: c.status,
      oldPath: text.decode(c.oldPath),
    })),
  };
}

export interface JobLogView {
  text: string;
  size: number;
  /** Bytes before the shown part exist. */
  truncated: boolean;
}

export async function loadJobLog(
  t: BridgeTransport,
  repo: string,
  run: number,
  job: number,
  p?: CallParams,
): Promise<JobLogView> {
  const res = await callUnary(
    t,
    Ci.method.jobLog,
    { repo: { name: repo }, runId: BigInt(run), jobId: BigInt(job), maxBytes: 64 * 1024 },
    p,
  );
  return { text: text.decode(res.data), size: Number(res.size), truncated: res.truncated };
}

export interface RepoSettingsView {
  visibility: string;
  branchless: boolean;
  cloud: boolean;
  sharedRefs: readonly string[];
  autolinks: readonly { prefix: string; url: string }[];
  policy:
    | {
        present: boolean;
        error: string;
        mergeStrategy: string;
        trunk: string;
        approvals: number;
        checks: readonly string[];
        allowForce: boolean;
        roles: readonly { name: string; members: readonly string[] }[];
        paths: readonly { glob: string; write: readonly string[] }[];
      }
    | undefined;
}

export async function loadRepoSettings(
  t: BridgeTransport,
  repo: string,
  rev: string,
  p?: CallParams,
): Promise<RepoSettingsView> {
  const [cfg, pol] = await Promise.all([
    callUnary(t, Repo.method.repoConfig, { repo: { name: repo }, rev }, p),
    callUnary(t, Repo.method.getPolicy, { repo: { name: repo }, paths: [] }, p),
  ]);
  return {
    visibility: cfg.visibility,
    branchless: cfg.branchless,
    cloud: cfg.cloud,
    sharedRefs: cfg.sharedRefs,
    autolinks: cfg.autolinks.map((a) => ({ prefix: a.prefix, url: a.url })),
    policy: pol.present
      ? {
          present: true,
          error: pol.error,
          mergeStrategy: pol.mergeStrategy,
          trunk: pol.trunk?.branch ?? "",
          approvals: pol.trunk?.requireApprovals ?? 0,
          checks: pol.trunk?.requireChecks ?? [],
          allowForce: pol.trunk?.allowForce ?? false,
          roles: pol.roles.map((r) => ({ name: r.name, members: r.members })),
          paths: pol.paths.map((r) => ({ glob: r.glob, write: r.write })),
        }
      : undefined,
  };
}

export interface AgentRunView {
  id: number;
  repo: string;
  agent: string;
  title: string;
  state: AgentRunState;
  target: string;
  createdBy: string;
  /** Seconds since the epoch. */
  created: number;
  changes: number;
  failure: string;
}

export async function loadAgentRuns(
  t: BridgeTransport,
  repo: string,
  p?: CallParams,
): Promise<AgentRunView[]> {
  const res = await callUnary(t, Agents.method.listRuns, { repo: { name: repo }, limit: 30 }, p);
  return res.runs.map((r) => ({
    id: Number(r.id),
    repo: r.repo || repo,
    agent: r.agent,
    title: r.title,
    state: r.state,
    target: r.target,
    createdBy: r.createdBy,
    created: Number(r.created),
    changes: r.changes.length,
    failure: r.failureMessage,
  }));
}
