# Deploy bundle

A self-contained stack for running `asc serve` on one machine, started with one command:

```sh
docker compose -f deploy/compose.yml up -d --wait
```

Stop and discard it with `docker compose -f deploy/compose.yml down -v`.

## What it runs

| Service | Role | Host address |
| --- | --- | --- |
| `garage` | single-node S3 store, bucket `asc-it` made by `garage-init` | none |
| `postgres` | the `asc-serve` database | none |
| `asc-migrate` | `asc serve migrate`, runs once | none |
| `asc-primary` | `asc serve`, primary role, token-store auth | `http://127.0.0.1:17890` (admin and `/metrics` on `19464`) |
| `asc-cache` | `asc serve`, cache role, upstream is the primary | `http://127.0.0.1:17891` (admin on `19465`) |
| `forgejo` | Forgejo with `DEFAULT_BRANCH = master` (`forgejo/app.ini`) | `http://127.0.0.1:13000` |

`deploy/Dockerfile` builds `asc --no-default-features --features serve` in two stages and runs it as
uid 10001 on a read-only root filesystem with all capabilities dropped.

## Transport

SSH or HTTPS only. The one exception is plain http to loopback behind an explicit opt-in, and this
bundle uses it: `asc-primary` and `asc-cache` share Garage's network namespace (`network_mode:
service:garage`), so Garage is `127.0.0.1:3900` and the primary is `127.0.0.1:7890` for them, and
`ASC_ALLOW_LOOPBACK_HTTP=1` is set. Host ports bind `127.0.0.1` only. Clients on the host need the same
opt-in (`export ASC_ALLOW_LOOPBACK_HTTP=1`). For anything beyond your own machine, terminate TLS in
front and use `asc-serve.toml.example`.

## Secrets

Every secret in `compose.yml` and `forgejo/app.ini` (Garage key, Postgres password, admin token,
Forgejo keys) is public and for local use only. Do not expose these ports and do not reuse the values.

## Use it

```sh
# mint a token (printed once), then create a repository and push
TOKEN=$(docker compose -f deploy/compose.yml exec -T asc-primary \
  asc serve token create --user me --repo 'me/*' --admin --expires 1d)
export ASC_ALLOW_LOOPBACK_HTTP=1 ASC_TOKEN=$TOKEN
asc api RepoAdmin --remote http://127.0.0.1:17890/me/app --json '{"repo":{"name":"me/app"},"init":{}}'
asc clone http://127.0.0.1:17890/me/app app
# metrics (admin listener, bearer token from compose.yml)
curl -H 'Authorization: Bearer asc-deploy-admin-token-not-secret' http://127.0.0.1:19464/metrics
```

## Smoke test

With the stack up:

```sh
cargo test -p asc --test deploy_smoke -- --ignored
```

It initialises a fresh repository through `RepoAdmin`, then clones, pushes, lands, runs
`diff --remote`, puts and gets a turbo artefact, and scrapes `/metrics`. It starts no services.

## Production

`asc-serve.toml.example` is the https shape: S3 over https, Postgres with verified TLS, a TLS proxy
in front, and secrets from the environment or files. `deploy/Dockerfile.serve` builds the
full-featured image (git smart-HTTP and SSH); `deploy/coolify/` holds the staging runbook.
