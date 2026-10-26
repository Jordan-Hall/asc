# asc serve on Coolify: staging runbook

Stage 1 of SPEC (search `Stage 1 (week`): `asc serve` runs beside Forgejo with its state in a
Postgres schema `asc` on Forgejo's database server and in a Garage bucket `asc-staging`. Proposed
address: **https://asc-staging.example.org**.

| File | Purpose |
|---|---|
| `asc-serve.compose.yml` | Coolify "Docker Compose" application: a one-shot migration service and `ascserve`. |
| `asc-serve.env.example` | Every variable, with a comment each and placeholders only. |
| `garage-provision.sh` | Bucket plus a key that can read and write that one bucket. |
| `coolify-apply.sh` | Creates or updates the Coolify application. `--dry-run` is the default. |

| `asc-serve.standalone.compose.yml` | The variant that runs on staging today: no Forgejo, token-store auth, SQLite. See "Standalone staging" below. |
| `forgejo-staging.compose.yml` | ASC Hub (the Forgejo v16 fork) staging, SQLite, own volume. |

Nothing here runs against the platform until you run it. The image is built elsewhere
(`deploy/Dockerfile.serve`) and pushed to `forge.example.org/asc/asc-serve:<sha12>`.

## What the image must provide

The compose file assumes these facts about the image. Check them before step 4.

- `asc serve` is started as `serve` (the entrypoint is the `asc` binary) and `serve migrate` runs the
  migration.
- A non-root user with the uid in `ASC_RUN_UID` (default 10001), `/var/lib/asc` owned by it (a named
  volume copies that ownership on first use), `git` on the path, and `curl` for the healthcheck.
- The Coolify server is logged in to `forge.example.org` (`docker login`) so it can pull.

## Design notes

- **Configuration.** `asc serve` reads one TOML file (`ASC_SERVE_CONFIG`); secrets in it are file
  paths, except the Postgres DSN, which is inline. The Forgejo-mode compose file renders the file,
  and the two token files, from Compose `configs` with inline content (the standalone file uses a
  file mount instead; see below). S3 credentials use the standard
  `AWS_*` variables. Values come from the Coolify environment.
- **Forge.** `[auth] provider = "forgejo"` with `[forgejo] url`: a Basic credential from plain `git`
  goes to Forgejo (SPEC 15.6.2). `asc serve` refuses to start with `[forgejo]` and no
  `internal_token_file`, so Forgejo's `INTERNAL_TOKEN` is required.
- **Git.** `[git] enabled = true` serves smart-HTTP (SPEC 25). Set `ASC_GIT_ENABLED=false` for the
  403 stub. The Forgejo-mode file has no SSH listener; the standalone staging file enables one
  (host port 22222 to container 2222, see "Standalone staging").
- **Admin.** The admin listener keeps its loopback default: no metrics route, no token. Use
  `docker exec` for `/readyz` detail.
- **Network.** `ascserve` is on the application's own Coolify network (holding only this app and
  the proxy) and on the platform network named by `ASC_BACKEND_NETWORK` for outbound calls to
  Postgres and Garage. Containers on the platform network can still reach port 7890 on it, so the
  proxy trust below is exact (see step 5).
- **Hardening.** Read-only root file system with a `/tmp` tmpfs, `cap_drop: ALL`,
  `no-new-privileges`, a pids limit, CPU and memory limits, and a named volume `asc-state` for
  derived data only (cache, git views).

## Standalone staging (what runs now)

`https://asc-staging.example.org` runs `asc-serve.standalone.compose.yml`. Facts that shaped it:

- `[auth] provider = "token_store"`; the database is SQLite (`sqlite:///var/lib/asc/asc.db`) on the
  `asc-state` volume, so one replica only; objects go to Garage bucket `asc-staging`
  (`https://garage.example.org`, https only). `[git] enabled = true`. SSH git is
  configured (`[ssh]`, `listen = "0.0.0.0:2222"`, published as host port 22222; host key in the
  state volume). It is not yet deployed or verified end to end.
- The config file is a Coolify **file mount** (a `volumes:` entry with `content:`) holding the
  staging values. Coolify rejects compose `configs:` content in read-only services, which is why
  the Forgejo-mode file's approach is not used here.
- Resource limits are `deploy.resources.limits` only (`cpus`, `memory`, `pids`). Coolify rejects
  `pids_limit` together with `deploy` limits, so use the one form.
- Services: `asc-garage-init` (one-shot bucket and key; remove `GARAGE_ADMIN_TOKEN` after the first
  deploy), `asc-migrate`, `asc-bootstrap` (mints the first admin token once into
  `/var/lib/asc/bootstrap/admin-token`; read it from the Coolify terminal of `ascserve`, store it,
  delete the file) and `ascserve`.
- Image: `forge.example.org/asc/asc-serve:${ASC_IMAGE_TAG}`; pin the sha12 printed by the
  `image` workflow, never a moving tag.
- Starting a stopped service through the Coolify API is `POST /api/v1/services/{uuid}/start`
  (the application endpoints do not apply to a service).

### CI notes

