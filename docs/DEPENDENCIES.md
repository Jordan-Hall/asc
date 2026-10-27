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

The edit is minimal and needs no package manager. It changes the version text of the one package and nothing
else, and the result is read back with the file parsers before it is kept:

* Requirements are bumped only if they do not already admit the new version, keeping the operator, the spacing
  and the number of components (`^1.2.3` to `^2.0.1`, `~> 1.2` to `~> 2.0`, `==3.2.0` to `==4.2.1`). Complex
  ones (`>=1, <2`, `1.x`, `||`) are refused with the reason.
* Rewritten: `Cargo.toml`, `Cargo.lock` (version and registry checksum), `package.json`, `package-lock.json`
  (versions 2 and 3: version, tarball and integrity of every copy, and the requirement the root repeats),
  `requirements*.txt` (exact pins without `--hash`), `pyproject.toml` (PEP 508 strings), `go.mod`, `pom.xml`
  (the version, the property that holds it, or the managed entry), `gradle.lockfile`, `*.csproj`,
  `composer.json`, `Gemfile`, `Gemfile.lock`, `mix.exs`.
* Never rewritten, because they hold hashes of a whole graph or one entry per range: `yarn.lock`,
  `pnpm-lock.yaml`, `poetry.lock`, `uv.lock`, `Pipfile.lock`, `packages.lock.json`, `composer.lock`,
  `mix.lock`, `pubspec.lock`, `Package.resolved`, `Podfile.lock`, `go.sum`. When the manifest moves and one of
  these is left, the proposal says so in its `notes` (and its message): a resolver still has to run. When only
  such a file would change, the update is skipped with that reason.
* Dependencies of the new version that the old one did not have are not added to a lockfile; the notes name the
  command (`cargo update -p NAME`, `npm install`, `bundle install`, `go mod tidy`).
* Every value written comes from a registry or a file and is checked first: versions are plain literals,
  checksums are hex, integrity is `shaNNN-base64`, tarballs are https URLs without quotes or spaces.

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
asc deps config [--rev REV]
```

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
