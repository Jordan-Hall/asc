# commitlint traces

| Trace | Driver |
|---|---|
| `edit-message` | `commitlint --edit .git/COMMIT_EDITMSG` with a valid and an invalid message, as a `commit-msg` hook runs it. |
| `history-range` | `commitlint --from master --to topic` and `commitlint --last`, as a CI step runs them. |

## Versions

@commitlint/cli 21.2.4, git 2.55. The rules are written inline in the recorder scenarios, so no
shared config package is involved. Pins are in `../pins.toml`.

## Provenance

Recorded by `../record/record.sh` from `../record/scenarios/commitlint/*.sh`. commitlint's git
use is small: `config core.commentChar`, `merge-base`, and `log` with a format. The `commit-msg`
hook case is also in `../husky/hooks.trace`.
