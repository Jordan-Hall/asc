# Git defects (U-n)

Upstream bugs in pinned git (v2.55.0) or in a library it uses: behaviour nobody intends and the docs do not describe. Method and grammar: `docs/git-fidelity.md`. Odd-but-intended behaviour is in `git-quirks.md`. Ids carry a hyphen (`U-1`) because PLAN's `U01` to `U89` are units. Each entry has exactly one decision; a `refuse` or `idealize` entry also has a bullet under `## Upstream` in `divergences.md`.

Seeded 2026-10-23. Counts: 2 defects (replicate 0, refuse 2, idealize 0). Nothing here has been reported upstream; a report, when made, goes in the entry as `- **Upstream:** <link>`.

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
