# Dependencies: scanning and update proposals

ASC reads the third-party dependencies of a repository in any of twelve ecosystems, reports the ones with
known vulnerabilities and the ones that are out of date, and proposes updates as ordinary changes. It is
self-hosted: the vulnerability source is OSV (the public API, a mirror you run, or a file you import), and
nothing depends on a vendor's service.

Everything is the `DepsService` (`proto/asc/v1/deps.proto`, feature `deps.v1`), the `asc deps` command and
the `[deps]` section of the server config. Settings of one repository are the file `.asc/deps.toml` on its
trunk.

## What it reads

Files are recognised by name and read by pure parsers (`asc-serve/src/deps/parse`). Nothing is executed, no
path or URL in a file is followed, and every reader is bounded: 32 MiB per file, 200,000 dependencies per
file, bounded nesting for JSON, TOML, XML and the restricted YAML of the lockfiles. A file that cannot be read
whole is reported with its error and contributes nothing, never a partial list.

| Ecosystem (OSV name) | Pins (exact versions) | Requirements and names |
|---|---|---|
| crates.io | `Cargo.lock` | `Cargo.toml` |
| npm | `package-lock.json` (v1-v3), `npm-shrinkwrap.json`, `yarn.lock` (v1 and Berry), `pnpm-lock.yaml` | `package.json` |
| PyPI | `requirements*.txt` (pins), `poetry.lock`, `uv.lock`, `Pipfile.lock` | `pyproject.toml`, `Pipfile`, ranges in `requirements*.txt` |
| Go | `go.mod` (replacements applied), `go.sum` (only without a `go.mod`) | |
| Maven | `pom.xml` (properties and `dependencyManagement` of the file), `gradle.lockfile` | |
| NuGet | `packages.lock.json` | `*.csproj`, `*.fsproj`, `*.vbproj` |
| Packagist | `composer.lock` | `composer.json` |
| RubyGems | `Gemfile.lock` | `Gemfile` |
| Hex | `mix.lock` | `mix.exs` |
| Pub | `pubspec.lock` | |
| SwiftURL | `Package.resolved` (v1-v3) | `Package.swift` |
| CocoaPods | `Podfile.lock` | |

Each dependency becomes `(ecosystem, name, version, direct or transitive, path)`. A manifest requirement is
read as its floor (`^1.2.3` is `1.2.3`) and counted only when no lockfile at or above its directory pins the
package; where a lockfile pins it, the manifest makes the pin *direct*. Entries that are not registry packages
(a path, a git source, a workspace member, a property that cannot be resolved) are left out and counted in the
`notes` of the file. `node_modules`, `.git`, `Pods`, and a `vendor` or `target` beside the manifest it belongs
to are not entered.

Property expansion in `pom.xml` (`${name}`) and MSBuild files (`$(Name)`) is bounded: no expanded string is
longer than 4 KiB, a file spends at most 10,000 substitutions, chains are at most 16 deep, and a property that
names itself is unresolved.

## Vulnerabilities

`Scan` asks the advisory source about every distinct package version:

* **OSV** (`[deps.osv] url`, default `https://api.osv.dev`): `POST /v1/querybatch`, up to 1,000 versions at a
  time, then `GET /v1/vulns/<id>`. A self-hosted mirror with the same API works the same way.
* **An advisory file** (`[deps] advisory_file`): the zip OSV publishes (one record per entry), a JSON array or
  JSON lines. It is evaluated locally (SEMVER and ECOSYSTEM ranges, explicit version lists; GIT ranges never
  match) and read again when it changes. With `[deps.osv] off = true` the server never calls OSV.
* With both, both have to answer, and their advisories are merged.

Answers are cached in the database: a package version for `osv_ttl_secs` (6 h), a record for `vuln_ttl_secs`
(24 h). The cache key is exactly the name sent to OSV, so spellings OSV tells apart never share an answer.

