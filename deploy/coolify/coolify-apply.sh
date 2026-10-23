#!/usr/bin/env bash
# Creates or updates the asc-serve staging application in Coolify through /api/v1, the way
# monorepo's scripts/deploy-affected.ts talks to it (Bearer token, COOLIFY_URL/COOLIFY_TOKEN).
#
#   COOLIFY_URL=https://coolify.example.org COOLIFY_TOKEN=... \
#     coolify-apply.sh [--dry-run | --apply] --env-file FILE [--deploy] [--name NAME] [--compose FILE]
#
#   --dry-run   (default) print every call that --apply would make; secrets are masked; the API
#               is not contacted.
#   --apply     make the calls. Refuses while a required value is missing or still CHANGE_ME.
#   --env-file  the application environment (see asc-serve.env.example): KEY=VALUE lines, values
#               taken verbatim. All of it is stored as the application's environment.
#   --deploy    with --apply, queue a deployment at the end (POST /deploy, as deploy-affected.ts).
#   --name      Coolify application name (default asc-serve-staging).
#   --compose   compose file (default asc-serve.compose.yml next to this script).
#
# Also read from the environment: COOLIFY_APP_UUID (update this application instead of looking one
# up by name), and for a first create COOLIFY_PROJECT_UUID, COOLIFY_SERVER_UUID,
# COOLIFY_ENVIRONMENT_NAME (default production) and optionally COOLIFY_DESTINATION_UUID.
#
# Secrets never appear in argv: the token goes to curl as a header file and every body as a
# --data @file, both mode 0600 in a private temp directory that is removed on exit. Needs bash,
# curl (--apply only) and python3.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mode=dry-run
env_file=
name=asc-serve-staging
compose="$here/asc-serve.compose.yml"
do_deploy=0

die() {
  printf 'coolify-apply: %s\n' "$*" >&2
  exit 1
}

usage() {
  sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d; s/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) mode=dry-run ;;
    --apply) mode=apply ;;
    --deploy) do_deploy=1 ;;
    --env-file)
      env_file="${2:-}"
      shift
      ;;
    --name)
      name="${2:-}"
      shift
      ;;
    --compose)
      compose="${2:-}"
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown argument '$1' (see --help)" ;;
  esac
  shift
done

[[ -n "${COOLIFY_URL:-}" && -n "${COOLIFY_TOKEN:-}" ]] || die "COOLIFY_URL and COOLIFY_TOKEN are required"
[[ -n "$env_file" && -r "$env_file" ]] || die "--env-file FILE is required and must be readable"
[[ -r "$compose" ]] || die "cannot read compose file $compose"
[[ "$name" =~ ^[a-z0-9][a-z0-9-]*$ ]] || die "--name must be lowercase letters, digits and dashes"
command -v python3 >/dev/null 2>&1 || die "python3 is required"

