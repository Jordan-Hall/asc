# Adapter gaps (A-n)

Differences, or missed differences, caused by the test harness (the GIT-ORACLE: the runner in `tests/compat/src/lib.rs`, the trace replayer in `tests/traces`, and how they find and run git). They are not product differences and are tracked here so that a harness bug never passes for a divergence, and a divergence never hides behind harness noise. Method and grammar: `docs/git-fidelity.md`.

Each gap has a **kind**:

| Kind | Meaning |
|---|---|
| `spurious` | the harness makes a difference appear that the product does not have. |
| `blind` | the harness cannot see a difference the product has, or does not run a case the SPEC requires. |
| `drift` | the oracle is not one fixed git, so a result can change with the machine. |

Seeded 2026-10-23 from `tests/compat/src/lib.rs`, `crates/asc-git/src/realgit.rs`, `build/ci/linux-check.sh`, `.forgejo/workflows/asc-ci.yml` and `deploy/Dockerfile.serve`. Counts: 5 gaps (`spurious` 0, `blind` 2, `drift` 3). Q03 adds the preflight that reports a failure it explains as `A-n`; Q02 closes the `drift` gaps.

### A-1 No "program name in hints" normalisation
- **Kind:** blind
- **Gap:** SPEC §17.1 says the runner normalises the program name in hint and usage text. `lib.rs` normalises only the root path (native, slash and MSYS forms) and progress lines. Text that carries the program name is compared raw, so a run through a binary not named `git` would show a false difference.
- **Evidence:** `tests/compat/src/lib.rs` (normalisation), SPEC §17.1.
- **Closed by:** Q03 (normalisation) or a SPEC edit that drops the requirement.
- **Status:** open
- **SPEC:** §17.1, §11.6

### A-2 No double run with `asc.branchless` true and false
- **Kind:** blind
- **Gap:** SPEC §21.1 BD7 requires the local commands to produce byte-identical results in both modes, and the compat suite to run both. `lib.rs` and `crates/asc/tests/compat.rs` run one mode only (`branchless` appears in neither). The branchless divergences `D-5` and `D-6` cannot be proven until it does, and a branchless-only difference in a local command would not be seen.
- **Evidence:** `grep -c branchless tests/compat/src/lib.rs crates/asc/tests/compat.rs` is 0 for both.
- **Closed by:** Q03 (a `# compat: branchless=both` marker, the second run), with the scenarios `D-5` and `D-6` name.
- **Status:** open
- **SPEC:** §21.1 BD7, §17.1

### A-3 The oracle git is not pinned
- **Kind:** drift
- **Gap:** the git a run compares against is whatever `RealGit::resolve` finds: `ASC_REAL_GIT`, `asc.realGit`, the cache, `PATH`, then Git for Windows. Only the floor, 2.40, is checked, and only major and minor. The developer machine has 2.55.0.windows.5; the Linux CI job installs the newest from `ppa:git-core/ppa`; the Windows CI job uses the runner's; `deploy/Dockerfile.serve` has Debian trixie's 2.47. A suite result can change with the machine.
- **Evidence:** `crates/asc-git/src/realgit.rs` (`MIN_VERSION`, `check_version`), `build/ci/linux-check.sh` line 3, `.forgejo/workflows/asc-ci.yml`, `deploy/Dockerfile.serve`.
- **Closed by:** Q02 (the pinned oracle, `ASC_GIT_ORACLE`, CI images pinned by hash).
- **Status:** open
- **SPEC:** §0 item 6, §11.1, §17.1

### A-4 The version gate reads only major and minor
- **Kind:** drift
- **Gap:** a `2.55.x` that is not v2.55.0, or a Git for Windows build with its own patches, passes the gate. Even with `A-3` closed the harness would not notice an oracle that is the wrong build of the right minor version.
- **Evidence:** `crates/asc-git/src/realgit.rs` `check_version`.
- **Closed by:** Q02 (the compat harness compares the oracle's `git version --build-options` and the file hash with the pin; `RealGit` keeps its 2.40 floor for users).
- **Status:** open
- **SPEC:** §11.1

### A-5 The shell the scenarios run in is whatever the machine has
- **Kind:** drift
- **Gap:** scenarios are POSIX `sh` scripts. The runner takes `sh` from `ASC_COMPAT_SH`, then `PATH`, then Git for Windows' `usr/bin` (Git for Windows' bundled shell on Windows, dash on Debian and Ubuntu). A scenario that uses a construct the local `sh` treats differently fails on one platform on its own. Time zone and locale are fixed (`TZ=UTC`, `LC_ALL=C`); the shell is not.
- **Evidence:** `tests/compat/src/lib.rs` (`locate_sh`, `fixed_env`).
- **Closed by:** Q03 (preflight prints the shell's identity and runs a fixed probe script; a mismatch is reported as `A-5`, not as a product failure), and Q02 for the Windows shell, which ships in the pinned Git for Windows.
- **Status:** open
- **SPEC:** §17.1
