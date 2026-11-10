# Divergences (D-n)

Every place where asc differs from pinned git (v2.55.0). Method and grammar: `docs/git-fidelity.md`. A decision to keep a git quirk (`replicate`) is not a divergence and is not listed. Differences caused by the test harness are not listed either; they are adapter gaps (`adapter-gaps.md`).

Each divergence has exactly one **class**:

| Class | Meaning |
|---|---|
| `upstream` | caused by a `refuse` or `idealize` decision on a quirk or defect. Only under `## Upstream`. |
| `workspace-gitdir` | caused by the synthetic gitdir of an asc workspace (SPEC §9.4). |
| `auth` | asc authenticates differently from git on purpose (SPEC §15.6). |
| `branchless` | a remote effect of branchless mode (SPEC §20.1 D5, §21.1 BD7, §21.4.2). |

Seeded 2026-10-23. Counts: upstream 8 (refuse 7, idealize 1), own 6 (`workspace-gitdir` 3, `auth` 1, `branchless` 2; D-7 retired).

## Upstream

One bullet per `refuse` or `idealize` decision. The lint (Q01) checks that each cited id exists and has that decision, and that every such decision has a bullet.

- **Q-1** (idealize) `git foo` runs the alias `alias.foo` before an external `git-foo` on `PATH`; git 2.55 runs the external first. Expected line drop: `tests/compat/allowed.toml` entry `external-commands`. SPEC §11.4.
- **Q-3** (refuse) `update-index --cacheinfo` with an object id asc has never seen exits 128 with `invalid object ... (git-fidelity Q-3)`; git writes the entry. SPEC §11.5.
- **Q-6** (refuse) asc never calls `approve` (credential `store`) for a credential taken from URL userinfo; git does. No error is printed (it is an omission); the test asserts no helper calls. SPEC §15.6.1.
- **Q-7** (refuse) Every spelling of `.git` is refused on every platform; git refuses the HFS-ignorable spellings only where `core.protectHFS` is on. The error names the path and `(git-fidelity Q-7)`: from `asc apply` for an HFS-ignorable spelling (git refuses the NTFS spellings too, with the same message, so no note there) and from ingest. SPEC §2.8.
- **Q-8** (refuse) `parse_tag` refuses a tag with a header beyond `tagger`; import keeps the raw tag, so the id is unchanged. Error `(git-fidelity Q-8)`. SPEC §2.9.
- **U-1** (refuse) asc forwards no `http.extraHeader` to another origin after a redirect; libcurl forwards the non-sensitive ones. Omission, asserted by test. SPEC §15.6.4 item 3.
- **U-2** (refuse) A patch that names a `.git` alias only as the source of a rename, deletion or mode change is refused up front as unsafe; git reports a missing file. Error `(git-fidelity U-2)`, from `asc apply`. SPEC §2.8.

- **U-3** (refuse) `cherry-pick --abort` or `revert --abort` of a sequence started on an unborn branch exits 128 with `cannot abort a cherry-pick started on an unborn branch (git-fidelity U-3)` and changes nothing; git fails to read its own record, or moves the branch to the commit it was picking. SPEC §4.4.

- **U-4** (refuse) With `core.ignorecase=true`, a `merge`, `cherry-pick`, `revert`, `rebase` or `stash apply` whose sides add two names that differ only in case exits 128 with `'a.txt' and 'A.txt' differ only in case and this work tree cannot hold both (core.ignorecase); nothing was changed (git-fidelity U-4)` and changes nothing; git commits both and silently writes one file over the other. SPEC §4.4.

Q-2, Q-4 and Q-5 are `replicate` and have no bullet.

## Own divergences

### D-1 The commit message path a hook receives
- **Class:** workspace-gitdir
- **Pinned git:** `prepare-commit-msg` gets `.git/COMMIT_EDITMSG` as its message file.
- **asc:** a workspace's `.git` is a gitfile and the message file lives in the synthetic gitdir, so the hook gets that absolute path.
- **Expected difference:** `tests/compat/allowed.toml` entry `hooks`, stderr, `prepare-commit-msg ... COMMIT_EDITMSG`.
- **Proof:** `tests/compat/scenarios/hooks.t`
- **Status:** proven
- **SPEC:** §9.4.1

