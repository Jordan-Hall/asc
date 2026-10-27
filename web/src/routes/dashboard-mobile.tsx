// The signed-in home on a phone, as drawn in the Mobile-Dashboard board.
import type { ReactNode } from "react";
import { useHome } from "../api/home";
import type { NeedRow } from "../api/needs";
import { MobileCta, repoColor } from "../components/chrome";
import { useAgents } from "../lib/agents";
import { Link } from "../lib/router";
import { agoShort } from "../lib/time";
import { useTitle } from "../lib/title";
import { greeting, useViewer } from "../lib/viewer";
import { OpenRepo } from "./open-repo";
import "../styles/screens/mdash.css";

function dotClass(kind: NeedRow["kind"]): string {
  if (kind === "merge") return "mdash30";
  if (kind === "checks") return "mdash28";
  return "mdash17";
}

function NeedLine(props: { row: NeedRow; agents: boolean; last: boolean }): ReactNode {
  const { row } = props;
  return (
    <Link to={row.href} className={`row tap ${props.last ? "mdash29" : "mdash16"}`}>
      <span className={dotClass(row.kind)} aria-hidden="true" />
      <div className="mdash18">
        <div className="mdash19">{row.headline}</div>
        <div className="mdash20">
          {row.repo}
          {row.detail ? ` · ${row.detail}` : ""}
        </div>
        {row.agent && props.agents ? (
          <div className="mdash21">
            <span className="mdash22">Agent: {row.agent}</span>
          </div>
        ) : null}
        {row.chip || row.stat ? (
          <div className="mdash21">
            {row.chip ? <span className="mdash24">{row.chip}</span> : null}
            {row.stat ? <span className="mdash25">{row.stat}</span> : null}
          </div>
        ) : null}
        {row.quote ? <div className="mdash27">{row.quote}</div> : null}
      </div>
      <span className="mdash26">
        {row.kind === "merge" ? <span className="mdash31">Merge</span> : agoShort(row.at)}
      </span>
    </Link>
  );
}

export default function DashboardMobile(): ReactNode {
  useTitle("Dashboard");
  const home = useHome();
  const { on: agents } = useAgents();
  const viewer = useViewer();
  const n = home.needs.length;
  const blocked = home.needs.filter((r) => r.kind === "checks").length;
  const hello = `${greeting(new Date().getHours())}${viewer ? `, ${viewer.first}` : ""}`;
  const sub =
    n === 0
      ? "Nothing is waiting on you."
      : `${n} ${n === 1 ? "thing needs" : "things need"} you. ${
          blocked > 0 ? `${blocked} blocked on failing checks.` : "Nothing is blocked."
        }`;
  const shown = home.needs.slice(0, 3);
  return (
    <div data-board="mdash" className="mdash mdash1" aria-busy={home.loading}>
      <div className="mdash9">
        <div>
          <h1 className="mdash10">{hello}</h1>
          <div className="mdash11">{sub}</div>
        </div>
        {home.repos.length === 0 ? (
          <section className="mdash12 mdash-first" aria-labelledby="first-h">
            <h2 className="mdash14" id="first-h">
              Open your first repository
            </h2>
            <OpenRepo />
          </section>
        ) : (
          <section className="mdash12" aria-labelledby="needs-h">
            <div className="mdash13">
              <h2 className="mdash14" id="needs-h">
                Needs you
              </h2>
              <span className="mdash15">{n}</span>
            </div>
            {shown.map((row, i) => (
              <NeedLine
                key={row.id}
                row={row}
                agents={agents}
                last={i === shown.length - 1 && n <= shown.length}
              />
            ))}
            {n > shown.length ? (
              <Link to="/inbox" className="row tap mdash29 mdash-more">
                {n - shown.length} more in your inbox
              </Link>
            ) : null}
            {n === 0 ? <div className="mdash-empty">Nothing is waiting on you.</div> : null}
          </section>
        )}
        {home.stacks.length > 0 ? (
          <section className="mdash12" aria-labelledby="stacks-h">
            <div className="mdash32">
              <h2 className="mdash14" id="stacks-h">
                Your stacks
              </h2>
              <Link to="/stacks" className="tap mdash33">
                All {home.stacks.length}
              </Link>
            </div>
            {home.stacks.slice(0, 3).map((s) => (
              <Link key={`${s.repo}:${s.ref}`} to={s.href} className="mdash43">
                <div className="mdash35">{s.name}</div>
                <div className="mdash44" role="img" aria-label={`${s.segments.length} changes`}>
                  {s.segments.map((seg, i) => (
                    <span
                      // biome-ignore lint/suspicious/noArrayIndexKey: segments are positional
                      key={i}
                      className={
                        seg === "done" ? "mdash45" : seg === "waiting" ? "mdash46" : "mdash47"
                      }
                    />
                  ))}
                </div>
              </Link>
            ))}
          </section>
        ) : null}
        {home.today.length > 0 ? (
          <section className="mdash48" aria-labelledby="today-h">
            <h2 className="mdash49" id="today-h">
              Today
            </h2>
            <div className="mdash50">
              {home.today.map((t) => (
                <div key={t.id}>
                  <b>{t.who}</b> {t.what}
                </div>
              ))}
            </div>
          </section>
        ) : null}
        {home.repos.length > 0 ? (
          <section className="mdash51" aria-label="Repositories">
            {home.repos.slice(0, 3).map((r, i) => (
              <Link key={r} to={`/${r}`} className="row tap mdash52">
                <span
                  className="mdash-sq"
                  style={{ background: repoColor(i) }}
                  aria-hidden="true"
                />
                {r}
                <span className="mdash54" aria-hidden="true">
                  ›
                </span>
              </Link>
            ))}
            <Link to="/repos" className="row tap mdash57">
              All repositories
            </Link>
          </section>
        ) : null}
      </div>
      <MobileCta>
        <Link to="/new/change" className="btn-primary tap">
          New stack
        </Link>
        {agents ? (
          <Link to="/agents/new" className="btn-secondary tap">
            New agent task
          </Link>
        ) : null}
      </MobileCta>
    </div>
  );
}