**A failed lookup is never a clean one.** A package whose advisories could not be had (the source failed, a
record could not be read, the answer did not match the question, the advisory file does not load) is counted in
`unknown_packages`, the scan says `LOOKUP_PARTIAL` or `LOOKUP_FAILED`, `asc deps scan` exits 2, and findings of
those packages are neither reported nor closed. A package whose ecosystem has no advisory data (CocoaPods) is
counted unknown too, but the lookup status is not failed. Names are asked in the spellings the ecosystem can
have (PyPI normalised, Packagist lower case, crates.io `-`/`_` and case variants, Hex and pub.dev lower case);
NuGet ids written without a capital letter, whose published casing cannot be derived, are unknown when nothing
is found.

Severity is the advisory's own label (`database_specific.severity`), else its CVSS v3 base score; an advisory
that states none counts as *moderate* wherever a threshold applies.

Findings of a scan of **trunk** are recorded (`ListFindings`): a finding keeps its id, first sighting and state
across scans, reopens if it comes back after being fixed, and becomes `FIXED` when a complete scan no longer
finds it. A scan of any other revision is reported and not recorded. `Dismiss` marks a finding dismissed with a
reason (`not-used`, `tolerable`, `inaccurate`, `fix-started`) and a note; a rescan keeps it dismissed.

## Updates

`ListUpdates` asks each registry (crates.io, npm, PyPI, the Go proxy, Maven Central, NuGet, Packagist,
RubyGems, Hex, pub.dev; SwiftURL and CocoaPods have no registry API) for the stable versions of every package
and shows, for each, the newest version, the version a proposal would pin, what that fixes and what it leaves
*unfixed*. Registry calls have their own timeout and size cap (`[deps.registries.<name>]`); a registry that
fails makes its packages `unknown_packages` with the reason, never "up to date".

