# Getting started with asc

Install asc, sign in to a server, clone, and work in stacks. The git to asc command map is in
[`CHEATSHEET.md`](CHEATSHEET.md); the `git` shim and `asc doctor` are in [`ONBOARDING.md`](ONBOARDING.md).
[`SPEC.md`](SPEC.md) wins where this page disagrees.

## 1. Install

Release files live at `https://forge.example.org/api/packages/asc/generic/asc`
(`<ver>/...`; the newest version is in `latest/version`). See [`RELEASING.md`](RELEASING.md).

| Platform | How |
|---|---|
| Windows (x86_64) | Run the MSI, or use winget with the manifests in `<ver>/winget/` (package id `asc.asc`). An elevated MSI also puts the shim directory on the machine `PATH`. |
| Linux (x86_64) | Download `install.sh`, `asc-<ver>-x86_64-unknown-linux-gnu.tar.gz` and its `.minisig`, then `sh install.sh --tarball <file> [--shim] [--no-service]`. It needs `minisign`. |
| From source | Rust 1.95: `cargo build --release -p asc`. |
| macOS, arm | No release build exists yet. |

Real git 2.40 or later is required. Then run `asc doctor`; `asc upgrade` updates a release install.
Optionally `asc install-shim` so `git` is asc inside an asc workspace (see ONBOARDING section 2).

## 2. Authenticate

asc has no `login` command. `asc auth status [--json] [<url>]` only reports which source would
supply a credential for a URL and never prints the secret. Credentials come, in this order,
from the URL, git's credential helpers, and the `ASC_TOKEN` environment variable (sent only to
the remote's own origin; see `crates/asc/src/credential.rs`).

1. Get a token from the server administrator. On an `asc serve` with the token store the
   administrator mints it with `asc serve token create --user <name> --repo '<owner>/*' --expires <duration>`
   (add `--admin` for an admin). The value is printed once.
2. Give it to asc either way:
   - `export ASC_TOKEN=<token>`, or
   - store it with git's helper, which asc uses as git does:
     `printf 'protocol=https\nhost=asc-staging.example.org\nusername=<name>\npassword=<token>\n' | git credential approve`.
3. Check: `asc auth status https://asc-staging.example.org`.

Never put a token in a URL. asc does not store a credential that came from URL userinfo
(divergence Q-6 in [`divergences.md`](divergences.md)). Nothing prompts.

## 3. Clone

```sh
asc clone https://asc-staging.example.org/<owner>/<repo>
```

The clone is lazy: file contents download when first read (`--mode plain` writes real files).
`asc clone <url> <dir> --reference <old .git>` borrows objects from an old clone. The default
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
Plain `git commit`, `git push origin feat` and friends also work through the shim. In a
branchless repository (the default) only `master`, release branches and tags are shared refs;
`git push origin feat` submits the stack as draft changes and `feat` becomes your user ref.
Needs a server: `asc submit` against a plain git remote exits 128 with an explanation.

`asc push --land` (or `-o land`) asks the server to land onto `master` when paths do not conflict.

## 5. Plain git against `asc serve`

`asc serve` speaks git directly, so tools without asc work:

- **Smart HTTP (https):** `git clone https://asc-staging.example.org/<owner>/<repo>.git`.
  The server answers 401, then git asks its credential helper (same token as above, as the
  password; use a helper or `http.extraHeader`, never the URL). Pushes go through the same
  checks as `asc push`. With `[git] enabled = false` the git routes answer 403 with `X-Asc-Git`.
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

- What works today and what does not: [`STATUS.md`](STATUS.md).
- Run your own server: [`../deploy/README.md`](../deploy/README.md).
- Something wrong: `asc doctor` and ONBOARDING section 8.
