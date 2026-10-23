#!/usr/bin/env bash
# Creates the Garage bucket for asc serve and a dedicated key that can read and write that bucket
# and nothing else (no owner right, no other bucket, no right to create buckets).
#
#   garage-provision.sh [bucket] [secret-file]
#
#   bucket       default asc-staging
#   secret-file  default ./garage-key-<bucket>.secret; created with mode 0600
#
# Run it where the `garage` CLI reaches the cluster (on the Garage host, or set GARAGE_CMD, for
# example GARAGE_CMD="docker exec -i garage-<service-uuid> /garage"). Safe to run again: an
# existing bucket, key or grant is kept, and the secret file is rewritten only when it is missing.
#
# Output: the key id on stdout. The secret goes only to the 0600 file, never to stdout or argv.
set -euo pipefail

bucket="${1:-asc-staging}"
secret_file="${2:-./garage-key-${bucket}.secret}"
key_name="asc-serve-${bucket}"
# Intentionally word-split: GARAGE_CMD may be a command with arguments.
read -r -a garage <<<"${GARAGE_CMD:-garage}"

die() {
  printf 'garage-provision: %s\n' "$*" >&2
  exit 1
}

if [[ ! "$bucket" =~ ^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$ ]]; then
  die "invalid bucket name '$bucket'"
fi
command -v "${garage[0]}" >/dev/null 2>&1 || die "cannot run '${garage[*]}'; set GARAGE_CMD"

umask 077

# Bucket: create only when `bucket info` says it does not exist.
if "${garage[@]}" bucket info "$bucket" >/dev/null 2>&1; then
  echo "bucket $bucket exists" >&2
else
  "${garage[@]}" bucket create "$bucket" >/dev/null
  echo "bucket $bucket created" >&2
fi

# Key: same rule. `key create` prints the secret; it is read from `key info --show-secret` below,
# so one path serves both a new key and a re-run that lost the file.
if "${garage[@]}" key info "$key_name" >/dev/null 2>&1; then
  echo "key $key_name exists" >&2
else
  "${garage[@]}" key create "$key_name" >/dev/null
  echo "key $key_name created" >&2
fi

# Exactly read and write on this one bucket; owner is explicitly withdrawn, and so is the right to
# create buckets. Both commands are no-ops when already in that state.
"${garage[@]}" bucket allow --read --write "$bucket" --key "$key_name" >/dev/null
"${garage[@]}" bucket deny --owner "$bucket" --key "$key_name" >/dev/null
"${garage[@]}" key deny --create-bucket "$key_name" >/dev/null

info="$("${garage[@]}" key info "$key_name" --show-secret)"
key_id="$(awk '/^Key ID:/ { print $3; exit }' <<<"$info")"
[[ -n "$key_id" ]] || die "no key id in 'key info $key_name'"

if [[ -s "$secret_file" ]]; then
  echo "secret file $secret_file exists; kept" >&2
else
  secret="$(awk '/^Secret key:/ { print $3; exit }' <<<"$info")"
  [[ -n "$secret" ]] || die "no secret in 'key info $key_name --show-secret'"
  (
    umask 077
    printf '%s\n' "$secret" >"$secret_file"
  )
  chmod 600 "$secret_file"
  echo "secret written to $secret_file (mode 0600)" >&2
fi

# Audit for the operator: the "Authorized buckets" block must list this bucket only (no secret).
echo "--- garage key info $key_name ---" >&2
"${garage[@]}" key info "$key_name" | grep -vi secret >&2 || true

echo "$key_id"