* An ordinary update goes to the newest stable version whose step is at most `max_kind` (patch, minor or
  major; the repository's setting, `minor` by default). If only bigger ones exist the update is *blocked* and
  says what it would take.
* A security update (`--security`, or `security_only = true` in the settings) goes to the smallest version that
  no advisory with a fix still affects, judged by each advisory's own ranges: an advisory with a backport line
  affects versions of another release line above its first fix. The chosen version's own advisories are asked
  too, and up to five candidates are tried; whatever still affects the target is `unfixed`, never `fixes`.
* Versions a registry reports are used only if they are plain literals (letters, digits and `. + _ -`).

## Proposals

`ProposeUpdate` groups the updates (see below), rewrites the files of each group and records each group as one
changeset on `refs/heads/agent/deps/<group>-<hash>` through the normal commit path (the repository policy, the
secret scan and the audit log all apply). **It never writes trunk**; the ref is built from that prefix and a
slug. Landing is a person's decision, through review and the gate. The same moves name the same ref, so a
repeated run reports "already proposed".

The edit is minimal and needs no package manager. It changes the version text of the one package and what
the file records about that version, and nothing else; the result is read back with the file parsers before
it is kept:

* Requirements are bumped only if they do not already admit the new version, keeping the operator, the spacing
  and the number of components (`^1.2.3` to `^2.0.1`, `~> 1.2` to `~> 2.0`, `==3.2.0` to `==4.2.1`). Complex
  ones (`>=1, <2`, `1.x`, `||`) are refused with the reason, and the update is not made.
* Manifests and pin files that hold a version only are edited in place: `Cargo.toml`, `package.json`,
  `requirements*.txt` (exact pins without `--hash`), `pyproject.toml` (PEP 508 strings), `go.mod`, `pom.xml`
  (the version, the property that holds it, or the managed entry), `gradle.lockfile`, `*.csproj`,
  `composer.json`, `Gemfile`, `mix.exs`.
* **Lockfiles that record hashes are rewritten from the registry's metadata**, not by running the package
  manager: `Cargo.lock`, `package-lock.json` (versions 1 to 3), `yarn.lock` (version 1), `pnpm-lock.yaml`
  (versions 6 and 9), `poetry.lock` (1.x and 2.x), `uv.lock`, `Pipfile.lock`, `composer.lock`, `Gemfile.lock`
  and `go.sum`. See below.
* Lockfiles this code has no registry facts for stay with their manifest edit and a note: `packages.lock.json`,
  `mix.lock`, `pubspec.lock`, `Package.resolved`, `Podfile.lock`. A move whose only change would be one of
  them is skipped with that reason.
* Every value written comes from a registry or a file and is checked first: versions are plain literals,
  checksums are hex, integrity is `shaNNN-base64` of the length its algorithm gives, tarballs are https URLs
  without quotes or spaces.

### Lockfiles

A lockfile is rewritten when three things hold, and the update is not made (for the whole move, manifest
included, so a repository never ends with a manifest and a lock that disagree) when one does not:

1. **Every digest comes from the registry and was verified.** The new version's digests are asked of the
   registry for exactly that version and checked against a second path:

   | Ecosystem | Digests written | Checked against |
   |---|---|---|
   | crates.io | `checksum` | the sparse index (`index.crates.io`) must give the same `cksum` as the API |
   | npm | `resolved`, `integrity` (and the SHA-1 of yarn v1) | the registry's ECDSA signature of `name@version:integrity`, verified with the keys it publishes at `/-/npm/v1/keys`; a package without a signature, keys that cannot be fetched (not cached, asked again with backoff) and a signature no published key matches all refuse the update |
   | PyPI | every file's URL, SHA-256, size and upload time | the PEP 691 simple index must list every file with the same SHA-256 |
   | RubyGems | `sha256` in `CHECKSUMS` | the compact index (`/info/<gem>`) must give the same checksum as the API |
   | Packagist | the whole package entry (`source`, `dist`, `autoload`, `require`, ...) | |
   | Go | the `h1:` hashes of the zip and of `go.mod` | the checksum database's answer is believed only after its signed tree head verifies (Ed25519, `deps.go_sumdb_key`, the `golang.org/x/mod/sumdb/note` format) and the record is proven to be in that tree with the log's tiles; the hash computed from the module zip and `go.mod` the proxy serves must then equal the record |

   A registry that signs nothing (a private npm registry) can be used with `npm_unsigned = true`; what it gives
   is then listed in the proposal as `UNVERIFIED`, never as verified.

   A digest the registry did not give, one of the wrong length, or one the second path contradicts refuses
   the update for that package and says which. The old entry is checked too: a lock whose digest for the old
   version is not the registry's was not made from that registry, and nothing is derived from it.
2. **The new version declares the same dependencies as the one it replaces** (and, in the files that record
   them, the same engines, platforms and binaries). Then the rest of the lock stays what the package manager
   would write. A version that changes its dependencies needs the package manager's resolver; the refusal
   names the differences (`dependency dependencies:a changes from ^1 to ^2`).
3. **Every other package that requires this one still accepts the new version.** The ranges the lock records
   are used where it records them (`package-lock.json`, `yarn.lock`, `composer.lock`, `Gemfile.lock`,
   `poetry.lock`); for `Cargo.lock`, `pnpm-lock.yaml`, `uv.lock` and `Pipfile.lock` the registry is asked what
   each dependent declares. A dependent whose requirement cannot be evaluated, or does not admit the new
   version, stops the move and is named. At most 200 dependents are checked per package.

**What is written is checked first.** Registry answers are data, and a hostile registry or mirror must not
be able to put lines or fields of its choosing into a lockfile. Before anything is written, tarball and file
URLs have to be https URLs without a user name, fragment, quote, backslash, bracket, brace, `#` or white space,
on the registry's own host, a CDN it is known to use (`files.pythonhosted.org`; GitHub, GitLab and Bitbucket for
Composer) or a host listed in `artifact_hosts`; file names, hashes, integrity values, timestamps and Go hashes
have strict shapes (`sha512-<base64>`, 64 or 40 hex digits, an ISO timestamp, `[A-Za-z0-9._+-]` names,
`h1:<base64 of 32 bytes>`). A package that fails is refused with the reason. Each writer then escapes what it
writes for its own format (TOML, YAML flow mapping, JSON, yarn string) as well.

What each rewrite does:

* `Cargo.lock`: the version and `checksum` of the block, and the `"name version"` references to it.
* `package-lock.json`: version, `resolved` (on the host the lock already uses, so a mirror stays a mirror) and
  `integrity` of every installed copy, the legacy `dependencies` tree, and the requirement the root and the
  workspace members repeat.
* `yarn.lock` v1: version, `resolved#sha1` and `integrity` of the entry; the ranges in its header that the new
  version satisfies stay, the one the manifest replaced is renamed, the entry moves to its sorted place, and
  an entry already at the new version (which yarn would merge) is refused. **Yarn 2+ (Berry) is refused**: its
  checksum is of an archive yarn builds itself, which no registry publishes.
