# lint-staged traces

| Trace | Driver |
|---|---|
| `partially-staged` | A file with staged and unstaged hunks: lint-staged backs up with `stash create` / `stash store`, hides the unstaged hunk, runs the task, stages the result and re-applies the hunk. |
| `restore-conflict` | The task changes the line the unstaged hunk touches: `apply` fails, `apply --3way` fails, lint-staged resets to HEAD and restores the backup stash. |
| `task-failure` | The task fails: lint-staged resets and restores from the backup. |
| `add-delete-rename` | A staged new file, deletion and rename next to a modified file. |

The forms SPEC §11.5 lists (`stash create|store`, `diff --binary --output --src-prefix
--submodule=short`, `apply --recount --unidiff-zero --whitespace=nowarn --3way`) all occur here.

## Versions

lint-staged 17.6.0, git 2.55. Pins are in `../pins.toml`.

## Provenance

Recorded by `../record/record.sh` from `../record/scenarios/lint-staged/*.sh`. The task is a
shell snippet run through `record/act.mjs`, so its effect on the files is a `sh` step in the
trace. The `.git/lint-staged/<hash>/` directory and the backup message carry a random hash per
recording; a trace keeps the values of its own recording, so re-recording changes them.
Not recorded: stdin (lint-staged pipes nothing to git).
