# Spike P51: degraded features without `VcsLogProvider`; comments API survey

Status: scaffold only. No Gradle wrapper, no JDK 21 and no IDE 2026.1 were available when this unit ran, so no row below is observed.
Every "Observed" cell is `UNVERIFIED` and the SPEC 24.5.4 hypothesis stands. Do not treat this file as a finding.

## How to run

1. Add `include(":spikes:vcslog")` to the root `settings.gradle.kts` (owned by P36).
2. `./gradlew :spikes:vcslog:test` checks the precondition (no `VcsLogProvider` registered; this file lists every row).
3. `./gradlew runIde` on 2026.1 with an `asc` repository open (Rider, RustRover, PyCharm, WebStorm optional), exercise each row, and replace `UNVERIFIED`.

## Degraded features (hypothesis from SPEC 24.5.4)

| Platform feature | Expected | Observed on 2026.1 | Replacement |
|---|---|---|---|
| Git tool window Log tab, Show in Log, Go to hash/branch | absent | UNVERIFIED | `asc` tool window Graph tab |
| Compare with Branch / Tag / Revision | absent or file-only | UNVERIFIED | `asc-ui` compare (P18), `DiffProvider` |
| Directory and project history | file history works; directory history needs `supportsHistoryForDirectories` | UNVERIFIED | Graph tab path filter |
| Cherry-pick, revert, reset from Log menu | absent | UNVERIFIED | `asc-ui` graph menu via the relay |
| Annotate gutter "Show in Log" | opens nothing | UNVERIFIED | open the `asc` tool window on that change |
| Branches popup, Update Project | Git popup absent | UNVERIFIED | `AscActions` |

## Comments API survey for diff editors (2026.1 and 2026.2)

| Question | Observed |
|---|---|
| Is there a public, non-internal API to add inline threads to a diff editor (for example a `DiffExtension` plus editor inlays, or the collaboration-tools comment UI)? | UNVERIFIED |
| Are those classes annotated `@ApiStatus.Internal` or `Experimental` on 2026.1 and 2026.2 (check with `verifyPlugin`)? | UNVERIFIED |

Decision rule (SPEC 24.5.4): a stable API on both versions moves inline review comments from v1.x to v1. Until the survey is run the decision stays v1.x, as SPEC 24.3 states.
