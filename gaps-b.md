# asc-side status of the settings and shell gap rows (unit asc-b)

Source rows: `D:/dev/asc-hub/docs/asc/feature-gaps/{settings,shell}.md`. Status words as in those files. "asc done" means the RPC, storage,
authorisation, audit and tests exist in `feat/asc-b`; the hub still has to call it and render it.

| Row | asc status | RPC the hub calls |
| --- | --- | --- |
| AdminBackups: schedule, target, next backup, Back up now, history with Retry / Restore / Verify, progress meter, RESTORE typed confirmation | asc done | `Backups.ListBackups` (history, schedule, status, progress), `ListTargets`, `RunBackup` (also Retry), `SetSchedule`, `VerifyBackup`, `PreviewRestore`, `Restore` |
| AdminDashboard: backup overdue with "Back up now" | asc done | `Backups.ListBackups` -> `status.overdue`, `status.last_success`; `RunBackup` |
| UserSettingsUsage: four tiles with meters and "of N" | asc done (agent minutes, AI spend, storage, bandwidth) | `Usage.GetUsage` (owner USER), `Usage.GetQuota` |
| UserSettingsUsage: by repository table | asc done | `Usage.GetUsage` -> `repos` |
| UserSettingsUsage: over-quota banner, empty month, BYO note | asc done | `GetUsageResponse.over_quota`, `empty`, `byo_ai_spend_micro_usd`, `byo_storage` |
| Spend and Budget panels: per key, per user, per org, per period | asc done | `Usage.ListSpend` (group_by KEY, USER, REPO, DAY, MONTH) |
| AdminUser: usage block (storage, agent minutes) | asc done | `Usage.GetUsage` with the `instance` claim and owner USER of that login is NOT allowed (a person reads their own); use the site administrator's `Usage.GetUsage` with owner INSTANCE for totals, or call as the user. See "Open" below |
| Storage and bandwidth feed | asc done, hub must report | `Usage.Report` (host service / site admin): storage gauge per repository, bandwidth delta, `byo_storage` flag |
| OrgSettings: Agents card (allow agents, agent changesets need a person, monthly spend ceiling) | asc done | `OrgSettings.GetOrgSettings`, `SetAgentPolicy` |
| OrgSettings: default merge policy, repositories inherit and can only tighten | asc done, land-gate hook open | `OrgSettings.SetMergeDefaults`, `ResolveMergePolicy`; `org::policy::apply` for the gate |
| OrgPeople / OrgTeams: agents section, agents with a path scope | asc done (organisation agent identities) | `OrgSettings.ListAgents`, `CreateAgent`, `UpdateAgent`, `DeleteAgent` |
| NewOrg: "Allow agents" switch | asc done | `OrgSettings.SetAgentPolicy` right after the organisation is created |
| AdminConfig: allow agents on this server, default agent budget | asc done | `OrgSettings.GetInstanceAgents`, `SetInstanceAgents` |
| UserSettingsSecurity: signed-in devices list, sign out one / everywhere else (asc side) | asc done for tokens, SSH keys, agent tokens; hub keeps browser sessions | `Sessions.ListSessions`, `RevokeSession`, `RevokeOtherSessions` |
| AdminUser: Sessions tab | asc done | same calls with `login` set and the `instance` admin claim |
| UserSettingsAccount: export your data (requested / ready, link, expiry) | asc done, mail and link are the hub's | `DataExport.RequestExport`, `GetExport`, `ListExports`, `Download` (stream), `DeleteExport` |
| UserSettingsAI: monthly cap per key | already done in `AiProviders`; unchanged | `AiProviders.UpdateKey` |
| UserSettingsAI: warn at 80%, stop at the cap | asc done (stored, audited warning once a month); agent runtime does not yet read `stop_at_cap` | `AiKeyShares.GetKeyCaps`, `SetKeyCaps` |
| UserSettingsAI / OrgAI: who can use it, share with organisation / repository / person | asc done | `AiKeyShares.ShareKey`, `ListShares`, `RevokeShare`, `ListSharedWithMe` |
| UserSettingsAI: usage per sharer, activity | asc done per sharer | `AiKeyShares.ShareUsage` |

## Open (not done in asc-b)

* **Land gate and agent runtime hooks.** `org::policy::apply` (merge defaults) and `OrgHub::agent_gate` (allow, need a person, ceiling) are
  complete and tested, but `policy/gate.rs` (line 820, `land_requirements()`) and `agents/` do not call them: those files are outside this
  unit. One line each: `let need = org::policy::apply(org_hub.merge_defaults(owner).await.as_ref(), policy.land_requirements());` and an
  `agent_gate(owner)` check before a run starts.
* **`stop_at_cap`** is stored and returned; the agent runtime should pause with a question instead of failing the run at `AI_CAP_REACHED`.
* **Bandwidth** is counted when a host reports it (`Usage.Report`) and by `UsageHub::add_bandwidth`; `git_http.rs` and `objects.rs` do not
  call `add_bandwidth` yet.
* **Admin view of one person's usage** (AdminUser usage block): `Usage.GetUsage` for a USER owner is the person's own; the hub can call it with the
  person's own claim. A site-administrator read of another account's usage is not offered.
* **Live restore.** A restore is staged and applied at the next start; the server does not go read-only while it waits, and does not restart itself.
* **CLI.** No `asc` subcommands were added for the new services (the hub is the client); `asc serve backup` and `asc serve restore` are unchanged.
* **Teams** are not an asc concept: a key shared "with a team" is shared with the team's repositories.
