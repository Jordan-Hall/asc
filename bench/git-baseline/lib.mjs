// Pure helpers for the git baseline harness (tested by bench.test.mjs).
export const SIZES = Object.freeze({
  small: { files: 20, commits: 6 },
  medium: { files: 2000, commits: 100 },
  asc: { files: 0, commits: 0 },
});
export const OPS = Object.freeze(['status', 'add', 'commit', 'checkout', 'log', 'diff', 'blame', 'clone', 'worktree-add']);
export const MAX_RUNS = 100;

export function parseArgs(argv, env = {}) {
  const o = { sizes: ['small', 'medium'], runs: 5, out: 'target/bench-git-baseline', work: null, asc: env.ASC_BENCH_ASC || 'D:\dev\monorepo', keep: false };
  const val = (i, f) => {
    if (i + 1 >= argv.length) throw new Error(`${f} needs a value`);
    return argv[i + 1];
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--sizes') o.sizes = val(i++, a).split(',').filter(Boolean);
    else if (a === '--runs') {
      const s = val(i++, a);
      if (!/^\d{1,3}$/.test(s)) throw new Error('--runs must be an integer');
      o.runs = Number(s);
    } else if (a === '--out') o.out = val(i++, a);
    else if (a === '--work') o.work = val(i++, a);
    else if (a === '--asc') o.asc = val(i++, a);
    else if (a === '--keep') o.keep = true;
    else throw new Error(`unknown argument: ${a}`);
  }
  if (o.runs < 1 || o.runs > MAX_RUNS) throw new Error(`--runs must be 1..${MAX_RUNS}`);
  if (o.sizes.length === 0) throw new Error('--sizes is empty');
  for (const s of o.sizes) if (!Object.hasOwn(SIZES, s)) throw new Error(`unknown size: ${s}`);
  o.sizes = [...new Set(o.sizes)];
  return o;
}

export function stats(ms) {
  const s = [...ms].sort((a, b) => a - b);
  const n = s.length;
  const mean = s.reduce((a, b) => a + b, 0) / n;
  const median = n % 2 ? s[(n - 1) / 2] : (s[n / 2 - 1] + s[n / 2]) / 2;
  const sd = n > 1 ? Math.sqrt(s.reduce((a, b) => a + (b - mean) ** 2, 0) / (n - 1)) : 0;
  const r = (x) => Math.round(x * 100) / 100;
  return { min: r(s[0]), median: r(median), mean: r(mean), max: r(s[n - 1]), stddev: r(sd) };
}

// Deterministic fast-import stream: `commits` commits over `files` files, branch master.
export function fastImportStream(files, commits) {
  const parts = [];
  const blob = (text) => `data ${Buffer.byteLength(text)}\n${text}\n`;
  const content = (f, c) => Array.from({ length: 20 }, (_, l) => `file ${f} line ${l} rev ${c}`).join('\n') + '\n';
  for (let c = 0; c < commits; c++) {
    const msg = `commit ${c}`;
    parts.push(`commit refs/heads/master\ncommitter bench <bench@example.invalid> ${1700000000 + c * 60} +0000\ndata ${msg.length}\n${msg}\n`);
    const touched = c === 0 ? files : Math.max(1, Math.floor(files / 20));
    for (let k = 0; k < touched; k++) {
      const f = c === 0 ? k : (k * 20 + c) % files;
      parts.push(`M 100644 inline src/d${f % 50}/f${f}.txt\n${blob(content(f, c))}`);
    }
    parts.push('\n');
  }
  parts.push('done\n');
  return parts.join('');
}

export function markdown(doc) {
  const ops = OPS;
  const out = [`# Git baseline (${doc.meta.git}, ${doc.meta.runs} runs, median ms)`, ''];
  out.push(`| op | ${doc.meta.sizes.map((s) => `git ${s}`).join(' | ')} | asc |`);
  out.push(`|---|${doc.meta.sizes.map(() => '---:').join('|')}|---|`);
  for (const op of ops) {
    const cells = doc.meta.sizes.map((s) => {
      const r = doc.results.find((x) => x.size === s && x.op === op);
      return r ? String(r.median) : 'n/a';
    });
    out.push(`| ${op} | ${cells.join(' | ')} |  |`);
  }
  out.push('');
  return out.join('\n');
}
