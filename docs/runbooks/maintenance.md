# Read-only maintenance (SPEC §14.5)

Puts every node of the server into read-only mode for an upgrade, a storage move or an incident.
Reads, clones and fetches keep working. Pushes, lands, merges, cloud syncs and object uploads are
refused with `UNAVAILABLE` and the message you set.

```sh
export CFG=/etc/asc/asc-serve.toml
ascs() { asc serve --config "$CFG" "$@"; }

# 1. Switch on. Every node follows within 5 seconds.
ascs admin maintenance on --message "moving to new disks" --for 30m --version 2.4.0
# 2. Check it: a push is refused with the message.
ascs admin maintenance status
# 3. Do the work (upgrade, migrate, move storage).
ascs migrate
# 4. Switch off. Writes are accepted again within 5 seconds.
ascs admin maintenance off
```

Notes:

* A site administrator can also switch from the hub: Site administration, Maintenance. When asc
  is enabled the hub switch also switches asc, so both go read-only together.
* `--for` is shown to users as the expected end. It is never acted on: maintenance ends only when
  someone switches it off.
* The admin listener's `/readyz` reports `"maintenance": true` and `"read_only": true`. The
  server stays ready (200), so load balancers keep sending reads.
* A node that cannot read the database keeps its last state. If the database is down when you
  switch off, nodes stay read-only until they can read it again.
* A staged restore ([restore.md](restore.md)) also makes the server read-only. That ends only at
  the restart. Switching maintenance off does not end it.
* Each switch is in the audit log as `server.maintenance.updated`. The CLI records the actor as
  `operator`.
