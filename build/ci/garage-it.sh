#!/usr/bin/env bash
# Runs asc-store's S3 conformance suite (`--features it-s3`, the garage_* tests) against a real Garage, no Docker.
# Usage: GARAGE_BIN=/path/to/garage build/ci/garage-it.sh   (Linux; prefer an ext4 clone, not DrvFs)
# Starts a single-node Garage from deploy/garage.toml (temp copy, loopback only), initialises it with the
# `garage` CLI (layout, key import, bucket asc-it; works on v1 and v2), runs the tests and stops Garage on exit.
# Env: GARAGE_BIN (required), ASC_GARAGE_WORK (default ~/garage-it), ASC_GARAGE_S3_PORT (3900), ASC_GARAGE_RPC_PORT (3901),
#      CARGO_TARGET_DIR (default ~/asct/garage). Test overrides: ASC_IT_GARAGE_{ENDPOINT,BUCKET,REGION,KEY,SECRET}.
set -euo pipefail
cd "$(dirname "$0")/../.."
REPO=$(pwd)

if [ "$(uname -s)" != "Linux" ]; then
  echo "garage-it.sh must run on Linux (uname: $(uname -s))" >&2
  exit 2
fi
: "${GARAGE_BIN:?set GARAGE_BIN to a garage binary (v1.x or v2.x)}"
[ -x "$GARAGE_BIN" ] || { echo "GARAGE_BIN is not executable: $GARAGE_BIN" >&2; exit 2; }

S3_PORT=${ASC_GARAGE_S3_PORT:-3900}
RPC_PORT=${ASC_GARAGE_RPC_PORT:-3901}
ADMIN_PORT=${ASC_GARAGE_ADMIN_PORT:-3903}
# These match the defaults in crates/asc-store/tests/it_s3.rs.
KEY=${ASC_IT_GARAGE_KEY:-GK0123456789abcdef01234567}
SECRET=${ASC_IT_GARAGE_SECRET:-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef}
BUCKET=${ASC_IT_GARAGE_BUCKET:-asc-it}

# A fresh directory per run, so a rerun never sees old layout, keys or objects.
WORK=${ASC_GARAGE_WORK:-$HOME/garage-it}/run-$$
rm -rf "$WORK"
mkdir -p "$WORK/meta" "$WORK/data"
CFG=$WORK/garage.toml
LOG=$WORK/garage.log
sed -e "s#^metadata_dir = .*#metadata_dir = \"$WORK/meta\"#" \
    -e "s#^data_dir = .*#data_dir = \"$WORK/data\"#" \
    -e "s#^rpc_bind_addr = .*#rpc_bind_addr = \"127.0.0.1:$RPC_PORT\"#" \
    -e "s#^rpc_public_addr = .*#rpc_public_addr = \"127.0.0.1:$RPC_PORT\"#" \
    -e "s#^replication_mode = .*#replication_factor = 1#" \
    -e "s#^api_bind_addr = \"0.0.0.0:3900\"#api_bind_addr = \"127.0.0.1:$S3_PORT\"#" \
    -e "s#^api_bind_addr = \"0.0.0.0:3903\"#api_bind_addr = \"127.0.0.1:$ADMIN_PORT\"#" \
    deploy/garage.toml >"$CFG"

PID=
cleanup() {
  status=$?
  if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.1; done
    kill -9 "$PID" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ] && [ -f "$LOG" ]; then echo "--- garage log (tail) ---" >&2; tail -n 15 "$LOG" >&2; fi
  rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT INT TERM

garage() { "$GARAGE_BIN" -c "$CFG" "$@"; }

"$GARAGE_BIN" --version
"$GARAGE_BIN" -c "$CFG" server >"$LOG" 2>&1 &
PID=$!

ID=
for _ in $(seq 1 100); do
  kill -0 "$PID" 2>/dev/null || { echo "garage exited early" >&2; exit 1; }
  ID=$(garage status 2>/dev/null | grep -oE '^[0-9a-f]{16}' | head -n1 || true)
  [ -n "$ID" ] && break
  sleep 0.2
done
[ -n "$ID" ] || { echo "garage did not report a node id" >&2; exit 1; }

garage layout assign -z dc1 -c 1G "$ID" >/dev/null
garage layout apply --version 1 >/dev/null
garage key import --yes -n asc-it "$KEY" "$SECRET" >/dev/null
garage bucket create "$BUCKET" >/dev/null
garage bucket allow --read --write --owner "$BUCKET" --key "$KEY" >/dev/null

# The S3 endpoint answers 403/404 (not a refused connection) once the layout is live.
for _ in $(seq 1 100); do
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$S3_PORT/" || true)
  [ "$code" != "000" ] && break
  sleep 0.2
done
echo "garage ready (node $ID)"

# Raw evidence, independent of object_store: does this Garage honour `If-None-Match: *` on PUT?
# 200 then 200 means the header is ignored (BucketRefs is refused); 200 then 412 means it is honoured.
put_code() {
  curl -s -o /dev/null -w '%{http_code}' -X PUT -H 'If-None-Match: *' --data-binary 'asc probe' \
    --user "$KEY:$SECRET" --aws-sigv4 "aws:amz:garage:s3" "http://127.0.0.1:$S3_PORT/$BUCKET/raw-probe" || true
}
first=$(put_code)
second=$(put_code)
echo "raw If-None-Match: * PUT twice -> $first then $second"

export ASC_IT_GARAGE_ENDPOINT=${ASC_IT_GARAGE_ENDPOINT:-http://127.0.0.1:$S3_PORT}
export ASC_IT_GARAGE_BUCKET=$BUCKET ASC_IT_GARAGE_KEY=$KEY ASC_IT_GARAGE_SECRET=$SECRET
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$HOME/asct/garage}
cd "$REPO"
# Only the garage_* tests: the minio_* ones need MinIO. --nocapture prints the probe outcome.
cargo test -p asc-store --features it-s3 --test it_s3 --test it_bucketrefs -- --ignored --nocapture --test-threads=1 garage_
