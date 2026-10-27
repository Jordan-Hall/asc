// Repository settings, read only: visibility, branchless mode, autolinks and the policy of the
// default branch. Changes are made in `.asc/policy.toml` on trunk and with `asc serve admin`.
import type { ReactNode } from "react";
import { useApp, useQ } from "../api/queries";
import { Badge, Empty, Page, Panel } from "../components/bits";
import { ErrorState } from "../components/error-state";
import { RowsSkeleton } from "../components/skeleton";
import { useParams } from "../lib/router";
import { useTitle } from "../lib/title";

function Item(props: { label: string; children: ReactNode }): ReactNode {
  return (
    <div className="flex min-h-11 flex-wrap items-center gap-x-4 gap-y-1 border-b border-line px-4 py-2 last:border-b-0">
      <dt className="w-44 text-sm text-fg-muted">{props.label}</dt>
      <dd className="m-0 min-w-0 flex-1 text-sm">{props.children}</dd>
    </div>
  );
}

export default function RepoSettingsPage(): ReactNode {
  const { owner = "", repo = "" } = useParams();
  useTitle(`Settings · ${owner}/${repo}`);
  const { queries } = useApp();
  const s = useQ(queries.repoSettings(`${owner}/${repo}`, "HEAD"));
  if (s.status === "error") return <ErrorState error={s.error} />;
  const d = s.data;
  const p = d?.policy;
  return (
    <Page
      title="Repository settings"
      sub={
        <>
          These are read from the server. Change the policy in <code>.asc/policy.toml</code> on
          trunk.
        </>
      }
    >
      {!d ? (
        <Panel>
          <RowsSkeleton rows={4} rowHeight={44} label="Loading settings" />
        </Panel>
      ) : (
        <>
          <h2 className="mb-2 text-lg font-semibold text-fg-strong">General</h2>
          <Panel label="General">
            <dl className="m-0">
              <Item label="Visibility">{d.visibility || "private"}</Item>
              <Item label="Branchless">
                <Badge tone={d.branchless ? "good" : "plain"}>{d.branchless ? "On" : "Off"}</Badge>
              </Item>
              <Item label="Commit cloud">
                <Badge tone={d.cloud ? "good" : "plain"}>{d.cloud ? "On" : "Off"}</Badge>
              </Item>
              <Item label="Shared refs">
                {d.sharedRefs.length > 0 ? (
                  <code>{d.sharedRefs.join(", ")}</code>
                ) : (
                  "The default branch, release branches and tags"
                )}
              </Item>
            </dl>
          </Panel>

          <h2 className="mb-2 mt-6 text-lg font-semibold text-fg-strong">Autolinks</h2>
          <Panel label="Autolinks">
            {d.autolinks.length === 0 ? (
              <Empty>No autolinks are configured.</Empty>
            ) : (
              <dl className="m-0">
                {d.autolinks.map((a) => (
                  <Item key={a.prefix} label={a.prefix}>
                    <code className="break-all">{a.url}</code>
                  </Item>
                ))}
              </dl>
            )}
          </Panel>

          <h2 className="mb-2 mt-6 text-lg font-semibold text-fg-strong">Policy</h2>
          <Panel label="Policy">
            {!p ? (
              <Empty>Trunk has no policy file, so the server defaults apply.</Empty>
            ) : (
              <dl className="m-0">
                {p.error ? (
                  <Item label="Problem">
                    <Badge tone="bad">Unusable</Badge> {p.error}
                  </Item>
                ) : null}
                <Item label="Merge strategy">{p.mergeStrategy || "Server default"}</Item>
                <Item label="Trunk">
                  <code>{p.trunk || "default branch"}</code>, {p.approvals}{" "}
                  {p.approvals === 1 ? "approval" : "approvals"} required
                  {p.allowForce ? ", force pushes allowed" : ""}
                </Item>
                <Item label="Required checks">
                  {p.checks.length > 0 ? <code>{p.checks.join(", ")}</code> : "None"}
                </Item>
                {p.roles.map((r) => (
                  <Item key={r.name} label={`Role ${r.name}`}>
                    {r.members.length > 0 ? r.members.join(", ") : "No members"}
                  </Item>
                ))}
                {p.paths.map((r) => (
                  <Item key={r.glob} label={`Path ${r.glob}`}>
                    Writable by {r.write.length > 0 ? r.write.join(", ") : "nobody"}
                  </Item>
                ))}
              </dl>
            )}
          </Panel>
        </>
      )}
    </Page>
  );
}
