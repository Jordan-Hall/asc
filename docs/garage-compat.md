# Garage compatibility

Measured results for asc-store's S3 layer against real Garage. The suite is `cargo test -p asc-store --features it-s3`
(the `garage_*` tests of `it_s3.rs` and `it_bucketrefs.rs`), run by `build/ci/garage-it.sh` and by the `garage-compat`
job in `.forgejo/workflows/asc-ci.yml`. No Docker is involved: the script starts the static Garage binary, builds the
layout, imports the fixed test key and creates bucket `asc-it` with the `garage` CLI (the admin API differs between v1 and
v2, the CLI does not).

The production Garage (ASC, `garage.example.org`) runs `dxflrs/garage:v2.3.0`, so **v2.3.0 is the production version** and
is tested below next to the latest v2.x. Run `asc doctor --store` against it to confirm;
the probe outcome below is the same on every version tested here.

## Run it

```sh
git clone <repo> ~/asc-garage && cd ~/asc-garage        # ext4, not DrvFs
GARAGE_BIN=~/garage-bin/garage-v2.3.0 bash build/ci/garage-it.sh
GARAGE_BIN=~/garage-bin/garage-v1 bash build/ci/garage-it.sh
```

Data and metadata go to a fresh `~/garage-it/run-<pid>` that the script removes on exit (trap); Garage is stopped with
it. Cargo builds into `CARGO_TARGET_DIR` (default `~/asct/garage`). `ASC_IT_GARAGE_EXPECT_EXCLUSIVE=true|false` makes the
probe test assert the expected outcome.

## Versions tested

Binaries: `https://garagehq.deuxfleurs.fr/_releases/<version>/x86_64-unknown-linux-musl/garage`. These are the latest v1.x and v2.x on the releases page on 2026-10-23, plus v2.3.0, the production version.
Garage publishes no checksum files (`garage.sha256` is a 404), so the hashes are
those of the bytes first fetched; the CI job verifies them.

| Version | sha256 |
|---|---|
| v1.3.1 | `0441419f7c0b372d37e533a80e08367023d30fa56a56d1db3f8a2aff27e24c92` |
| v2.3.0 (production) | `f98d317942bb341151a2775162016bb50cf86b865d0108de03eb5db16e2120cd` |
| v2.4.1 | `ae49f8de4aaee6b5ca305f7cbdccc8cd8a29b2804aac074649f18bfb83d0a2b6` |

`deploy/garage.toml` uses `replication_mode`, which v2 rejects ("legacy replication_mode is no longer supported"). The
script rewrites it to `replication_factor = 1` in its temp copy, which all three versions accept. Compose still pins
`dxflrs/garage:v1.0.1` with the original file.

## Results

| Test | v1.3.1 | v2.3.0 (production) | v2.4.1 |
|---|---|---|---|
| `garage_conformance` (`run_objects_suite` over `BucketObjects` and `Resilient`: put, get, range, has_many, list, tamper detection) | pass | pass | pass |
| `garage_open_roundtrip` (`s3::open_with` URL: path-style, region `garage`, put, get, ranged get, CopyObject, list, delete, 404) | pass | pass | pass |
| `garage_large_value_is_multipart` (32 MiB + 1 KiB through `open_with`, multipart upload and read back) | pass | pass | pass |
| `garage_list_paginates` (1200 keys, more than one ListObjectsV2 page) | pass | pass | pass |
| `garage_strict_store_does_not_downgrade` (`open_strict` probe, `open_with` Create twice) | pass | pass | pass |
| `garage_probe_is_reported` (raw store, `probe`) | pass (probe fails) | pass (probe fails) | pass (probe fails) |
| 6 of 6 | 6 pass, 0 fail | 6 pass, 0 fail | 6 pass, 0 fail |

The `minio_*` tests are not run (they need MinIO). The checksum headers `object_store` sends were accepted by all three versions.

## Probe outcome (`If-None-Match: *`)

| | v1.3.1 | v2.3.0 (production) | v2.4.1 |
|---|---|---|---|
| Second `PutMode::Create` of the same key | succeeds (header ignored) | succeeds (header ignored) | succeeds (header ignored) |
| Raw `curl --aws-sigv4` PUT with `If-None-Match: *`, twice | 200, 200 | 200, 200 | 200, 200 |
| `probe().create_exclusive` | false: "ignores create-only puts (second create succeeded)" | false: same message | false: same message |
| `NotImplemented` / 501 / 400 on the header | never | never | never |

All three versions accept the header and do nothing with it. Neither returns an error, so `is_create_unsupported` never
fires and the `create_unsupported` flag of `Resilient` stays false. This is the dangerous case: without the probe, a
`Create` looks like it worked.

What asc does about it:

- **Content-addressed objects** (`BucketObjects`) are unaffected. A repeated put of an identical value is idempotent, so
  an ignored header changes nothing. `garage_conformance` shows this.
- **`BucketRefs`** is refused, because the probe fails. The detail names the store: `Resilient(AmazonS3(asc-it)): ignores
  create-only puts (second create succeeded)`. `open_strict` does not downgrade `Create`, so a store that errors with
  `NotImplemented` is reported rather than hidden. Refs on Garage need `--refs local` or `asc serve`.
- The `create_unsupported` to `Overwrite` downgrade is covered by the unit tests in `crates/asc-store/src/s3.rs` (a store
  that answers `NotImplemented`). No tested Garage triggers it.

## asc-store changes

None were needed: path-style addressing, region `garage`, checksum headers, multipart, CopyObject and list pagination all
work as they are. The tests added in `it_s3.rs` cover those paths through the production `open_with`/`open_strict`
entry points.

## Suggested SPEC wording (not applied)

SPEC §3.4, Probe bullet, replace the last sentence: "Garage v1.3.1, v2.3.0 (production) and v2.4.1 all fail the probe: they accept
`If-None-Match: *` and ignore it (HTTP 200 on the second create), so `BucketRefs` is refused on Garage until a release
honours it (see docs/garage-compat.md)."

SPEC §19 row "MinIO, R2 and Garage `If-None-Match` behaviour (probe outcomes)": add "Garage v1.3.1, v2.3.0 (production) and v2.4.1 measured
2026-10-23: header ignored, probe fails (docs/garage-compat.md). MinIO and R2 not measured here."
