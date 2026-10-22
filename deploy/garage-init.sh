#!/bin/sh
# One-shot Garage setup for integration tests: layout, bucket `asc-it`, and a fixed key.
# Idempotent enough for a fresh CI run; the key matches the defaults in crates/asc-store/tests/it_s3.rs.
set -eu
A=http://garage:3903
H="Authorization: Bearer asc-it-admin-token"
KEY=GK0123456789abcdef01234567
SECRET=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
ID=$(curl -fsS -H "$H" $A/v1/status | sed -n 's/.*"id" *: *"\([0-9a-f]*\)".*/\1/p' | head -n1)
[ -n "$ID" ] || { echo "no node id" >&2; exit 1; }
curl -fsS -H "$H" -X POST $A/v1/layout -d "[{\"id\":\"$ID\",\"zone\":\"dc1\",\"capacity\":1073741824,\"tags\":[]}]" >/dev/null
curl -fsS -H "$H" -X POST $A/v1/layout/apply -d '{"version":1}' >/dev/null
curl -fsS -H "$H" -X POST $A/v1/key/import -d "{\"accessKeyId\":\"$KEY\",\"secretAccessKey\":\"$SECRET\",\"name\":\"asc-it\"}" >/dev/null
BID=$(curl -fsS -H "$H" -X POST $A/v1/bucket -d '{"globalAlias":"asc-it"}' | sed -n 's/.*"id" *: *"\([0-9a-f]*\)".*/\1/p' | head -n1)
curl -fsS -H "$H" -X POST $A/v1/bucket/allow -d "{\"bucketId\":\"$BID\",\"accessKeyId\":\"$KEY\",\"permissions\":{\"read\":true,\"write\":true,\"owner\":true}}" >/dev/null
echo "garage ready"
