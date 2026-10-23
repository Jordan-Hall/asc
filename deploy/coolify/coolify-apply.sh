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
#   --check-app-json FILE
#               offline: check a saved `GET /api/v1/applications/<uuid>` answer against the
#               COOLIFY_* identity below; exits non-zero on a mismatch. Sends nothing.
#
# Identity, required for --apply: COOLIFY_PROJECT_UUID, COOLIFY_ENVIRONMENT_NAME and
# COOLIFY_SERVER_UUID (optionally COOLIFY_DESTINATION_UUID for a create).
#
# Create versus update, never by name alone:
#   - COOLIFY_APP_UUID unset: create. The application list is read only to refuse when an
#     application of that name already exists in this project and environment (or cannot be
#     placed); the refusal prints its uuid.
#   - COOLIFY_APP_UUID set: update. The application is read first and must report the same
#     project, environment and server as the COOLIFY_* identity; any mismatch, or a field Coolify
#     does not report, stops the script before anything is sent (so no secret reaches it).
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
check_json=

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
    --check-app-json)
      check_json="${2:-}"
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
if [[ -z "$check_json" ]]; then
  [[ -n "$env_file" && -r "$env_file" ]] || die "--env-file FILE is required and must be readable"
  [[ -r "$compose" ]] || die "cannot read compose file $compose"
fi
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
        "environment_name": os.environ.get("COOLIFY_ENVIRONMENT_NAME") or "<COOLIFY_ENVIRONMENT_NAME>",
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
else:
    import os

    def dig(obj, keys):
        for k in keys:
            obj = obj.get(k) if isinstance(obj, dict) else None
        return obj if isinstance(obj, str) and obj else None

    def ident(app):
        return {
            "project": dig(app, ["environment", "project", "uuid"]) or dig(app, ["project_uuid"]),
            "environment": dig(app, ["environment", "name"]) or dig(app, ["environment_name"]),
            "server": dig(app, ["destination", "server", "uuid"])
            or dig(app, ["server", "uuid"])
            or dig(app, ["server_uuid"]),
        }

    ENVVAR = {"project": "COOLIFY_PROJECT_UUID", "environment": "COOLIFY_ENVIRONMENT_NAME", "server": "COOLIFY_SERVER_UUID"}
    want = {
        "project": os.environ.get("COOLIFY_PROJECT_UUID", ""),
        "environment": os.environ.get("COOLIFY_ENVIRONMENT_NAME", ""),
        "server": os.environ.get("COOLIFY_SERVER_UUID", ""),
    }
    if mode == "verify":
        # Fails closed: a field Coolify does not report counts as a mismatch.
        have = ident(data)
        problems = []
        for k, v in want.items():
            if not v:
                problems.append(f"{ENVVAR[k]} is not set")
            elif have[k] is None:
                problems.append(f"Coolify does not report the application's {k}; it cannot be verified")
            elif have[k] != v:
                problems.append(f"the application's {k} is '{have[k]}', not the expected '{v}'")
        print("\n".join(problems))
        sys.exit(3 if problems else 0)
    elif mode.startswith("same-name:"):
        # Applications of this name that are, or could be, in the target project and environment.
        for app in data if isinstance(data, list) else []:
            if not isinstance(app, dict) or app.get("name") != mode[10:]:
                continue
            have = ident(app)
            if have["project"] in (None, want["project"]) and have["environment"] in (None, want["environment"]):
                print(app.get("uuid", ""))
PY

build() { python3 -I "$work/build.py" "$1" "$env_file" "$compose" "$name" "${@:2}"; }

# verify_identity FILE: FILE is a Coolify application answer; it must match the COOLIFY_* identity.
verify_identity() {
  local out
  if out="$(python3 -I "$work/pick.py" "$1" verify)"; then
    return 0
  fi
  printf '%s\n' "$out" | sed 's/^/  - /' >&2
  return 1
}

if [[ -n "$check_json" ]]; then
  [[ -r "$check_json" ]] || die "cannot read $check_json"
  verify_identity "$check_json" || die "application does not match the COOLIFY_* identity"
  echo "ok: application matches the COOLIFY_* identity"
  exit 0
fi

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

missing_identity=
for v in COOLIFY_PROJECT_UUID COOLIFY_ENVIRONMENT_NAME COOLIFY_SERVER_UUID; do
  [[ -n "${!v:-}" ]] || missing_identity="$missing_identity $v"
done

if [[ "$mode" == dry-run ]]; then
  uuid="${COOLIFY_APP_UUID:-<new application uuid>}"
  echo "DRY RUN: nothing is sent. Application '$name'."
  if [[ -n "$missing_identity" ]]; then
    echo "note: --apply would stop: not set:$missing_identity"
  fi
  echo
  if [[ -z "${COOLIFY_APP_UUID:-}" ]]; then
    echo "# 1. CREATE (COOLIFY_APP_UUID is not set). Read the list only to refuse when an application"
    echo "#    named '$name' already exists in project/environment (its uuid is printed for you to set)"
    show GET /api/v1/applications
    echo "# 2. create the application"
    show POST /api/v1/applications/dockercompose create
  else
    echo "# 1. UPDATE $uuid. Read it and require its project, environment and server to equal"
    echo "#    COOLIFY_PROJECT_UUID, COOLIFY_ENVIRONMENT_NAME and COOLIFY_SERVER_UUID; stop on any"
    echo "#    mismatch, before anything is sent"
    show GET "/api/v1/applications/$uuid"
  fi
  echo "# 3. set the compose file and the domain (a created application is read back and verified first)"
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

[[ -z "$missing_identity" ]] || die "not set:$missing_identity (the target project, environment and server must be named)"

uuid="${COOLIFY_APP_UUID:-}"
if [[ -z "$uuid" ]]; then
  # Create. The name is only checked for a clash; it never selects an application to update.
  call GET /api/v1/applications
  clash="$(python3 -I "$work/pick.py" "$work/resp" "same-name:$name")"
  if [[ -n "$clash" ]]; then
    printf '%s\n' "$clash" | sed 's/^/  existing application: /' >&2
    if [[ "$(printf '%s\n' "$clash" | wc -l)" -gt 1 ]]; then
      die "more than one application is named '$name' in this project/environment; nothing was sent"
    fi
    die "an application named '$name' already exists; to update it set COOLIFY_APP_UUID=$clash (nothing was sent)"
  fi
  call POST /api/v1/applications/dockercompose create
  uuid="$(python3 -I "$work/pick.py" "$work/resp" uuid)"
  [[ -n "$uuid" ]] || die "Coolify did not return an application uuid"
  echo "created application $uuid" >&2
fi
[[ "$uuid" =~ ^[A-Za-z0-9]+$ ]] || die "unexpected application uuid"

# Whether it was just created or named by the operator, read it back and check where it lives
# before the compose file or any environment value is sent.
call GET "/api/v1/applications/$uuid"
verify_identity "$work/resp" || die "application $uuid is not in the expected project/environment/server; nothing was sent"
echo "application $uuid matches the project, environment and server" >&2

call PATCH "/api/v1/applications/$uuid" update
call PATCH "/api/v1/applications/$uuid/envs/bulk" envs
if [[ "$do_deploy" == 1 ]]; then
  call POST "/api/v1/deploy?uuid=$uuid&force=false"
fi
echo "$uuid"
