# ASC repo scripts traces

ASC's scripts call git in three places only.

| Trace | Source | Invocation |
|---|---|---|
| `check-schema-instances` | `scripts/check-schema-instances.ts:68` | `git ls-files` |
| `pr-pack-forge` | `scripts/pr-pack.ts:53-65` | `git remote get-url origin`; `git credential fill` with the request on stdin |

ASC's `.git/hooks` hold Git LFS hooks; asc has no LFS (docs/CARRY.md), so they are not traced.

## Versions

monorepo at commit `dbeeef348`, git 2.55. The pin is in `../pins.toml`.

## Provenance

**Extracted**: the argv come from reading the scripts' source, and the fixture stands in for
ASC. `credential` is repository-independent, so the shim routes it to real git (docs/SPEC.md
§11.4); the trace marks that step `routes = "real"` and the runner expects it in the shim's
fall-through log instead of the native table.
