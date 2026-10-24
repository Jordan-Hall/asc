# Adapter gaps (A-n)

Differences, or missed differences, caused by the test harness (the GIT-ORACLE: the runner in `tests/compat/src/lib.rs`, the trace replayer in `tests/traces`, and how they find and run git). They are not product differences and are tracked here so that a harness bug never passes for a divergence, and a divergence never hides behind harness noise. Method and grammar: `docs/git-fidelity.md`.

Each gap has a **kind**:

| Kind | Meaning |
|---|---|
| `spurious` | the harness makes a difference appear that the product does not have. |
| `blind` | the harness cannot see a difference the product has, or does not run a case the SPEC requires. |
| `drift` | the oracle is not one fixed git, so a result can change with the machine. |

Seeded 2026-10-23 from `tests/compat/src/lib.rs`, `crates/asc-git/src/realgit.rs`, `build/ci/linux-check.sh`, `.forgejo/workflows/asc-ci.yml` and `deploy/Dockerfile.serve`. Counts: 6 gaps (`spurious` 1, `blind` 2, `drift` 3), all closed. Q02 closed A-3 and A-4 (the oracle is pinned and checked by hash). Q03 closed A-1, A-2 and A-5, and added A-6 with the preflight (`tests/compat/src/preflight.rs`) that reports a failure it explains as `adapter gap A-n` and does not judge the product.

### A-1 No "program name in hints" normalisation
- **Kind:** blind
- **Gap:** SPEC §17.1 says the runner normalises the program name in hint and usage text. `lib.rs` normalises only the root path (native, slash and MSYS forms) and progress lines. Text that carries the program name is compared raw, so a run through a binary not named `git` would show a false difference.
- **Evidence:** `tests/compat/src/lib.rs` (normalisation), SPEC §17.1.
- **Closed by:** Q03. `Normaliser` (`tests/compat/src/lib.rs`) writes the program word `asc` as `git` in `hint:`, `usage:` and `   or:` lines, on both sides; `asc: ` tags, `asc-only` and paths are kept. Proof: `crates/asc/tests/compat.rs::preflight_program_name_in_hints_is_normalised`.
- **Status:** closed
- **SPEC:** §17.1, §11.6

### A-2 No double run with `asc.branchless` true and false
- **Kind:** blind
- **Gap:** SPEC §21.1 BD7 requires the local commands to produce byte-identical results in both modes, and the compat suite to run both. `lib.rs` and `crates/asc/tests/compat.rs` run one mode only (`branchless` appears in neither). The branchless divergences `D-5` and `D-6` cannot be proven until it does, and a branchless-only difference in a local command would not be seen.
- **Evidence:** `grep -c branchless tests/compat/src/lib.rs crates/asc/tests/compat.rs` is 0 for both.
- **Closed by:** Q03. A scenario with `# compat: branchless=both` on its first five lines runs on the asc side twice (`ASC_BRANCHLESS=true`, then `false`); the two runs must be byte-identical and each must match the oracle, both up to `allowed.toml`. `branchless-push-tag.t` and `branchless-push-all.t` use it. Proof: `crates/asc/tests/compat.rs::preflight_the_double_run_sets_both_modes_and_compares_them`. The remote effects D-5 and D-6 describe are server policy and need B12; until then the scenarios prove the byte-identity and the `asc.branchless=false` half (see `divergences.md`).
- **Status:** closed
- **SPEC:** §21.1 BD7, §17.1