### D-2 The index path a pre-commit hook receives
- **Class:** workspace-gitdir
- **Pinned git:** for a normal commit `pre-commit` runs with `GIT_INDEX_FILE=.git/index`.
- **asc:** the workspace's index is the synthetic one in its gitdir, so the hook gets that absolute path.
- **Expected difference:** `tests/compat/allowed.toml` entry `hooks`, stderr, `hook pre-commit index:`.
- **Proof:** `tests/compat/scenarios/hooks.t`
- **Status:** proven
- **SPEC:** §9.4.1

### D-3 The `core.*` keys of the synthetic gitdir's config
- **Class:** workspace-gitdir
- **Pinned git:** `git init` writes `repositoryformatversion`, `filemode`, `bare`, `logallrefupdates`, `symlinks`, `ignorecase`, `autocrlf`, `longpaths` in its own set and order.
- **asc:** `asc init` writes the synthetic gitdir's config with its own `core.*` set and order. User keys (alias, includes) are compared.
- **Expected difference:** `tests/compat/allowed.toml` entry `config-alias-includeif`, stdout.
- **Proof:** `tests/compat/scenarios/config-alias-includeif.t`
- **Status:** proven
- **SPEC:** §9.4

### D-4 A refused credential is asked for once more
- **Class:** auth
- **Pinned git:** a credential the host refuses is erased and git stops with `Authentication failed`.
- **asc:** non-interactive, one re-ask: asc calls the helper `get` once more without a prompt, erases that credential too and ends with git's own message. The extra `get` is the difference.
- **Proof:** `crates/asc/tests/auth_compat/main.rs::a_refused_credential_is_erased_and_the_failure_reads_like_gits`
- **Status:** proven
- **SPEC:** §15.6.1

### D-5 Pushing a tag that is not shared is rejected in branchless mode
- **Class:** branchless
- **Pinned git:** `git push origin <tag>` creates the tag on the remote.
- **asc (`asc.branchless=true`, the default):** `! [remote rejected] <tag> -> <tag> (asc: ref not shared in this repository)`, exit 1. `asc.branchless=false` or `repos.branchless=false` restores git's behaviour.
- **Proof:** `tests/compat/scenarios/branchless-push-tag.t` runs in both modes against a bare git remote (no ref policy) and must match git byte for byte, which proves the `asc.branchless=false` half and BD7 for the local commands. The rejection is server policy (`REF_NOT_SHARED`, `crates/asc-serve/tests/refpolicy.rs`); the client maps it to the printed line, and the other refs of the same atomic update show `(atomic push failed)`: `crates/asc/tests/branchless_push.rs` (`a_tag_outside_the_shared_refs_is_rejected_and_nothing_moves`). The compat runs use a bare git remote, so no `allowed.toml` entry is needed.
- **Status:** proven
- **SPEC:** §21.4.2, §20.1 D5, §21.1 BD7

### D-6 `push --all` does not submit stacks in branchless mode
- **Class:** branchless
- **Pinned git:** `git push --all` pushes every local branch as a server branch.
- **asc (branchless):** each non-shared branch becomes a user ref; shared-pattern branches follow their compare-and-swap rules; no `refs/for` submit, no PR; the output ends with `remote: asc: --all does not submit stacks; use asc submit`.
- **Proof:** `tests/compat/scenarios/branchless-push-all.t` runs in both modes against a bare git remote and must match git byte for byte, which proves the `asc.branchless=false` half and BD7 for the local commands. The user-ref mapping is server policy; the client prints the last line on stderr when it is branchless and the server says the repo is, and not with `-q`, `--dry-run` or nothing to send: `crates/asc/tests/branchless_push.rs` (`push_all_ends_with_the_no_submit_line_only_in_branchless_mode`). The compat runs use a bare git remote, so no `allowed.toml` entry is needed.
- **Status:** proven
- **SPEC:** §21.4.2

### D-7 A hook installed through `rev-parse --absolute-git-dir` is not found
- **Class:** workspace-gitdir
- **Pinned git:** `rev-parse --absolute-git-dir` is the directory git reads hooks from; a script writing `<it>/hooks/post-checkout` is run.
- **asc:** the path is the synthetic `wc/<id>/git`. asc now looks for hooks under the main workspace's synthetic gitdir before `<common>/hooks` (unless `core.hooksPath` is set), and `rev-parse --show-toplevel` and the hook's working directory name a nested worktree's requested path, so the hook fires as in git. No difference remains.
- **Proof:** `tests/traces/claude-code/worktree-git-hooks.trace`, `crates/asc/tests/cli_worktree_hooks.rs`
- **Status:** retired
- **SPEC:** §9.4, §9.4.1