- `.forgejo/workflows/image.yml` runs on the `asc-build-trusted` runner for pushes to `master` and
  `v*` tags and for manual runs of those refs; other refs build on `asc-build` and never publish.
- The runner needs `docker` with `buildx`.
- Repository variable `REGISTRY_USER` (the user the token belongs to) and secret `REGISTRY_TOKEN`
  (a `write:package` token) are required to push.
- If the runner lives in WSL, WSL stops its distribution when idle: keep a session open or raise
  `instanceIdleTimeout` in the WSL settings, or the runner goes offline mid-job.
- A running image build always finishes; newer pushes queue behind it (cold builds take up to 50 minutes).

## 1. Garage: bucket and key

Run on the host that has the `garage` CLI (or set `GARAGE_CMD="docker exec -i garage-<service-uuid> /garage"`):

```sh
deploy/coolify/garage-provision.sh asc-staging ./garage-key-asc-staging.secret
```

It prints the **key id**. The secret goes only to the 0600 file. The key has read and write on
`asc-staging` and nothing else (owner and bucket creation are withdrawn); the script prints
`garage key info` so you can confirm that one bucket is listed. Re-running it changes nothing.
Copy the id and the secret into the env file, then delete the secret file.

## 2. Postgres: a role with rights on schema `asc` only

On Forgejo's database server, as a superuser, connected to Forgejo's database (replace
`<forgejo-db>`):

```sql
CREATE ROLE asc_hub LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 20;
\password asc
CREATE SCHEMA asc_hub AUTHORIZATION asc_hub;
ALTER ROLE asc_hub IN DATABASE "<forgejo-db>" SET search_path = asc_hub;
```

The role owns schema `asc` (migrations create tables there) and nothing else. It has no grant on
Forgejo's tables in `public`, so it cannot read them. On Postgres 14 and older also confirm that
`public` does not allow `CREATE` for `PUBLIC` (`\dn+ public`); change it only if Forgejo is not
relying on it. Check with `psql -U asc`: `SELECT 1 FROM public.repository;` must fail with
`permission denied`.

DSN (percent-encode the password):
`postgres://asc_hub:<password>@<db-host>:5432/<forgejo-db>?sslmode=prefer&options=-csearch_path%3Dasc_hub`

## 3. Secrets and the Forgejo webhook

Copy `asc-serve.env.example` to a private file outside the repository and fill every `CHANGE_ME`:

| Variable | Where it comes from |
|---|---|
| `ASC_IMAGE_TAG` | the sha12 of the image you mean to run |
| `ASC_DATABASE_URL` | step 2 |
| `ASC_S3_ENDPOINT`, `ASC_S3_ACCESS_KEY_ID`, `ASC_S3_SECRET_ACCESS_KEY` | step 1; endpoint `https://garage.example.org` (https only; the server refuses plain http) |
| `ASC_BACKEND_NETWORK` | `docker network ls` on the Coolify server: the network on which `garage-<service-uuid>` resolves |
| `ASC_FORGEJO_INTERNAL_TOKEN` | Forgejo `app.ini` `[security] INTERNAL_TOKEN` |
| `ASC_FORGEJO_WEBHOOK_SECRET` | `openssl rand -hex 32` |
| `ASC_TRUSTED_PROXIES` | leave empty now; step 5 |

Add the Forgejo webhook on the repositories to mirror: URL `https://asc-staging.example.org/hooks/forgejo`,
content type JSON, the same secret, push events. The server also polls every 60 s, so a missing
webhook delays but does not lose imports.

## 4. Create the application and deploy

Dry run first. It prints every call with secrets masked and contacts nothing:

```sh
export COOLIFY_URL=https://coolify.example.org COOLIFY_TOKEN=...   # token never goes on a command line
deploy/coolify/coolify-apply.sh --dry-run --env-file /path/to/private.env
```

When it looks right (and has no "would stop on" note), find the project and server uuids in the
Coolify UI and apply:

```sh
export COOLIFY_PROJECT_UUID=... COOLIFY_SERVER_UUID=... COOLIFY_ENVIRONMENT_NAME=...
# later runs update the same application: export COOLIFY_APP_UUID=<uuid printed by the first run>
deploy/coolify/coolify-apply.sh --apply --env-file /path/to/private.env
```

It never selects an application by name. With `COOLIFY_APP_UUID` unset it **creates**: it reads the
application list only to refuse when `asc-serve-staging` already exists in that project and
environment (it prints that uuid), then creates the application. With `COOLIFY_APP_UUID` set it
**updates**. In both cases it reads the application back and requires the project, environment and
server Coolify reports to equal `COOLIFY_PROJECT_UUID`, `COOLIFY_ENVIRONMENT_NAME` and
`COOLIFY_SERVER_UUID`; a mismatch, or a field Coolify does not report, stops it before the compose
file or any secret is sent. To check a saved `GET /api/v1/applications/<uuid>` answer offline, run
`coolify-apply.sh --check-app-json answer.json`. Then it sets the compose file and the domain and
stores the environment. Open the application in the Coolify UI and use
**Preview compose** to confirm that the `configs` and the `backend` network survived Coolify's
rewrite; then deploy from the UI, or re-run with `--apply --deploy`. The `asc-migrate` service runs
`asc serve migrate` first and `ascserve` starts only after it succeeds.

