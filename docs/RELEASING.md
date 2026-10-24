# Releasing asc

Source of truth: SPEC 16.2, `.forgejo/workflows/release.yml`, `build/release/local-release.sh`.

## What a release contains

Published as Forgejo generic packages at `https://forge.example.org/api/packages/asc/generic/asc`
(`asc.releasesUrl`):

| Path | Content |
| --- | --- |
| `<ver>/asc-<ver>-x86_64-pc-windows-msvc.zip` and `.msi` | Windows. The zip is what `asc upgrade` installs; the MSI is for people and winget. |
| `<ver>/asc-<ver>-x86_64-unknown-linux-gnu.zip` and `.tar.gz` | Linux. The zip is for `asc upgrade`; the tarball is for `install.sh`. |
| `<ver>/install.sh`, `<ver>/asc-daemon.service` | Linux installer (the release public key is baked in) and systemd user unit. |
| `<ver>/winget/*.yaml` | winget manifests. |
| `<ver>/<file>.minisig` | minisign signature; its trusted comment is `file:<name> version:<ver>`. |
| `latest/version` | The newest version, one line. Moves last. |

Only the two x86_64 targets ship now. `aarch64-unknown-linux-gnu` and `aarch64-apple-darwin` are defined in a
manual-only job group (`extra-build`, `extra-publish`) because no runner with those labels is registered; a tag
push would queue jobs that never start. They run only through workflow_dispatch with `extra_targets` and
`version`, and they never move `latest/version`.

## Release steps (CI)

1. Bump the version: set `version` in `crates/asc/Cargo.toml` (or the workspace version it inherits), run
   `cargo check --locked -p asc` so `Cargo.lock` is updated, and commit both.
2. Merge to `master`. Tag it: `git tag vX.Y.Z && git push origin vX.Y.Z`. The tag must equal the crate
   version or the build jobs fail at the first step.
3. The workflow runs two independent chains:
   - Linux: `gate-linux` -> `build-linux` -> `publish-linux` (Linux runner).
   - Windows: `gate-windows` -> `build-windows` -> `publish-windows` (Windows host runner).
   Each chain uploads its own files when it finishes. A down Windows runner does not block the Linux files.
4. `publish-latest` moves `latest/version` only when both chains succeeded. If Windows was not available, the
   Linux files are live under `<ver>/` but `asc upgrade` does not see the release yet. Re-run the failed
   Windows jobs when the runner is back, or release Windows by hand (below).

Required Forgejo configuration: variable `ASC_RELEASE_PUBKEY` (minisign public key text), secrets
`ASC_MINISIGN_KEY` (the secret key) and `REGISTRY_TOKEN` (package write). The token is passed to curl through a
0600 header file, never on a command line.

## Registering the Windows host runner

The Windows jobs need a Forgejo runner on a Windows machine (the Windows 11 host is fine) with Visual Studio
Build Tools (MSVC), WiX Toolset 3 (for `cargo-wix`), Git for Windows (provides bash) and Python 3.

1. Install `forgejo-runner` for Windows and register it against the Forgejo instance with a registration token for
   the ASC organisation (Settings -> Actions -> Runners).
2. Give it exactly this label: `asc-build-trusted-windows-latest:host`. The `:host` scheme runs jobs directly on
   the machine, with no container, because the MSI check installs software and edits the machine `PATH`.
3. Start it as a service under a dedicated local account with no access to personal files.

Security note: a host runner executes workflow code with the privileges of its account, and the MSI step runs
`msiexec` with `USE_ASC_SHIM=1`, which changes the machine `PATH`. Use the `asc-build-trusted-` label only for
workflows that run on trusted pushes and tags of this repository. Never put the label on a job that runs for
pull requests or forks, and never register the runner for other repositories. Prefer a dedicated machine or
VM; at minimum keep the account separate and the runner off whenever you do not release. The Linux runner
(`asc-build-trusted`, `asc-build-trusted-ubuntu-latest`) follows the same rule.

## Releasing by hand (without CI)

`build/release/local-release.sh` builds, packages, checksums, signs and uploads. It builds Linux in WSL (or on
Linux) and Windows in Git Bash on Windows, with Rust 1.95.0, and writes to `E:/asct/release/<ver>`. Build files
go to `E:/asct/release-target`.

```
bash build/release/local-release.sh --dry-run                # print the plan, change nothing
bash build/release/local-release.sh --no-upload              # build, checksum, sign; stop before upload
ASC_RELEASE_KEY_FILE=/path/to/minisign.key \
ASC_RELEASE_PUBKEY='RWQ...' \
ASC_RELEASE_TOKEN=... \
  bash build/release/local-release.sh                        # everything
```

- `ASC_RELEASE_KEY_FILE`: path of the minisign secret key. Without it nothing is signed, and `asc upgrade` and
  `install.sh` refuse unsigned files. The key is read by minisign only, never printed or copied. On Windows, if
  `minisign` is not installed, it is run inside WSL.
- `ASC_RELEASE_TOKEN`: Forgejo token. It is written to a 0600 temporary header file for curl and deleted on exit.
- `--only linux|windows` builds one platform. `--latest-only` moves `latest/version` for files already in the
  output directory.
- `latest/version` moves only when both zips are in the output directory, and always last.
- The script also writes `SHA256SUMS` (and its signature) for checking by hand; CI does not publish it.

## How clients find releases

- `asc upgrade` and the daily daemon check read `<asc.releasesUrl>/latest/version`, then download
  `<ver>/asc-<ver>-<target>.zip` and `<ver>/asc-<ver>-<target>.zip.minisig`. It verifies the signature with the
  embedded release key (including the file name in the trusted comment) before it unpacks into
  `<prefix>/<ver>` and switches `<prefix>/current`.
- `install.sh` (Linux) takes a downloaded tarball and signature:
  `install.sh --tarball asc-<ver>-x86_64-unknown-linux-gnu.tar.gz [--sig <file>] [--prefix <dir>] [--shim] [--no-service]`.
  It verifies the signature with the key baked in at release time and refuses to run without `minisign` or a
  key. Download `install.sh`, the tarball and the `.minisig` from `<releasesUrl>/<ver>/`.
- winget uses the manifests in `<ver>/winget/`; the MSI hash in them is computed at publish time.

## Checks before tagging

- `cargo test -p asc --test compat` and `cargo test -p asc upgrade::` pass (the gate runs them).
- `bash -n build/release/local-release.sh` and `bash build/release/local-release.sh --dry-run` succeed.
