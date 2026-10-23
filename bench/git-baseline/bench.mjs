#!/usr/bin/env node
// Git baseline benchmark harness (BENCH0). See README.md.
import { spawnSync } from 'node:child_process';
import { appendFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { OPS, SIZES, fastImportStream, markdown, parseArgs, stats } from './lib.mjs';

const log = (m) => process.stderr.write(`${m}
`);
const TIMEOUT_MS = 600_000;
const SEED_DEPTH = 4;
const MAX_BUFFER = 64 * 1024 * 1024;

function makeRunner(home, exe, soft = false) {
  const empty = join(home, 'empty.gitconfig');
  writeFileSync(empty, '');
  const env = {
    ...process.env,
    GIT_CONFIG_NOSYSTEM: '1',
    GIT_CONFIG_GLOBAL: empty,
    GIT_TERMINAL_PROMPT: '0',
    GIT_PAGER: 'cat',
    GIT_EDITOR: 'true',
    GIT_AUTHOR_NAME: 'bench',
    GIT_AUTHOR_EMAIL: 'bench@example.invalid',
    GIT_COMMITTER_NAME: 'bench',
    GIT_COMMITTER_EMAIL: 'bench@example.invalid',
  };
  // Every call passes -c init.defaultBranch=master (U01b lint); never interactive.
  return (cwd, args, input) => {
    const r = spawnSync(exe, ['-c', 'init.defaultBranch=master', '-c', 'core.autocrlf=false', ...args], {
      cwd, env, input, encoding: 'utf8', timeout: TIMEOUT_MS, maxBuffer: MAX_BUFFER, stdio: ['pipe', 'pipe', 'pipe'],
    });
    if (r.status !== 0) throw new Error(`${soft ? 'asc' : 'git'} ${args.join(' ')} failed (${r.status}): ${(r.stderr || r.error || '').toString().slice(0, 500)}`);
    return r.stdout;
  };
}

function inside(root, p) {
  return resolve(p).startsWith(resolve(root) + sep);
}

function prepare(git, work, size, asc) {
  const dir = join(work, size);
  if (size === 'asc') {
    if (!existsSync(join(asc, '.git'))) throw new Error(`asc source not found: ${asc}`);
    // --local clone only reads the source; the clone is the benchmark subject.
    git(work, ['clone', '-q', '--local', '--', asc, dir]);
    git(dir, ['checkout', '-q', 'master']);
  } else {
    mkdirSync(dir);
    git(dir, ['init', '-q']);
    const { files, commits } = SIZES[size];
    git(dir, ['fast-import', '--quiet', '--done'], fastImportStream(files, commits));
    git(dir, ['checkout', '-q', '-f', 'master']);
  }
  return dir;
}

/** Classify an asc failure: not-yet-implemented command vs a real error (message bounded). */
function failureResult(message) {
  const unsupported = /not supported yet/.test(message);
  return { status: unsupported ? 'unsupported' : 'error', error: message.slice(0, 300) };
}

/** Build an asc repo from the last SEED_DEPTH git snapshots (oldest first). Returns false on failure. */
function seedAsc(git, asc, dir, ascDir, work) {
  const snap = join(work, `${basename(ascDir)}-snap`);
  try {
    mkdirSync(ascDir);
    asc(ascDir, ['init', '-q']);
    git(work, ['clone', '-q', '--local', '--no-checkout', '--', dir, snap]);
    const n = Math.min(SEED_DEPTH, Number(git(dir, ['rev-list', '--count', 'HEAD']).trim()));
    for (let k = n - 1; k >= 0; k--) {
      for (const e of readdirSync(ascDir)) if (e !== '.git') rmSync(join(ascDir, e), { recursive: true, force: true });
      git(snap, [`--work-tree=${ascDir}`, 'checkout', '-q', '-f', `origin/master~${k}`, '--', '.']);
      asc(ascDir, ['add', '-A']);
      asc(ascDir, ['commit', '-q', '-m', `snapshot ${k}`]);
    }
    return true;
  } catch (e) {
    log(`asc seed failed: ${e.message}`);
    return false;
  } finally {
    rmSync(snap, { recursive: true, force: true, maxRetries: 3 });
  }
}

function benchRepo(git, tool, dir, size, runs, work, label) {
  const tracked = git(dir, ['ls-files', '-z']).split('\0').filter(Boolean);
  if (tracked.length < 10) throw new Error(`${size}: too few files`);
  const blameFile = tracked.find((f) => /\.(txt|md|cs|rs|ts)$/.test(f)) ?? tracked[0];
  const back = Number(git(dir, ['rev-list', '--count', 'HEAD']).trim()) >= 4 ? 3 : 1;
  let gen = 0;
  const dirty = () => {
    gen++;
    for (const f of tracked.slice(0, 10)) appendFileSync(join(dir, f), `\nbench edit ${gen}\n`);
    for (let i = 0; i < 5; i++) writeFileSync(join(dir, `bench-new-${gen}-${i}.txt`), `new ${gen} ${i}\n`);
  };
  const cl = (i) => join(work, `${size}-clone-${i}`);
  const wt = (i) => join(work, `${size}-wt-${i}`);
  const ops = {
    status: { pre: () => {}, cmd: () => ['status', '--porcelain'] },
    add: { pre: () => git(dir, ['reset', '-q']), cmd: () => ['add', '-A'] },
    commit: { pre: () => { dirty(); git(dir, ['add', '-A']); }, cmd: () => ['commit', '-q', '-m', 'bench'] },
    checkout: { pre: () => git(dir, ['checkout', '-q', 'master']), cmd: () => ['checkout', '-q', '--detach', `master~${back}`], post: () => git(dir, ['checkout', '-q', 'master']) },
    log: { pre: () => {}, cmd: () => ['log', '--oneline', '-n', '1000'] },
    diff: { pre: () => {}, cmd: () => ['diff', '--stat', `master~${back}`, 'master'] },
    blame: { pre: () => {}, cmd: () => ['blame', '--', blameFile] },
    clone: { pre: () => {}, cmd: (i) => ['clone', '-q', '--', dir, cl(i)], post: (i) => rmSync(cl(i), { recursive: true, force: true, maxRetries: 3 }) },
    'worktree-add': { pre: () => {}, cmd: (i) => ['worktree', 'add', '-q', '--detach', wt(i), 'master'], post: (i) => { git(dir, ['worktree', 'remove', '--force', wt(i)]); git(dir, ['worktree', 'prune']); } },
  };
  dirty(); // state for status/add: 10 modified + 5 untracked files
  const results = [];
  for (const op of OPS) {
    const o = ops[op];
    const ms = [];
    let failure = null;
    for (let i = -1; i < runs && !failure; i++) { // run -1 is an untimed warm-up
      const tag = i < 0 ? 'w' : i;
      o.pre();
      const args = o.cmd(tag);
      const t0 = process.hrtime.bigint();
      try {
        tool(dir, args);
      } catch (e) {
        if (label === 'git') throw e; // a git failure is a harness bug; an asc failure is a result
        failure = e.message;
      }
      const dt = Number(process.hrtime.bigint() - t0) / 1e6;
      try { o.post?.(tag); } catch (e) { failure ??= e.message; }
      if (i >= 0 && !failure) ms.push(Math.round(dt * 100) / 100);
    }
    results.push(failure ? { size, op, ...failureResult(failure), asc: null } : { size, op, runs: ms.length, ms, ...stats(ms), asc: null });
    log(`${label} ${size} ${op}: ${failure ? results.at(-1).status : `median ${results.at(-1).median} ms`}`);
  }
  return { results, files: tracked.length, blameFile };
}

function main() {
  const o = parseArgs(process.argv.slice(2), process.env);
  const out = resolve(o.out);
  mkdirSync(out, { recursive: true });
  const base = o.work ? resolve(o.work) : tmpdir();
  mkdirSync(base, { recursive: true });
  const work = mkdtempSync(join(base, 'asc-bench-git-'));
  const git = makeRunner(work, 'git');
  // ASC_BIN (set by the caller; never discovered from PATH) enables the asc column.
  const ascBin = process.env.ASC_BIN ? resolve(process.env.ASC_BIN) : null;
  if (ascBin && !existsSync(ascBin)) throw new Error(`ASC_BIN not found: ${ascBin}`);
  const asc = ascBin ? makeRunner(work, ascBin, true) : null;
  const doc = { meta: { git: git(work, ['--version']).trim(), asc: asc ? asc(work, ['--version']).trim() : null, platform: process.platform, runs: o.runs, sizes: o.sizes, asc: o.sizes.includes('asc') ? o.asc : null, repos: {} }, results: [] };
  try {
    for (const size of o.sizes) {
      const dir = prepare(git, work, size, o.asc);
      const r = benchRepo(git, git, dir, size, o.runs, work, 'git');
      // asc cannot open a plain git repo, so it gets its own repo seeded with the last SEED_DEPTH
      // snapshots of the git history (one asc commit each); setup steps then run through asc itself.
      if (asc) {
        const ascDir = `${dir}-asc`;
        let c = null;
        try {
          if (seedAsc(git, asc, dir, ascDir, work)) c = benchRepo(asc, asc, ascDir, size, o.runs, work, 'asc');
        } catch (e) {
          log(`asc ${size} aborted: ${e.message}`);
        }
        for (const [i, x] of r.results.entries()) {
          if (!c) x.asc = { status: 'error', error: 'asc repo seeding or setup failed' };
          else {
            const { size: _s, op: _o, asc: _c, ...rest } = c.results[i];
            x.asc = rest;
          }
        }
        if (inside(work, ascDir)) rmSync(ascDir, { recursive: true, force: true, maxRetries: 3 });
      }
      doc.meta.repos[size] = { files: r.files, blameFile: r.blameFile };
      doc.results.push(...r.results);
      if (inside(work, dir)) rmSync(dir, { recursive: true, force: true, maxRetries: 3 }); // free space after each size
    }
  } finally {
    if (!o.keep && inside(base, work)) rmSync(work, { recursive: true, force: true, maxRetries: 3 });
  }
  writeFileSync(join(out, 'results.json'), JSON.stringify(doc, null, 2) + '\n');
  writeFileSync(join(out, 'results.md'), markdown(doc));
  log(`wrote ${join(out, 'results.json')} and results.md`);
}

if (process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url))) {
  try { main(); } catch (e) { log(`bench: ${e.message}`); process.exit(1); }
}