base="${COOLIFY_URL%/}"
if [[ "$mode" == apply && ! "$base" =~ ^https:// && ! "$base" =~ ^http://(localhost|127\.0\.0\.1)(:|/|$) ]]; then
  die "refusing to send the token over plain http to $base"
fi

umask 077
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
chmod 700 "$work"

# The payload builder. Values are read from the env file and the process environment, never from
# argv. With --mask, secret-looking values and the compose text are replaced for printing.
cat >"$work/build.py" <<'PY'
import base64, hashlib, json, os, re, sys

kind, env_file, compose_file, name = sys.argv[1:5]
mask = "--mask" in sys.argv[5:]
SECRET = ("SECRET", "TOKEN", "PASSWORD", "DATABASE_URL", "ACCESS_KEY", "DSN")


def read_env(path):
    env, bad = {}, []
    with open(path, encoding="utf-8") as fh:
        for n, line in enumerate(fh, 1):
            line = line.rstrip("\r\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            m = re.fullmatch(r"([A-Z][A-Z0-9_]*)=(.*)", line)
            if not m:
                bad.append(f"line {n} is not KEY=VALUE")
            elif m.group(1) in env:
                bad.append(f"line {n} repeats {m.group(1)}")
            else:
                env[m.group(1)] = m.group(2)
    return env, bad


def secret(key):
    return any(s in key for s in SECRET)


env, bad = read_env(env_file)
compose = open(compose_file, "rb").read()
text = compose.decode()
# Required = no default and not allowed empty (`${NAME:?`); `${NAME?` may be empty but must exist.
required = sorted(set(re.findall(r"\$\{([A-Z][A-Z0-9_]*):\?", text)))
exists = sorted(set(re.findall(r"\$\{([A-Z][A-Z0-9_]*)\?", text)))

if kind == "check":
    problems = list(bad)
    for k in required + ["ASC_FQDN"]:
        if not env.get(k):
            problems.append(f"{k} is missing or empty")
    for k in exists:
        if k not in env:
            problems.append(f"{k} is missing (it may be empty, but it must be set)")
    for k, v in sorted(env.items()):
        if "CHANGE_ME" in v:
            problems.append(f"{k} is still a CHANGE_ME placeholder")
    print("\n".join(problems))
    sys.exit(3 if problems else 0)

if bad:
    sys.exit("env file: " + "; ".join(bad))

raw = base64.b64encode(compose).decode()
if mask:
    raw = f"<base64 of {len(compose)} bytes, sha256 {hashlib.sha256(compose).hexdigest()[:12]}>"
fqdn = env.get("ASC_FQDN", "")

if kind == "create":
    body = {
        "project_uuid": os.environ.get("COOLIFY_PROJECT_UUID") or "<COOLIFY_PROJECT_UUID>",
        "server_uuid": os.environ.get("COOLIFY_SERVER_UUID") or "<COOLIFY_SERVER_UUID>",
        "environment_name": os.environ.get("COOLIFY_ENVIRONMENT_NAME") or "production",
        "name": name,
        "description": "asc serve staging (SPEC Stage 1); managed by deploy/coolify/coolify-apply.sh",
        "docker_compose_raw": raw,
        "instant_deploy": False,
    }
    if os.environ.get("COOLIFY_DESTINATION_UUID"):
        body["destination_uuid"] = os.environ["COOLIFY_DESTINATION_UUID"]
elif kind == "update":
    body = {
        "name": name,
        "docker_compose_raw": raw,
        "docker_compose_domains": [{"name": "ascserve", "domain": f"https://{fqdn}"}],
    }
elif kind == "envs":
    body = {
        "data": [
            {
                "key": k,
                "value": "********" if mask and secret(k) else v,
                "is_preview": False,
                "is_literal": True,
                "is_multiline": False,
            }
            for k, v in sorted(env.items())
        ]
    }
else:
    sys.exit(f"unknown kind {kind}")
print(json.dumps(body, indent=2 if mask else None))
PY

# Reads a JSON answer and prints one field, or the API's own message on failure.
cat >"$work/pick.py" <<'PY'
import json, sys

path, mode = sys.argv[1:3]
try:
    data = json.load(open(path, encoding="utf-8"))
except Exception:
    data = None
if mode == "message":
    if isinstance(data, dict):
        print(data.get("message", ""), json.dumps(data.get("errors", "")) if data.get("errors") else "")
elif mode == "uuid":
    if isinstance(data, dict):
        print(data.get("uuid", ""))
elif mode.startswith("find:"):
    want = mode[5:]
    for app in data if isinstance(data, list) else []:
        if isinstance(app, dict) and app.get("name") == want:
            print(app.get("uuid", ""))
            break
PY

build() { python3 -I "$work/build.py" "$1" "$env_file" "$compose" "$name" "${@:2}"; }

# --- validation ------------------------------------------------------------------------------
problems="$(build check || true)"
if [[ -n "$problems" ]]; then
  if [[ "$mode" == apply ]]; then
    printf '%s\n' "$problems" | sed 's/^/  - /' >&2
    die "the env file is not ready; fix the items above (nothing was sent)"
  fi
  echo "note: --apply would stop on:"
  printf '%s\n' "$problems" | sed 's/^/  - /'
  echo
fi

# --- dry run ---------------------------------------------------------------------------------
show() { # method path [kind]
  echo "$1 $base$2"
  echo "  Authorization: Bearer ********"
  echo "  Content-Type: application/json"
  if [[ -n "${3:-}" ]]; then
    echo "  body:"
    build "$3" --mask | sed 's/^/    /'
  fi
  echo
}

if [[ "$mode" == dry-run ]]; then
  uuid="${COOLIFY_APP_UUID:-<application uuid>}"
  echo "DRY RUN: nothing is sent. Application '$name'."
  echo
  if [[ -z "${COOLIFY_APP_UUID:-}" ]]; then
    echo "# 1. look the application up by name"
    show GET /api/v1/applications
    echo "# 2. only when no application has that name: create it"
    show POST /api/v1/applications/dockercompose create
  else
    echo "# 1-2. COOLIFY_APP_UUID is set: no lookup, no create"
    echo
  fi
  echo "# 3. set the compose file and the domain (also run right after a create)"
  show PATCH "/api/v1/applications/$uuid" update
  echo "# 4. store the environment"
  show PATCH "/api/v1/applications/$uuid/envs/bulk" envs
  if [[ "$do_deploy" == 1 ]]; then
    echo "# 5. queue the deployment"
    show POST "/api/v1/deploy?uuid=$uuid&force=false"
  else
    echo "# no --deploy: start the deployment from the Coolify UI, or re-run with --apply --deploy"
  fi
  exit 0
fi

# --- apply -----------------------------------------------------------------------------------
printf 'Authorization: Bearer %s\nContent-Type: application/json\nAccept: application/json\n' \
  "$COOLIFY_TOKEN" >"$work/headers"
chmod 600 "$work/headers"

# call METHOD PATH [kind]: sends the request, leaves the answer in $work/resp, dies on non-2xx.
call() {
  local method="$1" path="$2" kind="${3:-}" status
  local -a data=()
  if [[ -n "$kind" ]]; then
    build "$kind" >"$work/body"
    chmod 600 "$work/body"
    data=(--data "@$work/body")
  fi
  status="$(curl -sS --max-time 60 -o "$work/resp" -w '%{http_code}' -X "$method" \
    -H "@$work/headers" "${data[@]}" "$base$path")" || die "$method $path: curl failed"
  if [[ ! "$status" =~ ^2 ]]; then
    die "$method $path: HTTP $status $(python3 -I "$work/pick.py" "$work/resp" message)"
  fi
  echo "$method $path -> HTTP $status" >&2
}

uuid="${COOLIFY_APP_UUID:-}"
if [[ -z "$uuid" ]]; then
  call GET /api/v1/applications
  uuid="$(python3 -I "$work/pick.py" "$work/resp" "find:$name")"
fi
if [[ -z "$uuid" ]]; then
  : "${COOLIFY_PROJECT_UUID:?COOLIFY_PROJECT_UUID is required to create the application}"
  : "${COOLIFY_SERVER_UUID:?COOLIFY_SERVER_UUID is required to create the application}"
  call POST /api/v1/applications/dockercompose create
  uuid="$(python3 -I "$work/pick.py" "$work/resp" uuid)"
  [[ -n "$uuid" ]] || die "Coolify did not return an application uuid"
  echo "created application $uuid" >&2
else
  echo "updating application $uuid" >&2
fi
[[ "$uuid" =~ ^[A-Za-z0-9]+$ ]] || die "unexpected application uuid"

call PATCH "/api/v1/applications/$uuid" update
call PATCH "/api/v1/applications/$uuid/envs/bulk" envs
if [[ "$do_deploy" == 1 ]]; then
  call POST "/api/v1/deploy?uuid=$uuid&force=false"
fi
echo "$uuid"
