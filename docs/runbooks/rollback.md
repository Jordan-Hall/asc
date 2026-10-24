# Rollback drill (SPEC §14.3 stage 2)

Run on staging before the Stage 2 flip, and again on production only if the flip must be undone.
The bridge is current (`bridge=eager`), so git SHAs are identical after the rollback.

```sh
export CFG=/etc/asc/asc-serve.toml
ascs() { asc serve --config "$CFG" "$@"; }
REPOS="asc/dds asc/soma"

# 1. Freeze pushes in Forgejo (maintenance mode).
# 2. Wait until asc's outbox has delivered every effect to the bridge. Exit 1 = not drained.
for r in $REPOS; do ascs admin outbox drain --wait --timeout 600 --repo "$r" || exit 1; done
# 3. Switch Forgejo back: set storage_backend=git (Forgejo admin setting, K08) for each repo.
# 4. Flip asc back to mirror. Refused while any outbox row is undelivered.
for r in $REPOS; do ascs admin set-mode --repo "$r" --mode mirror --forgejo-reads git --bridge off || exit 1; done
# 5. Unfreeze, then prove the mirror still matches git.
ascs admin import --repo asc/dds && ascs admin import --repo asc/soma
ascs admin verify-import --repo asc/dds --repo asc/soma
```

The drill passes when step 5 exits 0 and prints `mismatches: 0`, a push to Forgejo reaches the
mirror on the next import, and CI still checks out. Record the date in the cutover ticket.

If step 2 times out, the last line names the oldest outbox error; fix that effect (usually a
bridge promote failure, `tier-forgejo.md`) and rerun from step 2. Never use `--force` here.
