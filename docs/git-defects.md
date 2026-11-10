# Git defects (U-n)

Upstream bugs in pinned git (v2.55.0) or in a library it uses: behaviour nobody intends and the docs do not describe. Method and grammar: `docs/git-fidelity.md`. Odd-but-intended behaviour is in `git-quirks.md`. Ids carry a hyphen (`U-1`) because PLAN's `U01` to `U89` are units. Each entry has exactly one decision; a `refuse` or `idealize` entry also has a bullet under `## Upstream` in `divergences.md`.

Seeded 2026-10-23. Counts: 3 defects (replicate 0, refuse 3, idealize 0). Nothing here has been reported upstream; a report, when made, goes in the entry as `- **Upstream:** <link>`.

## Git defects kept

None. A defect in an on-disk or wire format that asc must replicate to interoperate would be recorded here with decision `replicate`.

## Defects not kept

### U-1 libcurl forwards a non-sensitive `http.extraHeader` to another host after a redirect
- **Git:** git hands `http.extraHeader` to libcurl as a custom header. After a 302 to a second host, libcurl strips `Authorization` and `Cookie` but still sends any other custom header (for example `X-Auth-Token: ...`) to the new host. A credential carried in an extra header leaks across origins.
- **Ideal:** `git-config(1)` `http.extraHeader` says the header is added "to all requests"; `gitcredentials(7)` and SPEC §15.6.4 say a credential goes only to the host it was obtained for. The ideal is that no header chosen for one origin follows a redirect to another.
- **Decision:** refuse (asc forwards no extra header to another origin; an omission, so the test asserts the new host sees none, and also that git does, to prove the oracle shows the defect)
- **Refusal:** omission
- **Proof:** `crates/asc/tests/auth_compat/main.rs::a_redirect_to_another_host_carries_no_credential`
- **Status:** proven
- **SPEC:** §15.6.4 item 3

### U-2 `apply` does not flag an alias of `.git` that is only the source of a rename, a deletion or a mode change
- **Git:** for a patch whose only mention of a `.git` alias (`.g<U+200C>it/config`, a trailing-dot or short-name form) is the source side of a rename, a deletion or a mode change, `git apply` reports a missing file, not an unsafe path. The refusal protecting `.git` fires on the write side only, so the failure message and the check depend on which side names the alias.
- **Ideal:** the path check applies to every path a patch names, whichever side; the failure says the path is unsafe.
- **Decision:** refuse (asc refuses every such patch up front, before anything is written, as unsafe)
- **Refusal:** error
- **Proof:** `tests/compat/scenarios/apply-unsafe-asc.t`
- **Status:** proven
- **SPEC:** §2.8, CARRY "Patch apply paths"

### U-3 `cherry-pick --abort` and `revert --abort` of a sequence started on an unborn branch
- **Git:** a multi-commit pick started on a branch with no commits records an empty `sequencer/head` and `sequencer/abort-safety`. If the first pick stops on a conflict, `--abort` fails with `fatal: could not parse .git/sequencer/abort-safety` and the sequence stays. If a root commit was made first, `--abort` moves the branch to the commit that was being picked, carrying that commit's history into the branch.
- **Ideal:** `--abort` returns to the state before the command: the branch unborn and the tree empty.
- **Decision:** refuse (asc refuses the abort, changes nothing and points at `--quit`, which ends the sequence and keeps any commits it made)
- **Refusal:** error
- **Proof:** `crates/asc/tests/cli_pick_unborn.rs::an_abort_of_a_sequence_started_unborn_is_refused`
- **Status:** proven
- **SPEC:** §4.4

### U-4 A merge that adds two names differing only in case writes one over the other on a case-insensitive work tree
- **Git:** with `core.ignorecase=true`, when one side adds `a.txt` and the other adds `A.txt`, ort merges cleanly and commits both names, then checks both out to the same file: the second overwrites the first without a message, and `status` shows the lost one as modified. The same holds for `cherry-pick`, `rebase`, `stash apply` and the autostash. `merge-tree` writes no work tree and is unaffected.
- **Ideal:** a merge does not lose file content in the work tree without saying so (`git-merge` documents that conflicts and blocked paths stop the merge before anything is overwritten).
- **Decision:** refuse (asc refuses the command before anything is written, naming both paths; `merge-tree` and a work tree with `core.ignorecase=false` behave as git)
- **Refusal:** error
- **Proof:** `crates/asc/tests/cli_merge_file_dir.rs::a_case_pair_on_a_case_insensitive_work_tree_is_refused_and_nothing_moves`
- **Status:** proven
- **SPEC:** §4.4
