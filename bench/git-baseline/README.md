# Git baseline benchmark (BENCH0)

Hyperfine-style timings of `git status/add/commit/checkout/log/diff/blame/clone/worktree add`
on small (20 files), medium (2000 files) and ASC-sized repos. The `asc` column is left empty
for U25 to fill.

    bash bench/git-baseline/run.sh --sizes small,medium --runs 5 --out target/bench-git-baseline
    node --test bench/git-baseline/

Options: `--sizes small,medium,asc`, `--runs N` (1..100, plus one untimed warm-up each),
`--out DIR`, `--work DIR` (scratch location, default OS temp), `--asc PATH` (default
`D:\dev\monorepo`, or `ASC_BENCH_ASC`), `--keep` (keep scratch repos).

Outputs `results.json` (per-run ms, min/median/mean/max/stddev, `asc: null`) and `results.md`.

Notes
- `asc` is a `git clone --local` of the ASC repo into scratch; the source is only read. Scratch
  repos are deleted after each size to save disk (unless `--keep`).
- Every git call passes `-c init.defaultBranch=master`, ignores system/global config, and is
  non-interactive (`GIT_TERMINAL_PROMPT=0`). Nothing is pushed.
- Needs `git` and `node` (no hyperfine). Sizes, run counts and git output are bounded.
