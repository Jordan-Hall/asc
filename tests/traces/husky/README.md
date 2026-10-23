# husky traces

| Trace | Driver |
|---|---|
| `hooks` | `husky` (the `prepare` script) sets `core.hooksPath`. Hooks: `pre-commit` runs lint-staged, `commit-msg` runs commitlint, `post-checkout` looks for a changed lockfile, `pre-push` refuses a dirty tree. Commits (one rejected by commitlint), a branch switch, a partially staged commit and a push to a bare remote. |

## Versions

husky 9.1.7, lint-staged 17.6.0 and commitlint 21.2.4 (the hooks run them), git 2.55. Pins are in
`../pins.toml`; the packages are in `../record/tools/package-lock.json`.

## Provenance

Recorded by `../record/record.sh` from `../record/scenarios/husky/hooks.sh`. husky itself only
runs `git config core.hooksPath .husky/_`. The calls in each `[[step.hook]]` are the ones the
hook's tool made while git ran the hook, with `GIT_INDEX_FILE` and `GIT_EDITOR` as git set them.
The replay writes each hook as a script that repeats those calls and deletes it after the step.
Not recorded: stdin (git feeds `pre-push` its ref list, which the hook ignores).
