// The signed-in home on a desktop, as drawn in the Dashboard board: the work and repository lists on
// the left, what needs you in the middle, your stacks and today on the right.
import { type ReactNode, useRef } from "react";
import { type HomeData, useHome } from "../api/home";
import type { NeedRow } from "../api/needs";
import { NavLists } from "../components/chrome";
import { useAgents } from "../lib/agents";
import { greeting, useViewer } from "../lib/viewer";
import { Link } from "../lib/router";
import { agoShort } from "../lib/time";
import { useTitle } from "../lib/title";
import { OpenRepo } from "./open-repo";
import "../styles/screens/dash.css";

const SHOWN = 3;

function dotClass(kind: NeedRow["kind"]): string {
  if (kind === "merge") return "dash52";
  if (kind === "checks") return "dash49";
  return "dash38";
}

function NeedLine(props: { row: NeedRow; agents: boolean }): ReactNode {
  const { row } = props;
  const chips = row.chip !== "" || row.stat !== "" || (props.agents && row.agent !== "");
  return (
    <Link to={row.href} className="row dash37" data-need>
      <span className={dotClass(row.kind)} aria-hidden="true" />
      <div className="dash39">
        <div className="dash40">
          {row.headline}{" "}
          <span className="dash41">
            {row.repo}
            {row.detail ? ` · ${row.detail}` : ""}
          </span>
        </div>
        {chips ? (
          <div className="dash42">
            {props.agents && row.agent ? <span className="dash43">Agent: {row.agent}</span> : null}
            {row.chip ? <span className="dash45">{row.chip}</span> : null}
            {row.stat ? <span className="dash46">{row.stat}</span> : null}
          </div>
        ) : null}
        {row.quote ? <div className="dash48">{row.quote}</div> : null}
      </div>
      {row.kind === "merge" ? (
        <span className="dash53">Merge</span>
      ) : (
        <span className="dash47">{agoShort(row.at)}</span>
      )}
    </Link>
  );
}

/** j and k move between the rows, Enter opens the focused one (a link). */
function useRowKeys(): React.RefObject<HTMLElement | null> {
  const ref = useRef<HTMLElement | null>(null);
  return ref;
}

function onRowKeys(e: React.KeyboardEvent<HTMLElement>): void {
  if (e.ctrlKey || e.metaKey || e.altKey) return;
  const rows = [...e.currentTarget.querySelectorAll<HTMLElement>("[data-need]")];
  const at = rows.indexOf(document.activeElement as HTMLElement);
  if (e.key === "j") rows[Math.min(rows.length - 1, at + 1)]?.focus();
  else if (e.key === "k") rows[Math.max(0, at - 1)]?.focus();
}

function NeedsCard(props: { home: HomeData; agents: boolean }): ReactNode {
  const { needs, loading, repos } = props.home;
  const ref = useRowKeys();
  return (
    <section className="dash32" ref={ref} onKeyDown={onRowKeys} aria-labelledby="needs-h">
      <div className="dash33">
        <h2 className="dash34" id="needs-h">
          Needs you
        </h2>
        <span className="dash35">{needs.length}</span>
        <span className="dash36">j/k to move · Enter to open</span>
      </div>
      {needs.slice(0, SHOWN).map((row) => (
        <NeedLine key={row.id} row={row} agents={props.agents} />
      ))}
      {needs.length > SHOWN ? (
        <Link to="/inbox" className="row dash-more">
          {needs.length - SHOWN} more in your inbox
        </Link>
      ) : null}
      {needs.length === 0 ? (
        <div className="dash-empty" aria-busy={loading}>
          {loading
            ? "Loading…"
            : repos.length === 0
              ? "Nothing yet."
              : "Nothing is waiting on you."}
        </div>
      ) : null}
    </section>
  );
}

function StacksCard(props: { home: HomeData }): ReactNode {
  const { stacks } = props.home;
  if (stacks.length === 0) return null;
  return (
    <section className="dash67" aria-labelledby="stacks-h">
      <h2 className="dash68" id="stacks-h">
        Your stacks
      </h2>
      <div className="dash69">
        {stacks.slice(0, 3).map((s) => (
          <Link key={`${s.repo}:${s.ref}`} to={s.href} className="dash-stack">
            <div className="dash70">{s.name}</div>
            <div className="dash71" role="img" aria-label={`${s.segments.length} changes`}>
              {s.segments.map((seg, i) => (
                <span
                  // biome-ignore lint/suspicious/noArrayIndexKey: segments are positional
                  key={i}
                  className={seg === "done" ? "dash72" : seg === "waiting" ? "dash73" : "dash74"}
                />
              ))}
            </div>
          </Link>
        ))}
      </div>
    </section>
  );
}

function TodayCard(props: { home: HomeData }): ReactNode {
  const { today } = props.home;
  if (today.length === 0) return null;
  return (
    <section className="dash67" aria-labelledby="today-h">
      <h2 className="dash68" id="today-h">
        Today
      </h2>
      <div className="dash75">
        {today.map((t) => (
          <div key={t.id}>
            <b>{t.who}</b> {t.what}
          </div>
        ))}
      </div>
    </section>
  );
}

function Empty(): ReactNode {
  return (
    <section className="dash32 dash-first" aria-labelledby="first-h">
      <h2 className="dash-first-h" id="first-h">
        Open your first repository
      </h2>
      <p className="dash-first-p">
        Type a name such as <code>acme/app</code>, or press <kbd className="kbd">Ctrl</kbd>{" "}
        <kbd className="kbd">K</kbd>. The repositories you open are listed here, in this browser
        only.
      </p>
      <OpenRepo />
    </section>
  );
}

export default function DashboardDesktop(): ReactNode {
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
  return (
    <div data-board="dash" className="dash dash1" aria-busy={home.loading}>
      <div className="dash15">
        <aside className="side dash16" aria-label="Work and repositories">
          <NavLists highlight="/inbox" />
        </aside>
        <div className="dash25">
          <div className="dash26">
            <div>
              <h1 className="dash27">{hello}</h1>
              <div className="dash28">{sub}</div>
            </div>
            <div className="dash29">
              {agents ? (
                <Link to="/agents/new" className="dash30">
                  New agent task
                </Link>
              ) : null}
              <Link to="/new/change" className="dash31">
                New stack
              </Link>
            </div>
          </div>
          {home.repos.length === 0 ? <Empty /> : <NeedsCard home={home} agents={agents} />}
        </div>
        <div className="dash66">
          <StacksCard home={home} />
          <TodayCard home={home} />
          <section className="dash76">
            <b>Tip.</b> Press <span className="kbd">g</span> then <span className="kbd">i</span> to
            jump to your inbox from anywhere.
          </section>
        </div>
      </div>
    </div>
  );
}
