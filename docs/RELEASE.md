# Release engineering

How asc is built, signed, published and installed. The GitHub flow is described here
(`.github/workflows/release.yml`). The Forgejo flow (generic packages, MSI, systemd unit) is in
[`RELEASING.md`](RELEASING.md); both produce files that `asc self-update` accepts.

## What a release contains

A release is one flat directory. `<ver>` is `MAJOR.MINOR.PATCH`.

| File | Content |
| --- | --- |
| `asc-<ver>-<target>.tar.gz` | Linux and macOS archive: `asc`, `LICENSE-APACHE.txt`, `LICENSE-MIT.txt`. Read by `install.sh` and Homebrew. |
| `asc-<ver>-<target>.zip` | The same files for every target (`asc.exe` on Windows). Read by `install.ps1`, Scoop, winget, `asc self-update` and `asc upgrade`. |
| `asc-vscode-<ver>.vsix` | VS Code extension. |
| `asc-jetbrains-<ver>.zip` | JetBrains plugin (signed when the signing secrets exist). |
| `asc-<ver>.cdx.json` | CycloneDX 1.5 SBOM of the `asc` executable. |
| `asc.rb`, `asc.scoop.json`, `winget-asc.asc*.yaml` | Homebrew formula, Scoop manifest and winget manifests with this release's URLs and hashes. |
| `manifest.json` | The archives and packages, for download pages (below). |
| `SHA256SUMS` | SHA-256 of every other file except signatures. |
| `<file>.minisig` | minisign signature of each file above, including `SHA256SUMS` and `manifest.json`. |

Targets:

| Target | Built on | Notes |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Ubuntu 22.04 | Dynamic glibc build; needs glibc 2.35 or newer. `smoke.sh` fails the build if the executable needs a newer symbol version. No musl build is shipped. |
| `aarch64-unknown-linux-gnu` | Ubuntu 22.04, cross compiled | Same baseline; started under qemu-user for the version check. |
| `x86_64-apple-darwin` | macOS 15 (arm64 runner), cross compiled | Architecture checked with `lipo`; run only if Rosetta is present. |
| `aarch64-apple-darwin` | macOS 15 | |
| `x86_64-pc-windows-msvc` | Windows Server 2022 | Static C runtime. |

Every executable is built with `cargo build --release --locked -p asc`, stripped, and checked by
`scripts/release/smoke.sh`: `asc --version` must print the release version and the first 12 characters
of the tagged commit. `asc --version` reads, for example, `asc 1.2.3 (git 2.55.0, commit 0123456789ab)`.

## manifest.json

The file a download page reads. Schema 1:

```json
{
  "schema": 1,
  "name": "asc",
  "version": "1.2.3",
  "commit": "<40 hex>",
  "released_at": "2026-10-26T12:00:00Z",
  "files": [
    {
      "version": "1.2.3",
      "kind": "cli",
      "os": "linux",
      "arch": "x86_64",
      "target": "x86_64-unknown-linux-gnu",
      "format": "tar.gz",
      "file": "asc-1.2.3-x86_64-unknown-linux-gnu.tar.gz",
      "sha256": "<64 hex>",
      "size": 12345678,
      "signature": "asc-1.2.3-x86_64-unknown-linux-gnu.tar.gz.minisig"
    }
  ]
}
```

- `kind` is `cli`, `vscode`, `jetbrains`, `sbom`, `homebrew`, `scoop` or `winget`.
- For `cli`: `os` is `linux`, `macos` or `windows`; `arch` is `x86_64` or `aarch64`; `format` is `tar.gz` or
  `zip`. Other kinds have `os` and `arch` set to `any` and `target` set to `null`.
- `file` is a plain file name in the release directory. A reader builds the URL as
  `<ASC_DOWNLOAD_BASE_URL>/<version>/<file>`.
- `released_at` is the commit time of the tagged commit, not the build time, so the file is reproducible.
- The manifest does not list `SHA256SUMS`, itself, or signatures. `scripts/release/verify.py` checks it
  against `SHA256SUMS`, the files and the promised set of platforms.

## Download directory layout

`publish-to-hub.sh` writes, and the installers and `asc self-update` read, this layout under the directory
that `ASC_DOWNLOAD_BASE_URL` names:

```
<base>/latest/version          the newest version, one line
<base>/latest/manifest.json    that version's manifest.json (what a download page reads first)
<base>/<ver>/...               every file of the release
```

Any static file host serves it. The URL must be `https`; plain `http` is accepted only for the loopback
interface and only with `ASC_ALLOW_LOOPBACK_HTTP=1`.

## Cutting a release

