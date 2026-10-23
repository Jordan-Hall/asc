# Git quirks (Q-n)

Places where pinned git (v2.55.0) does something odd that git intends, and where the docs say something else or nothing. Method and grammar: `docs/git-fidelity.md`. Upstream bugs are in `git-defects.md`. Each entry has exactly one decision. A `refuse` or `idealize` entry also has a bullet under `## Upstream` in `divergences.md`; a `replicate` entry is not a divergence.

Seeded 2026-10-23 from the compat suite, `tests/compat/allowed.toml`, `docs/CARRY.md`, SPEC §11.4, §15.6 and the `asc` and `asc-core` tests. Counts: 8 quirks (replicate 3, refuse 4, idealize 1).

## Git quirks kept

Decision `replicate`: asc does what git does. Not divergences.

### Q-2 rev-parse echoes an option it does not know
- **Git:** `git rev-parse --no-such-flag` prints `--no-such-flag` and exits 0. Scripts depend on the echo, because `rev-parse` splits "flags meant for rev-list" from revisions.
- **Ideal:** `git-rev-parse(1) --flags` / `--no-flags`: unknown tokens that start with `-` are passed through as flags. An error for an unknown option is the common-sense reading; the docs describe the pass-through.
- **Decision:** replicate
- **Proof:** `tests/compat/scenarios/rev-parse-unknown-option.t` (to write)
- **Status:** pending Q01
- **SPEC:** §11.5 (stacks add no `rev-parse` flag because of this echo; the change id is read with `asc change-id`, §20.2)

### Q-4 ref-filter atoms that git accepts and prints empty
- **Git:** `for-each-ref --format` accepts spellings such as `%(author:x)`, whose modifier git ignores and which expand to an empty string, instead of rejecting them as it does an unknown atom.
- **Ideal:** `git-for-each-ref(1)` lists `authorname`, `authoremail`, `authordate` and their `:<mod>` forms only; an unrecognised modifier is an error in the clean reading.
- **Decision:** replicate (the output is parsed by scripts and tools; the atom table is compared byte for byte)
- **Proof:** `crates/asc/tests/cli_plumbing.rs::every_atom_matches_git`, `crates/asc/tests/cli_plumbing.rs::upstream_and_push_atoms_match_git`
- **Status:** proven
- **SPEC:** §11.1, §11.5

### Q-5 commits keep headers asc does not model
- **Git:** a commit object may carry `mergetag`, `gpgsig`, `encoding` and unknown headers, and git re-emits them byte for byte; the object id is the SHA-1 of those bytes.
- **Ideal:** the commit object description in `git-commit-tree(1)` and the user manual names `tree`, `parent`, `author` and `committer`, and `gitformat-signature(5)` covers `gpgsig`; further headers are not forbidden, and keeping them is the only way to keep the id.
- **Decision:** replicate (an on-disk and wire format: the id must not change on import or export)
- **Proof:** `crates/asc-core/src/git.rs` test `verify_commit_checks_gpgsig_and_asc`, `crates/asc-core/src/git.rs` test `strict_commit_grammar`
- **Status:** proven
- **SPEC:** §2.9, §14.1, CARRY "Import round-trip invariant"

## Quirks not kept

### Q-1 An external `git-<name>` runs before `alias.<name>`
- **Git:** with `alias.foo` set and an executable `git-foo` on `PATH`, `git foo` runs `git-foo`; the alias is shadowed.
- **Ideal:** `git-config(1)` on `alias.*`: an alias is the user's own shortcut and expands to the command it names. It says nothing about externals; the least surprising order is builtin, then alias, then external.
- **Decision:** idealize (command dispatch is purely local; no format involved)
- **Proof:** `tests/compat/scenarios/external-commands.t`, `tests/compat/allowed.toml` (the `external-commands` entry)
- **Status:** proven
- **SPEC:** §11.4

### Q-3 update-index --cacheinfo stages an object id that is not in the object database
- **Git:** `git update-index --add --cacheinfo 100644,<unknown id>,path` writes the index entry without checking the object.
- **Ideal:** `git-update-index(1)` describes `--cacheinfo` as registering "the given mode, object and path"; it does not promise the object exists, and `--info-only` exists for the unchecked case. An index entry naming a missing object is invalid state.
- **Decision:** refuse (asc stages only content it can read; exit 128, `invalid object`, with the id in the error)
- **Refusal:** error
- **Proof:** `crates/asc/tests/cli_plumbing.rs::update_index_matches_git` (the `ghost.txt` case)
- **Status:** proven
- **SPEC:** §11.5

### Q-6 git approves a credential taken from URL userinfo
- **Git:** after a successful request to `https://user:pass@host/...`, git calls the credential helpers with `store`, so the secret typed on a command line lands in the helper's store.
- **Ideal:** `gitcredentials(7)`: `approve` is for credentials a helper or a prompt produced. A secret from a URL is the caller's own and is not offered for storage.
- **Decision:** refuse (asc sends the Basic header and never calls `approve` for URL userinfo; this is an omission, so the test asserts "no helper calls")
- **Refusal:** omission
- **Proof:** `crates/asc/tests/auth_compat/main.rs::url_userinfo_is_basic_and_asc_never_approves_it`
- **Status:** proven
- **SPEC:** §15.6.1

### Q-7 `.git` aliases are refused by platform defaults only
- **Git:** `core.protectNTFS` and `core.protectHFS` decide which spellings of `.git` (`.git `, `.git.`, `GIT~1`, `.git::$INDEX_ALLOCATION`, HFS-ignorable code points such as U+200C inside `.git`) git refuses. The HFS protection is not on for every platform, so the same patch or tree is accepted on one platform and refused on another.
- **Ideal:** the safe reading of `git-config(1)` for both settings is "on, everywhere".
- **Decision:** refuse (every spelling is refused on every platform, up front, for patches, `am`, checkout and every ingest path; `checkout_safe` applies both protections always)
- **Refusal:** error
- **Proof:** `tests/compat/scenarios/apply-unsafe-asc.t`, `tests/compat/scenarios/apply-unsafe-paths.t`, `crates/asc-core/src/path.rs` test `checkout_safe_table`
- **Status:** proven
- **SPEC:** §2.8, CARRY "Patch apply paths"

### Q-8 tag objects with headers beyond `tagger`
- **Git:** a tag whose header block holds more than `object`, `type`, `tag` and `tagger` is read and re-emitted as is.
- **Ideal:** `git-mktag(1)`: the tag format is those four headers. Extra headers are not part of it.
- **Decision:** refuse (the structured parse, `parse_tag`, refuses an unknown header; import then keeps the tag as raw bytes, so its id is unchanged and the on-disk format is still replicated, CARRY "parse_tag")
- **Refusal:** error
- **Proof:** `crates/asc-core/src/git.rs` test `strict_tag_grammar_and_checks`, `crates/asc-core/src/git.rs` test `non_canonical_tag_keeps_raw`
- **Status:** proven
- **SPEC:** §2.9, §14.1, CARRY "canonicalise_tag"