Not yet done by this runbook: register the repositories (`RepoAdmin.init` with `mode=mirror`, for
`asc/soma` and `asc/dds`, SPEC Stage 1) and the Forgejo-side proxy route for `/.well-known/asc`
(only needed to make `asc clone https://forge.example.org/...` discover this server).

## 5. Trust the proxy, and only the proxy

`limits.trusted_proxies` decides which peer may name the real client in `X-Forwarded-For`. It is
empty after step 4 (no forwarded header is trusted; every client looks like the proxy and shares
one per-IP limit). Set it to the proxy's **exact address on this application's network**, as a /32:

```sh
docker network ls | grep <application-uuid>
docker inspect coolify-proxy --format '{{(index .NetworkSettings.Networks "<application-uuid>").IPAddress}}'
```

Put `ASC_TRUSTED_PROXIES="172.18.0.2/32"` (quotes included, your address) in the application's
environment and redeploy. Never use a range: any container inside it could spoof the header and
get around per-IP limits and admission. If the proxy is recreated and its address changes, the
setting fails safe (the header is ignored) until you update it.

## 6. Verify

```sh
H=https://asc-staging.example.org
curl -fsS $H/healthz && curl -fsS $H/readyz && echo ok       # process, then database and outbox
curl -fsS $H/.well-known/asc                                    # discovery document
git ls-remote $H/asc/soma.git                                # 401 challenge, then your Forgejo PAT
git clone $H/asc/soma.git /tmp/soma-git                      # plain smart-HTTP clone
asc clone $H/asc/soma /tmp/soma-asc                            # native asc clone through discovery
(cd /tmp/soma-asc && asc doctor --store)                         # bucket probe (SPEC 6.2)
```

Use a git credential helper or `http.extraHeader` for the PAT, never a token in the URL. With
`ASC_GIT_ENABLED=false` the git commands answer 403 with `X-Asc-Git`.

`asc doctor --store` is implemented (`crates/asc/src/cli/doctor.rs`: store transport checks and an
integrity sample of stored objects). Its probe puts a `probe/<uuid>` key twice with `Create`; the second put must fail with
`AlreadyExists`, and SPEC expects Garage v2 to fail it. That matters only when a bucket holds refs
(`BucketRefs`). Here refs live in Postgres and the bucket holds objects, but record the result.

## Logs

`asc serve` writes its logs to stderr, so Coolify's log view and `docker logs` show them at once.

- `ASC_LOG` (compose passes it as `RUST_LOG`) is the level filter; the default is `info`. Use
  `info,asc_serve=debug` to see more of the server, or `info,asc_serve::access=off` to drop the
  access log.
- `ASC_LOG_FORMAT=json` (or `[log] format = "json"` in `asc-serve.toml`) writes one JSON object per
  line. The default is human-readable text. The variable wins over the config file.
- At start the server logs `asc serve starting` (version, role, listen addresses, `public_url`,
  store scheme and bucket, database kind, auth provider) and, once every listener is up,
  `asc serve ready` (git, ssh with host key fingerprints, mcp, web, h3, migration state). Neither
  line holds a credential, DSN, query string or key.
- Each HTTP or RPC request gets one line on target `asc_serve::access`: method, path (no query),
  status, `latency_ms`, `principal` (when the request authenticated), `request_id` and, for gRPC,
  `grpc_status`. The same id goes back in the `x-request-id` response header; a caller may send its
  own (letters, digits, `-`, `_`, `.`, 64 characters at most). `/healthz`, `/readyz` and `/metrics`
  log at `debug`. 4xx responses and failed RPCs log at `warn`, 5xx at `error`. gRPC streaming
  calls report the status of the response headers.
- Authorization headers, tokens and request bodies are never logged. Refused credentials show as
  a count (`authorization refused ... refused_total=N`, one `warn` per 100) and the 401 or 403 in
  the access log.

## 7. Rollback

- **Bad image:** set `ASC_IMAGE_TAG` back to the previous sha12 (in the Coolify UI, or edit the env
  file and re-run `coolify-apply.sh --apply --deploy`). Migrations are expand-only (SPEC 8.2), so
  the previous release runs on the newer schema.
- **Bad config:** fix the variable and redeploy; the state volume is derived data.
- **Bad mirror:** nothing in Forgejo changes. In Stage 1 `asc serve` only reads the repositories.

## 8. Teardown

1. Delete the application in Coolify (with its volume `asc-state`).
2. Remove the Forgejo webhooks.
3. Drop the schema and role: `DROP SCHEMA asc_hub CASCADE; DROP OWNED BY asc_hub; DROP ROLE asc_hub;`.
4. Remove the key and the bucket: `garage key delete asc-serve-asc-staging --yes`, then empty the
   bucket (`aws s3 rm --recursive`) and `garage bucket delete asc-staging --yes`.
5. Delete the private env file and any leftover `*.secret` file.