### A-3 The oracle git is not pinned
- **Kind:** drift
- **Gap:** the git a run compares against is whatever `RealGit::resolve` finds: `ASC_REAL_GIT`, `asc.realGit`, the cache, `PATH`, then Git for Windows. Only the floor, 2.40, is checked, and only major and minor. The developer machine has 2.55.0.windows.5; the Linux CI job installs the newest from `ppa:git-core/ppa`; the Windows CI job uses the runner's; `deploy/Dockerfile.serve` has Debian trixie's 2.47. A suite result can change with the machine.
- **Evidence:** `crates/asc-git/src/realgit.rs` (`MIN_VERSION`, `check_version`), `build/ci/linux-check.sh` line 3, `.forgejo/workflows/asc-ci.yml`, `deploy/Dockerfile.serve`.
- **Closed by:** Q02. `build/git-oracle/pins.env` pins v2.55.0 (`git-2.55.0.tar.xz`, sha256-verified) and Git for Windows `v2.55.0.windows.5` (`MinGit-2.55.0.5-64-bit.zip`, sha256-verified); `build/git-oracle/oracle.sh` fetches it; the compat harness reads `ASC_GIT_ORACLE` first, and with `ASC_ORACLE_STRICT=1` (set in CI and by `build/ci/linux-check.sh`) `Tools::get` fails with `adapter gap A-3` for any other git; `deploy/Dockerfile.serve` builds the same pin.
- **Status:** closed
- **SPEC:** §0 item 6, §11.1, §17.1

### A-4 The version gate reads only major and minor
- **Kind:** drift
- **Gap:** a `2.55.x` that is not v2.55.0, or a Git for Windows build with its own patches, passes the gate. Even with `A-3` closed the harness would not notice an oracle that is the wrong build of the right minor version.
- **Evidence:** `crates/asc-git/src/realgit.rs` `check_version`.
- **Closed by:** Q02. `oracle.sh fetch` writes `.oracle-hash` (the source digest and the sha256 of the built `git`); with `ASC_ORACLE_STRICT=1` the harness checks `git --version` (A-3), then that the record is for the pinned source and that the binary hashes to the record (`adapter gap A-4`). `RealGit` keeps its 2.40 floor for users.
- **Status:** closed
- **SPEC:** §11.1

### A-5 The shell the scenarios run in is whatever the machine has
- **Kind:** drift
- **Gap:** scenarios are POSIX `sh` scripts. The runner takes `sh` from `ASC_COMPAT_SH`, then `PATH`, then Git for Windows' `usr/bin` (Git for Windows' bundled shell on Windows, dash on Debian and Ubuntu). A scenario that uses a construct the local `sh` treats differently fails on one platform on its own. Time zone and locale are fixed (`TZ=UTC`, `LC_ALL=C`); the shell is not.
- **Evidence:** `tests/compat/src/lib.rs` (`locate_sh`, `fixed_env`).
- **Closed by:** Q03. The preflight reports the shell's identity (bash, ksh, zsh, or dash/busybox) and runs a fixed probe script (`$(( ))`, `printf --`, `test`, here-documents, `trap`); a difference from its golden output fails a differing case as `adapter gap A-5`, not as a product failure. Q02 covers the Windows shell, which ships in the pinned Git for Windows. Proof: `crates/asc/tests/compat.rs::preflight_a_forced_wrong_shell_prints_a5`.
- **Status:** closed
- **SPEC:** §17.1

### A-6 The scenario environment is assumed, not checked
- **Kind:** spurious
- **Gap:** the scenarios assume that a script made executable runs, that `git init` names `master`, that the clock is UTC and that git prints English. The runner fixes `TZ`, `LC_ALL` and the home config, but never checks that the machine honours them: a file system that drops the exec bit (hooks and scripts do not run), a `git` or `sh` that ignores `TZ`, or a translated message would make a case differ on the machine alone, and the failure would read as a product difference.
- **Evidence:** `tests/compat/src/lib.rs` (`fixed_env`, `PRELUDE`), before Q03 no check of any of them.
- **Closed by:** Q03. The preflight probes `exec-bit` (both sides), `default-branch` (both sides), `time-zone` and `locale` (the git side), once per test process and only when a case fails. A failed check that explains a differing section reports the case as `adapter gap A-6: <check>: <detail>`, with the text that the product was not judged. Proof: `crates/asc/tests/compat.rs::preflight_a_forced_exec_bit_failure_prints_a6`, `preflight_a_real_difference_with_a_clean_preflight_is_a_product_failure`.
- **Status:** closed
- **SPEC:** §17.1