* `pnpm-lock.yaml`: the importers' `specifier` and `version`, the `packages` key and `resolution.integrity`,
  the `snapshots` key, every dependency map and every peer suffix (`react-dom@18.2.0(react@18.2.0)`) that names
  the version. lockfileVersion 5 (pnpm 7) is refused.
* `poetry.lock`: the version and the `files` list (or `[metadata.files]` before Poetry 1.5) in the style of
  the file; when `pyproject.toml` changed, the `content-hash`, after the same code has been shown to give the
  hash the lock holds for the *old* file (Poetry 1.x and 2.x algorithms). A source other than PyPI is refused.
* `uv.lock`: the version, `sdist` and `wheels` (wheels by file name, `upload-time` where the file has it), and
  the `specifier` the project's own `requires-dist` records when the move changed the requirement.
* `Pipfile.lock`: the version and the sorted hashes of every file; the `Pipfile` must already admit the new
  version, because it is not edited and the lock's own `_meta.hash` is of it.
* `composer.lock`: the package entry is replaced whole, in Composer's layout and key order, keeping the
  `notification-url` it had; when `composer.json` changed, the `content-hash`, again only after reproducing the
  old one.
* `Gemfile.lock`: the version, the `DEPENDENCIES` requirement and the `CHECKSUMS` line; a platform build
  (`nokogiri (1.15.0-x86_64-linux)`) is refused, because its checksum is another one.
* `go.sum`: the zip and `go.mod` hash lines of the new version, in `go`'s sort order, and the old zip line
  dropped (the old `go.mod` line stays, the module graph may still need it).

What was verified is listed in the proposal's `notes` (and its message), one line per check.

The rewriters were also run, outside the test suite, over real lockfiles (this project's neighbours: the
`npm` CLI, `yarn`, `gin`, `rails`, `symfony/demo`, `poetry`, `pipenv` and a `uv` example) moving every package
to the version and digests it already has; the result was the same text byte for byte in all but the cases
the rules above refuse.

A scheduled run is a workflow. `SetConfig` records `.asc/deps.toml` and, when there is a `schedule`,
`.asc/workflows/deps.toml` (a schedule trigger, a manual trigger, and one command step that runs
`asc deps propose --remote https://<server>/<owner>/<repo> --token-env DEPS_TOKEN` with the repository secret
`deps-token`; its egress is this server only) on `refs/heads/agent/deps/config-<hash>`, to be reviewed and
landed. The operator allows `asc` in `[workflows.sandbox]`, a maintainer sets the secret to a token that can
write to the repository and switches the workflow on. Removing the schedule removes the generated workflow (a
workflow file you wrote yourself is left alone).

## `.asc/deps.toml`

Read from the **trunk tip**, never from the revision under review or a change being judged. Unknown keys are
errors; an invalid file refuses `ProposeUpdate` (a scan uses the defaults and `GetConfig` reports the error).

```toml
version = 1
ecosystems = ["cargo", "npm"]    # OSV names or aliases; empty: every ecosystem found
schedule = "0 4 * * 1"           # five-field cron, UTC, at most once an hour
security_only = false            # propose only updates that fix an open advisory
severity_threshold = "high"      # advisories below this are reported but not proposed for
max_kind = "minor"               # patch | minor | major
max_proposals = 5                # changes one run records, at most 50
grouping = "ecosystem"           # none (one per package, the default) | ecosystem | kind | all

[[group]]                        # checked before `grouping`
name = "lint-tools"
ecosystem = "npm"
patterns = ["eslint*", "prettier"]
kinds = ["patch", "minor"]

[[ignore]]
ecosystem = "npm"
name = "left-pad"                # glob, `*` matches any run of characters
kinds = ["major"]                # update kinds not proposed; empty and no advisories: no update at all
advisories = ["GHSA-xxxx"]       # advisories not reported for this package
until = "2027-01-01"             # the entry lapses after this day (UTC)
reason = "pinned by the vendor"
```

## Blocking a landing

`.asc/policy.toml`:

