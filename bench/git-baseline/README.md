# asc vs git benchmark (BENCH0, BENCH1)

Hyperfine-style timings of `git status/add/commit/checkout/log/diff/blame/clone/worktree add`
on small (20 files), medium (2000 files) and ASC-sized repos. With `ASC_BIN` set, the same
commands also run through `asc` and the report adds an `asc` and an `asc/git` ratio column per size.

    bash bench/git-baseline/run.sh --sizes small,medium --runs 5 --out target/bench-git-baseline
    ASC_BIN="$PWD/target/release/asc.exe" bash bench/git-baseline/run.sh --sizes small,medium,asc --runs 3 --out target/bench-asc-vs-git
    node --test bench/git-baseline/

Options: `--sizes small,medium,asc`, `--runs N` (1..100, plus one untimed warm-up each),
`--out DIR`, `--work DIR` (scratch location, default OS temp), `--asc PATH` (default
`D:\dev\monorepo`, or `ASC_BENCH_ASC`), `--keep` (keep scratch repos).

Outputs `results.json` (per-run ms, min/median/mean/max/stddev; `asc` is `null` without `ASC_BIN`,
otherwise the same stats or `{status: "unsupported"|"error", error}`) and `results.md`.

asc column
- `ASC_BIN` must point at the asc binary; it is never looked up on PATH. Build first with
  `cargo build -p asc --release`.
- asc cannot open a plain git repo, so each size gets an asc repo (`asc init`) seeded with the last 4
  snapshots of the git history, one asc commit each. Setup steps for asc run through asc.
- A command asc does not implement yet ("not supported yet": blame, clone, worktree) is reported
  as `unsupported`; any other failure is `error` with a bounded message. Neither aborts the run.
- A ratio above 1.00x means asc is slower than git for that op.

Notes
- `asc` is a `git clone --local` of the ASC repo into scratch; the source is only read. Scratch
  repos are deleted after each size to save disk (unless `--keep`).
- Every git call passes `-c init.defaultBranch=master`, ignores system/global config, and is
  non-interactive (`GIT_TERMINAL_PROMPT=0`). Nothing is pushed.
- Needs `git` and `node` (no hyperfine). Sizes, run counts and git output are bounded.
