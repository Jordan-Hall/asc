# turbo traces

| Trace | Driver |
|---|---|
| `dry-run` | `turbo run build --dry=json` in a bun workspace with two packages, on a clean tree and then with an edited, a staged and an untracked file. |
| `affected` | A branch that edits one package and the lockfile: `--affected`, `--filter=[master...HEAD]` and `turbo ls --affected` with `TURBO_SCM_BASE`. |

Spike S4 (docs/SPEC.md §19) asks for every git invocation turbo makes with the synthetic `.git`;
these traces are the real-git side of it. turbo 2.10 reads the index and the object database
itself, so it calls git only for `rev-parse`, `branch --show-current`, `diff HEAD`,
`diff-tree --merge-base`, `ls-files --others --modified`, `diff --name-only --cached` and
`show <base>:bun.lock`.

## Versions

turbo 2.10.11 (the version ASC pins), bun 1.4.2 as package manager, git 2.55. Pins are in
`../pins.toml`.

## Provenance

Recorded by `../record/record.sh` from `../record/scenarios/turbo/*.sh`. The fixture's `bun.lock`
is written by hand in the scenario (turbo only parses it). The task output (`echo a`) is not
part of the trace.