1. Set `version` in `crates/asc/Cargo.toml`, run `cargo check --locked -p asc` so `Cargo.lock` follows, commit.
2. Merge. Wait for the `ci` workflow to pass on that commit.
3. `git tag vX.Y.Z && git push origin vX.Y.Z`. The tag must equal the crate version.
4. The workflow:
   - `prepare` stops unless the tag equals the crate version, the `ci` workflow succeeded for the commit, the
     variable `ASC_RELEASE_PUBKEY` and the secret `ASC_MINISIGN_KEY` exist, and the release script tests pass.
   - `build` makes the five executables and checks each one.
   - `plugins` builds the VS Code and JetBrains packages.
   - `assemble` builds the archives, SBOM, packaging manifests, `manifest.json` and `SHA256SUMS`, signs every
     file, and runs `verify.py` with the public key. A release that does not verify is not published.
   - `publish` creates the GitHub release as a draft, uploads every file, and publishes it last.
5. Mirror it (below).

A manual run of the workflow (`workflow_dispatch`) is a rehearsal: it does everything except create the
GitHub release, and keeps the assembled directory as the `release` workflow artifact for 30 days.

Repository configuration:

| Name | Kind | Meaning |
| --- | --- | --- |
| `ASC_RELEASE_PUBKEY` | variable | The minisign public key text (second line of the `.pub` file). Compiled into every executable for `asc self-update`, used by `verify.py`. |
| `ASC_MINISIGN_KEY` | secret | The matching secret key, unencrypted. Written to a mode 0600 file for the signing step and deleted. |
| `JB_CERTIFICATE_CHAIN`, `JB_PRIVATE_KEY`, `JB_PRIVATE_KEY_PASSWORD` | secrets | Optional. Sign the JetBrains zip; without them it is attached unsigned and the run says so. |

Making the key pair: `minisign -G -W -p asc-release.pub -s asc-release.key` (`-W` writes an unencrypted key).
Keep the secret key out of the repository. Changing the key means shipping a new release whose binaries carry the
new public key; installed copies verify updates against the key they were built with.

## Verifying a download

With the files of one release in a directory:

```sh
# 1. The signature of the checksum list (needs minisign and the release public key)
minisign -V -P "$ASC_RELEASE_PUBKEY" -m SHA256SUMS
# 2. The files against the list
sha256sum --check --ignore-missing SHA256SUMS        # macOS: shasum -a 256 --check
# Or all of it, including the manifest, the archive contents and every signature:
python3 scripts/release/verify.py . --pubkey "$ASC_RELEASE_PUBKEY"
```

The trusted comment of each signature is `file:<name> version:<ver>`, so a validly signed file cannot be passed
off as another file or version. `asc self-update` and `install.sh` check that. The signature proves the file came
from whoever holds the release key; a checksum fetched from the same host as the archive proves only that the
download is intact.

## Mirroring a release to a hub

```sh
gh release download vX.Y.Z --dir rel            # or: the workflow artifact `release`
sh scripts/release/publish-to-hub.sh --from rel --to /srv/asc-downloads --pubkey "$ASC_RELEASE_PUBKEY"
```

`publish-to-hub.sh` verifies the release first (checksums, manifest, archive contents and, with the key, every
signature; `--skip-signature-check` is the explicit opt-out), then copies it to `<to>/<ver>/` through a staging
directory. A version that exists is not overwritten without `--force`. `latest/` moves last and never to an older
version without `--force`; `--no-latest` publishes a back-port without moving it. `--dry-run` prints the plan.

## Installers

Both are non-interactive, have no default host, and require `ASC_DOWNLOAD_BASE_URL` (or `--base-url` /
`-BaseUrl`).

`scripts/install.sh` (POSIX sh; Linux and macOS):

| Option | Variable | Meaning |
| --- | --- | --- |
| `--base-url URL` | `ASC_DOWNLOAD_BASE_URL` | Release host directory. |
| `--version X.Y.Z` | `ASC_VERSION` | Pin a version instead of `latest/version`. |
| `--prefix DIR` | `ASC_INSTALL_PREFIX` | Default `${XDG_DATA_HOME:-~/.local/share}/asc-install` (not the asc data directory). |
| `--bin-dir DIR` | `ASC_BIN_DIR` | Default `~/.local/bin`. |
| `--dry-run` | | Print the plan (URL, hash, trust level, destinations); download no archive, write nothing. |
| | `ASC_RELEASE_PUBKEY` | Require a valid signature on `SHA256SUMS` from this key; needs `minisign`. |
| | `ASC_ALLOW_LOOPBACK_HTTP=1` | Allow `http://` for `127.0.0.1`, `localhost`, `[::1]`. |

It detects the OS and CPU (an x86_64 shell under Rosetta installs the native macOS build), reads
`SHA256SUMS`, checks the signature when a key is set, downloads the archive (size capped), checks its hash,
extracts only a regular file named `asc`, and installs
`<prefix>/<ver>/asc` with `<prefix>/current -> <ver>` and `<bin-dir>/asc -> <prefix>/current/asc`. This is the
layout `asc self-update` continues. It refuses to replace a `bin-dir/asc` it did not create, never edits a shell
profile, and prints a PATH hint when the bin directory is not on `PATH`. Installing a version that is already
there changes nothing.

