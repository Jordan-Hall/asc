# Getting started with asc

Install asc, sign in to a server, clone, and work in stacks. Common tasks and their commands are in
[`CHEATSHEET.md`](CHEATSHEET.md); `asc doctor` is in [`ONBOARDING.md`](ONBOARDING.md).
[`SPEC.md`](SPEC.md) wins where this page disagrees.

## 1. Install

Release files are served from a download directory that holds `latest/version` and `<ver>/...`; set
`ASC_DOWNLOAD_BASE_URL` to its URL (your ASC Hub or any static host). See [`RELEASE.md`](RELEASE.md).

| Platform | How |
|---|---|
| Linux, macOS (x86_64, aarch64) | `sh scripts/install.sh` installs to `~/.local/share/asc-install` and links `~/.local/bin/asc`. `--dry-run` shows the plan. |
| Windows (x86_64) | `./scripts/install.ps1` installs to `%LOCALAPPDATA%\Programs\asc` and adds it to the user `PATH`. Or run the MSI of the Forgejo flow, or use the winget, Scoop or Homebrew manifests of a release. |
| Linux, from a downloaded tarball | `sh install.sh --tarball <file> [--shim] [--no-service]` from `packaging/linux/` (needs `minisign`; installs the systemd user unit). |
| From source | Rust 1.95: `cargo build --release -p asc`. |

Set `ASC_RELEASE_PUBKEY` to the release's minisign public key to make the installers require a signature.

Then run `asc doctor`; `asc self-update` (or `asc upgrade`) updates an install made by the installers.

## 2. Authenticate

`asc auth login --host <url> --token-stdin` stores a token with the system credential helpers (the token
is read from standard input, never from an argument), `asc auth logout --host <url>` forgets it, and
`asc auth status [--json] [<url>]` reports which source would supply a credential for a URL and never
prints the secret. Credentials come, in this order, from the URL, the credential helpers, and the
`ASC_TOKEN` environment variable (sent only to the remote's own origin; see
`crates/asc/src/credential.rs`).

1. Get a token from the server administrator. On an `asc serve` with the token store the
   administrator mints it with `asc serve token create --user <name> --repo '<owner>/*' --expires <duration>`
   (add `--admin` for an admin). The value is printed once.
2. Give it to asc either way:
   - `export ASC_TOKEN=<token>`, or
   - store it: `printf '%s' "$TOKEN" | asc auth login --host https://asc-staging.example.org --token-stdin`
3. Check: `asc auth status https://asc-staging.example.org`.

Never put a token in a URL. asc does not store a credential that came from URL userinfo
(divergence Q-6 in [`divergences.md`](divergences.md)). Nothing prompts.

## 3. Clone

```sh
asc clone https://asc-staging.example.org/<owner>/<repo>
```

The clone is lazy: file contents download when first read (`--mode plain` writes real files).
`asc clone <url> <dir> --reference <old clone>` borrows objects from an existing clone. The default
branch is `master`.

## 4. The daily loop

```sh
asc status
asc add -p                       # or: asc add <paths>
asc create -m "add parser"       # a new change on HEAD from the staged changes
asc create -m "use parser"       # a second change stacked on the first
asc sl                           # show the stack
asc modify                       # amend the current change; children are restacked
asc absorb                       # fold staged hunks into the changes that own those lines
asc submit -s                    # send the whole stack up as draft changes (one UpdateRefs)
asc submit --publish             # make the PRs reviewable (flags: --draft, --reviewer, --auto-merge)
asc sync                         # fetch, fast-forward trunk, drop landed changes, restack
asc undo                         # undo the last operation (asc op log shows the operation log)
```

Commands that exist beyond these are in `asc --help` and the `crates/asc/src/stack/cli/`
sources (`restack`, `fold`, `split`, `squash`, `reorder`, `reword`, `abandon`, `stack get`, ...).
In a branchless repository (the default) only `master`, release branches and tags are shared
refs; `asc push origin feat` submits the stack as draft changes and `feat` becomes your user ref.
`asc submit` needs an ASC server and exits 128 with an explanation against anything else.

`asc push --land` (or `-o land`) asks the server to land onto `master` when paths do not conflict.

## 5. Git compatibility

You do not need git to use ASC. For existing tools and CI that still speak git, `asc serve`
offers a compatibility layer, and an optional `git` shim (`asc install-shim`, see
[`ONBOARDING.md`](ONBOARDING.md)). Git 2.40 or later must be installed for the layer and the shim.

- **Smart HTTP (https):** `git clone https://asc-staging.example.org/<owner>/<repo>.git`.
  The server answers 401, then git asks its credential helper (same token as above, as the
  password; use a helper or `http.extraHeader`, never the URL). Pushes go through the same
  checks as `asc push`. With `[git] enabled = false` the git routes answer 403 with `X-Asc-Git`.
  - **A token alone in the URL fails.** `git clone https://TOKEN@host/...` gets a 401 on git 2.55
    with libcurl 8.5: git takes the token as the user name, asks for a password, and an empty
    askpass answer makes libcurl send no credentials at all. These forms work:
    `https://TOKEN:@host/...` (an empty password after the colon), a credential helper that
    answers with the token, or `git -c http.proactiveAuth=basic clone https://TOKEN@host/...`
    (git 2.46 or later). A token in a URL ends up in shell history and in `.git/config`, so
    prefer the helper; the colon form is for CI, where the job's log and config are discarded.
- **SSH:** `git clone ssh://git@<host>:<port>/<owner>/<repo>`. The listener is in the default
  build (feature `git-ssh`, config `[ssh]`, default port 2222, off unless `[ssh] enabled = true`).
  The staging compose file enables it and publishes container port 2222 as host port 22223:
  `git clone ssh://git@asc-staging.example.org:22223/<owner>/<repo>` (configured; not
  verified until deployed). Check the host key on first connect: the server logs
  `ssh host key` with a `SHA256:...` fingerprint at start, and `asc serve ssh host-key show`
  prints it with a `known_hosts` line. Compare it with the fingerprint ssh shows before you
  answer yes; never accept a mismatch. The key lives in the state volume
  (`/var/lib/asc/ssh/ssh_host_ed25519_key`), so redeploys keep it; `host-key rotate` changes it.

## 6. Transport policy

Remotes are reached over https or ssh only. Plain `http://` is refused, except loopback
(`127.0.0.1`, `localhost`) when you set `ASC_ALLOW_LOOPBACK_HTTP=1` in the client (and, for a
local test server, in `asc serve` too). Use it for the local compose bundle in `deploy/` only.
HTTP/3 is used when the server advertises it and falls back to HTTP/2.

## 7. Where to go next

- Every command, with an example: [`CLI.md`](CLI.md). Every configuration key: [`CONFIG.md`](CONFIG.md).
  All docs: [`README.md`](README.md).
- What works today and what does not: [`STATUS.md`](STATUS.md).
- Run your own server: [`../deploy/README.md`](../deploy/README.md).
- Something wrong: `asc doctor` and ONBOARDING section 8.