```toml
[deps]
block_severity = "high"   # low | moderate | high | critical; absent: the policy never blocks on advisories
block_unknown = true      # default: refuse when the advisory lookup failed
```

An update of a protected ref that **changes a dependency file** is checked: the packages it adds (present in
the new file and not in the old one) are looked up, and the update is refused with `POLICY_DENIED` if any has
an advisory at or above `block_severity` that is not dismissed, or (with `block_unknown`) if the lookup failed
or timed out (`gate_timeout_secs`). It lists `ecosystem name version advisory (severity)`. More than 200
dependency files or 2,000 new versions in one update are refused as too many to check. If the scanner is off on
the server (`[deps] enabled = false`) such an update is refused too, because it cannot be checked. Updates that
touch no dependency file ask nothing of the network.

## `asc deps`

```text
asc deps scan [--rev REV] [--ecosystem E]... [--refresh]     # exit 1: findings, 2: not every lookup answered
asc deps findings [--state S]... [--severity S] [--package NAME] [--limit N] [--page TOKEN]
asc deps updates [--rev REV] [--security] [--ignored]
asc deps propose [--security] [--max-kind K] [--max-groups N] [--dry-run] [ecosystem:name]...
asc deps dismiss <id> --reason not-used|tolerable|inaccurate|fix-started [--note TEXT]
asc deps reopen <id>
asc deps config get [--rev REV]
asc deps config set [--ecosystem E]... [--all-ecosystems] [--schedule CRON | --no-schedule]
                    [--security-only BOOL] [--severity-threshold S|none] [--max-kind K] [--max-proposals N]
                    [--grouping G] [--group SPEC]... [--no-groups] [--ignore SPEC]... [--no-ignore] [--dry-run]
```

`config set` reads the settings in force from trunk, changes what the flags name and proposes the result as
`.asc/deps.toml` (and the workflow, when there is a schedule) on an `agent/deps/config-...` ref. A group is
`NAME=ECOSYSTEM:PATTERN[,PATTERN][:KIND[,KIND]]` (`*` for any ecosystem); an ignore rule is
`ECOSYSTEM:NAME[:KIND[,KIND]][:until=YYYY-MM-DD][:reason=TEXT]`, where the reason runs to the end of the text.
`--group` and `--ignore` replace the lists; `--no-groups` and `--no-ignore` empty them.

All take `--remote` (a configured remote or the repository URL) and `--json`. `--token-env NAME` takes the
credential from the environment variable `NAME` and needs `--remote` to be an https URL; it is for scheduled
workflows, whose secrets cannot be named `ASC_*`.

## Who may call

| Call | Needs |
|---|---|
| `ListFindings`, `ListUpdates`, `GetConfig` | read |
| `Scan`, `ProposeUpdate`, `SetConfig`, `Dismiss` | write, and a maintainer of the policy or an administrator |

Agent credentials reach none of them (every service an agent token is not named for is closed to it).

## Server config

See `deploy/asc-serve.toml.example`. Every URL is judged by the network guard (https only unless the operator's
loopback opt-in is on; no loopback, private, link-local or metadata address; no credentials in the URL) and
must name a host the egress allowlist admits (by default, the hosts of the configured URLs). A mirror that
lives on a private address is not reachable for the same reason bring-your-own storage is not: put it behind a
public name, or import its data with `advisory_file`. Redirects are never followed.

Besides the registries of the table above, a lockfile hash is checked against a second source, each its own
endpoint (`[deps.registries.crates_index]`, `https://index.crates.io`, and `[deps.registries.go_sumdb]`,
`https://sum.golang.org`; PyPI's simple index and RubyGems' compact index are on the hosts of `pypi` and
`rubygems`). `go_sumdb_key` is the verifier key of the checksum database (`sum.golang.org` by default; a
private database's key goes here), `npm_unsigned` allows a registry that does not sign its packages, and
`artifact_hosts` lists hosts besides the registry's own that artifact URLs may name. `archive_max_bytes` (128 MiB) and `archive_timeout_secs` (120) bound the one download a check
reads, a Go module zip; a larger module cannot be verified and its `go.sum` is not rewritten. Switching an
endpoint off (`off = true`) switches off the rewrites that need it, never the check.
