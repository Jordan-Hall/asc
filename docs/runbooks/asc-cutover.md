# ASC cutover runbook (SPEC §14.3)

Every command runs on the `asc serve` host. `CFG` is the server config. Commands read the
database and store named there and never prompt; secrets stay in the files the config names,
never in argv. Each command exits 0 on success and 1 on a failed check, so a gate is
`cmd || exit 1`.

```sh
export CFG=/etc/asc/asc-serve.toml
alias ascs='asc serve --config "$CFG"'
```

Commands used below:

| Command | Meaning |
|---|---|
| `ascs migrate` | Apply the schema (never done at server start). |
| `ascs admin import --repo R [--init] [--source URL]` | Fetch and import git history. The only way git history reaches a server (SPEC §8.4). `--init` creates R in mirror mode. `--source` has no credentials; the config supplies tokens. |
| `ascs admin verify-import --repo R...` | Every git commit's export equals git. Skipped refs (SPEC §14.1 step 6) are listed and not counted. Prints `mismatches: N`; exits 1 when N > 0. |
| `ascs admin verify --all` | Closure of every ref exists and hashes; the git map round-trips. |
| `ascs admin outbox drain --wait [--repo R] [--timeout S]` | Wait for the running server to deliver all queued effects. Exits 1 on timeout. |
| `ascs admin set-mode --repo R [--mode mirror\|primary] [--forgejo-reads git\|asc] [--bridge off\|lazy\|eager] [--force]` | Flip repo switches. Refuses `--mode mirror` while the outbox holds undelivered rows, and `--mode primary` before a clean import (`--force` skips only the second check). A running server picks the change up within one second. |

## Stage 0 (week 6): pilots, no server

1. Prep: apply the §12.5 hygiene. Archive the 25.6 GiB LFS store to a cold S3 bucket, outside asc.
2. Pilot users install asc (MSI) and run
   `asc clone https://forge.example.org/asc/dds D:\work\dds --reference D:\dev\monorepo\.git`.
3. `asc doctor --report-reclaimable` lists the old `.git` (58 GB), the 49 git worktrees and the
   submodule git dirs. The user deletes them; asc never does.

## Stage 1 (week 10): mirror mode

```sh
ascs migrate
ascs admin import --repo asc/dds  --init
ascs admin import --repo asc/soma --init
```

Add the Forgejo webhook and the proxy route for `/.well-known/asc`. Then the gate:

```sh
ascs admin verify-import --repo asc/dds --repo asc/soma
```

**Gate (staging first, then production):** exit 0 and the last line is `mismatches: 0`. Do not
proceed otherwise. Move CI to the aliased `actions/checkout` (§12.7) and the plan job.

## Stage 2 (week 14): asc is truth

Roll back on staging first (`rollback.md`). Then, in the freeze window:

```sh
# 1. Freeze pushes in Forgejo (maintenance mode), then the final import and its gate.
ascs admin import --repo asc/dds  && ascs admin import --repo asc/soma
ascs admin verify-import --repo asc/dds --repo asc/soma || exit 1
# 2. Flip. Refused unless the import above was clean.
for r in asc/dds asc/soma; do
  ascs admin set-mode --repo "$r" --mode primary --forgejo-reads git --bridge eager || exit 1
done
# 3. Deploy the fork (K01-K09) with storage_backend=asc; unfreeze.
ascs admin verify --all || exit 1
```

The existing bare repo is now the bridge.

## Stage 3 (week 18)

`storage_backend=dual` for a week. Zero mismatches above noise in
`forgejo_asc_shadow_mismatch_total`, then per repo
`ascs admin set-mode --repo R --forgejo-reads asc` and `storage_backend=asc`.

## Stage 4 (week 22)

K12-K13 deployed; `ascs admin set-mode --repo R --bridge lazy`. Full storage replacement.

## Later

Stage 6 IDs (optional); folding soma and artefacts into asc are user decisions.