`scripts/install.ps1` (Windows PowerShell 7 or Windows PowerShell 5.1) does the same with
`%LOCALAPPDATA%\Programs\asc\<ver>\asc.exe` (not the asc data directory `%LOCALAPPDATA%\asc`) and a `current` junction, and adds `<prefix>\current` to the user `PATH`
unless `-NoModifyPath` is given. Parameters: `-BaseUrl`, `-Version`, `-Prefix`, `-DryRun`, `-NoModifyPath`; the
same environment variables apply.

## Updating

`asc self-update [--check] [--from DIR]` (`asc upgrade` is the same command) reads `asc.releasesUrl`, else
`ASC_DOWNLOAD_BASE_URL`, else the directory given with `--from`; downloads `asc-<ver>-<target>.zip` and its
`.minisig`; verifies the signature against the key compiled in at build time (and that the trusted comment names
that file); unpacks beside the running version; switches `current`; and stops the old daemon. Everything it
fetches is size bounded. A build without `ASC_RELEASE_PUBKEY` cannot update.

It works for the layout the installers and the MSI make (`<prefix>/<ver>/asc`). An executable in any other place,
such as a Homebrew cellar, refuses and says so; update those with their package manager.

## Package managers

`assemble.py` renders the templates in `packaging/` with the release URL and the real hashes:

- Homebrew: `packaging/homebrew/asc.rb.in` -> `asc.rb`. Put it in a tap under `Formula/`.
- Scoop: `packaging/scoop/asc.json.in` -> `asc.scoop.json`; install with `scoop install ./asc.scoop.json` or
  publish it in a bucket as `asc.json`.
- winget: `packaging/winget/portable/*.in` -> `winget-asc.asc.yaml`, `winget-asc.asc.installer.yaml`,
  `winget-asc.asc.locale.en-GB.yaml` (a portable zip installer). Drop the `winget-` prefix when submitting to a
  source. The MSI manifests in `packaging/winget/` belong to the Forgejo flow.

The URLs point at the directory the release was assembled for: the GitHub release for a workflow run. To render
them for a hub instead, run `assemble.py` with `--release-url <base>/<ver>`.

## Reproducibility

A build is meant to give the same bytes for the same commit, toolchain and runner image:

- `--locked`, Rust 1.95.0 pinned by `rust-toolchain.toml` and the workflow, `CARGO_INCREMENTAL=0`,
  `codegen-units=1`, `strip=symbols`, no debug info;
- `--remap-path-prefix` for the workspace, `CARGO_HOME` and `RUSTUP_HOME`, so build paths are not embedded;
- `/Brepro` and a static C runtime on Windows;
- `ASC_BUILD_COMMIT` and `SOURCE_DATE_EPOCH` set from the tagged commit;
- archives written by `assemble.py` with fixed member order, modes, owners and timestamps, and a gzip header
  with no name or time. Running `assemble.py` twice on the same inputs gives identical files (tested).

Not pinned: the runner images and the system C toolchains they carry, which can change the C parts of the
executables. Compare hashes of two builds on the same image; the build job prints each executable's SHA-256.

## SBOM

`asc-<ver>.cdx.json` lists the crates reachable from the `asc` package in `Cargo.lock`, for every platform and
feature (so it is a superset of what one target links), with registry checksums, purls and the dependency graph.
It is deterministic for one lock file, version and commit.

## Tests

| Command | Covers |
| --- | --- |
| `python3 scripts/release/test/test_release.py` | `assemble.py`, `sbom.py`, `verify.py`: archives, manifest, determinism, SBOM, tamper cases. |
| `bash scripts/release/test/test-release.sh` | `smoke.sh`, `sign.sh`, `publish-to-hub.sh` end to end; signature cases when `minisign` is installed. |
| `bash scripts/release/test/test-install-sh.sh` | `install.sh` against a local fixture host: dry run, install, upgrade, tamper, bad URLs, foreign files, signatures. `TEST_SH=dash` runs it under dash. |
| `pwsh -NoProfile -File scripts/release/test/test-install-ps1.ps1` | `install.ps1` the same way, on Windows. |
| `cargo test -p asc --test self_update` | `asc self-update` and `asc --version` through the built binary. |

The `release-scripts` workflow runs them on Linux (with `minisign` and `shellcheck`), macOS and Windows.

## Editors

`plugins` builds `asc-vscode-<ver>.vsix` (`bun run package <ver>`, then the VSIX content check) and
`asc-jetbrains-<ver>.zip` (`buildPlugin`, and `signPlugin` when the signing secrets exist). Publishing them to a
marketplace stays in the `vscode-release` and `jetbrains-release` workflows (tags `vscode-v*` and
`jetbrains-v*`), which also run the IDE compatibility verification. The JetBrains build uses the Gradle wrapper
if `plugins/jetbrains/gradlew` is checked in, else the Gradle version the workflow installs.
