# Naming

The product is **ASC**, Agent Source Control: source control for people and their agents. The hub is **ASC Hub**.

| Surface | Name |
|---|---|
| Command line tool and the one binary | `asc` (`asc.exe` on Windows) |
| Rust crates | `asc`, `asc-core`, `asc-store`, `asc-engine`, `asc-serve`, ... under `crates/asc*` |
| npm packages | `@asc/asc-client`, `@asc/asc-ui`, ... under `packages/asc-*` |
| Protocol package | `asc.v1` (directory `proto/asc/v1`) |
| Environment variables | `ASC_*` (for example `ASC_HOME`, `ASC_TOKEN`) |
| Per-repository files | `.asc/` (policy, projects, CI, review), `.ascignore`, `.ascattributes` |
| Home directory | the platform data directory under the name `asc`, overridden by `ASC_HOME` |
| Hidden refs | `refs/asc/` |
| Git configuration section | `asc.*` |
| Push options | `asc.ttl`, `asc.push.review` and the other `asc.*` options |
| Discovery document | `/.well-known/asc` |
| Container images | `asc-serve`, `asc-hub` |
| SQL objects | tables and columns prefixed `asc_`; the database role and schema are named `asc_hub` because `asc` is a reserved word in SQL |
| User agent | `asc/<version>` |
| Token prefixes | `asp_` (personal), `asd_` (deploy), `asc_` (CI) |

Stored-format identifiers carry the new name too, so data written under any earlier name is not readable. The envelope magic is the two bytes `as`; the 4 byte magics are `ASC1` (git hook), `ASVS` and `ASVL` (vector files). Staging data is disposable and is wiped when moving to this release.

Host names in committed deployment files are placeholders under `example.org`. Real host names belong in the operator's environment (`ASC_*` variables, repository variables).
