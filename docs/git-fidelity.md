# Git fidelity

How asc stays faithful to git, and how every place where it is not is recorded. This is the method; the registers are `git-quirks.md`, `git-defects.md`, `divergences.md` and `adapter-gaps.md`. SPEC §0 item 6, §11 and §17.1 fix the goal ("asc is git at the surface"); this file fixes the discipline. Units: PLAN Q01 to Q04.

## The method

Four things are kept apart, so that a difference is always attributed to the right party.

| Name | What it is | In asc |
|---|---|---|
| IDEAL GIT | The documented protocol and formats: git-scm docs, `gitprotocol-*`, `gitformat-*`, `gitcredentials`, or a clean implementation of them. | The reading of the docs, cited per entry. |
| PINNED GIT | One exact git build. | v2.55.0, below. |
| MODEL | asc's own implementation of git semantics. | `asc-core`, `asc-engine`, `asc-git`, the CLI. |
| GIT-ORACLE | The compat harness's view of pinned git: the binary it runs, the environment it gives it, the way it normalises the output. | `tests/compat`, `tests/traces`, `ASC_GIT_ORACLE` (Q02). |

Three gaps between them, three registers:

1. **Pinned git differs from ideal git.** Either git does something odd but intended (a **quirk**, `Q-n`, `git-quirks.md`) or it has an upstream bug (a **defect**, `U-n`, `git-defects.md`). Each gets exactly one decision (below).
2. **The model differs from pinned git.** A **divergence** (`D-n`, `divergences.md`), one class each. A divergence is either listed under `## Upstream` because a quirk or defect decision caused it (it cites the `Q-n` or `U-n`), or under `## Own divergences` because asc chose it for its own reasons (security, workspace architecture, branchless mode).
3. **The oracle differs from pinned git.** An **adapter gap** (`A-n`, `adapter-gaps.md`). It is a harness bug or a harness blind spot, never a product difference, and is tracked on its own so that harness bugs never pass for product differences (and product differences never hide behind harness noise).

### Ids

Register ids carry a hyphen: `Q-1`, `U-1`, `D-1`, `A-1`. PLAN already uses `Q01` to `Q04` for these units and `U01` to `U89` for the older unit list, so an unhyphenated `Un` always means a PLAN unit and `U-n` always means a defect. Ids are never reused or renumbered; a retired entry keeps its id with `Status: retired`.

## The pinned version

**git v2.55.0**, source tarball `git-2.55.0.tar.xz`, sha256

```
457fdb04dc8728e007d4688695e6912e6f680727920f2a40bf11eacc17505357
```

(checked against `https://mirrors.edge.kernel.org/pub/software/scm/git/sha256sums.asc` and against a download of the tarball, 2026-10-23).

Windows oracle: Git for Windows `v2.55.0.windows.5`, `MinGit-2.55.0.5-64-bit.zip`, sha256

```
56d7b226b7693196cfc71fef26568f536c4a021ab6c37ff2db4287bed908e96e
```

(the digest GitHub publishes for the release asset). The developer machine already runs `git version 2.55.0.windows.5`.

Why 2.55.0:

- It is the version asc already claims. SPEC §0 item 6 and §11.1 say "git 2.55"; `crates/asc/src/dispatch.rs` lists the builtins of 2.55, `crates/asc/src/cli/*/usage.rs` copies its usage texts and `msg.rs` copies its msgids; `tests/traces/pins.toml` records `git = "2.55"`. Moving the target moves all of that.
- It is the newest 2.55 release. `sha256sums.asc` lists no 2.55.x maintenance tarball. 2.56.0 exists upstream; moving to it is a deliberate bump (below), not a drift.
- The tarball is the one thing that can be hashed on every platform. Git for Windows carries its own patches (`.windows.N`), so the Windows oracle is pinned separately by its asset hash, and the two are expected to agree on every scenario. If they disagree, that is an adapter gap (`A-n`) or a quirk of the Windows build, recorded as such.

Today nothing enforces this. `RealGit` accepts any git from 2.40 up and compares only major and minor (`crates/asc-git/src/realgit.rs`, `MIN_VERSION`), CI uses whatever the runner ships (`ppa:git-core/ppa` on Linux, the runner's own on Windows) and `deploy/Dockerfile.serve` uses Debian trixie's 2.47. That drift is `A-3`. Q02 closes it.

The 2.40 floor stays: it is what asc needs to run against a user's git. The pin is what asc is tested against.

### Bumping the pin

A bump is one change: new tarball hash, new Git for Windows asset hash, `pins.toml`, the CI images, then a full compat run. New failures are classified by the rules below; every classified failure becomes a register entry or a fix in the same change. A bump never edits `allowed.toml` without a register id.

## The decision rule

Every quirk and every defect has exactly one decision.

| Decision | Meaning | Allowed when | Where it is recorded |
|---|---|---|---|
| **replicate** | asc does what pinned git does. | Required for anything on a wire or on disk: object bytes, pack, pkt-line, refs, index, config, hook arguments and environment. Also allowed for local output scripts parse. | `## Git quirks kept` (or `## Git defects kept`) in its own register. Not a divergence. |
| **refuse** | asc refuses the input, by name. | Input asc will not act on: a path, a header, an id, a credential flow. | The register entry, plus a bullet under `## Upstream` in `divergences.md` with the refusal text. The refusal's error message names the id (for example `... (git-fidelity Q-3)`), so a user or a log can find this file. A refusal that is an omission (asc does not send something) cannot print an error; its test asserts the omission and carries the id. |
| **idealize** | asc does what ideal git says. | Only purely local, human-facing output or dispatch. Never a wire or disk format, never anything a tool round-trips. | The register entry, plus a bullet under `## Upstream`. |

Rules, in force order:

1. **Wire and on-disk formats are always replicate.** If another git must read it, or a git hook or tool must parse it, asc writes what pinned git writes, byte for byte, quirks included. A format quirk asc cannot represent is kept as raw bytes (`Q-5`, `Q-8`), never normalised.
2. **Idealize is for purely local human output only.** If a script, hook, IDE or another git could see the difference, the decision is replicate or refuse.
3. **Refuse names its id.** `refuse` without an id in the error (or, for an omission, in the asserting test) is a defect in asc.
4. **One entry, one decision, one proof.** No entry has two decisions. A decision that changes is a new entry that supersedes the old one (`Status: superseded by Q-n`).
5. **A decision needs a proof.** A scenario in `tests/compat/scenarios/`, a test (`crates/*/tests/`, an inline `#[test]`) or a trace, that runs in CI and fails if asc does the other thing. An `asc-only` scenario is a valid proof when the decision is "asc refuses".
6. **The ideal is cited.** An entry says what the docs or the clean protocol say. If the docs are silent the entry says so and the ideal is the least surprising reading.

## Classifying a failure

A compat failure is, in order:

1. **Adapter gap** if the harness preflight explains it (Q03): an exec bit, a missing or different `sh`, the default branch, the time zone, the locale, an oracle that is not the pinned build. Report it as `A-n` and fix the harness. The product is not judged.
2. **Quirk or defect decision** if the difference is a decision already in `git-quirks.md` or `git-defects.md`. The expected difference lives in `tests/compat/allowed.toml` with `register = "Q-n"` (or `U-n`). Nothing else may drop lines.
3. **Product divergence** if asc differs from pinned git and no decision covers it. Two outcomes only: fix asc, or add a `D-n` (own) or a `Q-n`/`U-n` with a decision and an allowed entry. Both need the SPEC section that justifies it.
4. Otherwise a **product bug**: fix asc.

Flow in one line: preflight explains it, then `A-n`; a register id covers it, then it is expected; else fix or register. The lint (Q01) enforces the second step, the preflight (Q03) the first.

## Register grammar

Each register is Markdown with one `###` section per entry. The lint parses it, so the shape is fixed.

```
### Q-3 update-index --cacheinfo accepts an unknown object id
- **Git:** what pinned git does (v2.55.0).
- **Ideal:** what the docs or the clean protocol say.
- **Decision:** refuse                     (one word: replicate | refuse | idealize)
- **Refusal:** error                       (refuse only: `error` = the message names the id; `omission` = asc does not do it, the test names the id)
- **Proof:** `path::name`, `path`          (comma list; each file must exist)
- **Status:** proven                       (proven | pending <PLAN unit> | open | retired)
- **SPEC:** §n.n
```

Divergences (`D-n`) use **Class** instead of Decision and Ideal, and **Cites** for an upstream bullet. Adapter gaps use **Kind** (`spurious`, `blind`, `drift`). Only `proven` and `retired` entries pass the strict lint. `pending` entries name the PLAN unit that writes the proof, as `traces/pending.toml` does.

Markers the lint reads (Q01):

- Every `[[allow]]` in `tests/compat/allowed.toml` gets `register = "<id>"`, an id that exists.
- An `asc-only` scenario that stands in for a decision gets `# fidelity: <id>[, <id>]` within its first five lines (the runner reads only the exact `# compat: asc-only` line, so this does not disturb it).
- A Rust test that proves an entry has `// fidelity: <id>` on the line above the `#[test]`.
- A `tests/traces/pending.toml` entry with `differs = true` gets `register = "<id>"`.

## The checks

- Lint (Q01): every id cited by `allowed.toml`, a scenario, a test, a trace or an upstream bullet exists; every `proven` entry is cited by a file that exists; every entry has one decision (or class, kind); every refuse has a bullet under `## Upstream` and a `Refusal` kind; the text `git-fidelity <id>` occurs in a non-test source file under `crates/` for every refuse whose kind is `error`; every `## Upstream` bullet cites a quirk or defect whose decision is refuse or idealize.
- Oracle (Q02): the compat suite and the traces run against the pinned build through `ASC_GIT_ORACLE`, and the suite fails if the oracle is not 2.55.0 by hash.
- Preflight (Q03): adapter gaps are reported as `A-n` before the product is blamed.
- Differential fuzz (Q04): generated operation sequences run against asc and the oracle; a difference must map to a register entry or fail.

## Not covered here

The harness shape (`tests/compat/src/lib.rs`, scenarios, `allowed.toml`) stays as SPEC §17.1 describes it. The SPEC edits this method implies (an amendment to §17.1 for the register, the lint, the oracle and the preflight) belong to the Q units; they are listed in PLAN Q01 to Q03 and not made here.
