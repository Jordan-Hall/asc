// The signed-out front page on a phone, as drawn in the Mobile-Landing board.
import { useEffect, useRef, useState } from "react";
import { LogoMark } from "../components/icons";
import { useAgents } from "../lib/agents";
import { Link } from "../lib/router";
import { useTitle } from "../lib/title";
import "../styles/screens/mland.css";

const SECTIONS = [
  ["#stacks", "Stacked changes"],
  ["#review", "Review"],
  ["#agents", "Agents"],
  ["#search", "Search"],
  ["#git", "Git compatible"],
] as const;

/** The menu behind the hamburger: the page sections and the way in. */
function MarketingMenu(props: { open: boolean; onClose: () => void }) {
  const ref = useRef<HTMLDialogElement | null>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (props.open && !d.open) d.showModal();
    if (!props.open && d.open) d.close();
  }, [props.open]);
  return (
    <dialog
      ref={ref}
      className="mland-menu"
      aria-label="Menu"
      onClose={props.onClose}
      onClick={(e) => {
        if (e.target === ref.current) props.onClose();
      }}
    >
      <div className="mland-menu-in">
        <button
          type="button"
          className="tap mland-menu-x"
          aria-label="Close menu"
          onClick={props.onClose}
        >
          Close
        </button>
        <nav aria-label="Sections">
          {SECTIONS.map(([href, label]) => (
            <a key={href} href={href} className="tap" onClick={props.onClose}>
              {label}
            </a>
          ))}
          <Link to="/signin" className="tap" onClick={props.onClose}>
            Sign in
          </Link>
        </nav>
      </div>
    </dialog>
  );
}

export default function LandingMobile() {
  useTitle("Review code at the speed it is written");
  const { on: agents } = useAgents();
  const [menu, setMenu] = useState(false);
  return (
    <>
      <div data-board="mland" className="mland mland1">
        <header className="mland2">
          <div className="mland3">
            <button
              type="button"
              className="tap mland4"
              aria-label="Open menu"
              aria-haspopup="dialog"
              onClick={() => setMenu(true)}
            >
              <svg
                width="22"
                height="22"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="1.8"
                strokeLinecap="round"
                strokeLinejoin="round"
                aria-hidden="true"
              >
                <path d="M4 7h16M4 12h16M4 17h16" />
              </svg>
            </button>
            <Link to="/" className="tap mland5" aria-label="ASC Hub home">
              <LogoMark size={26} tone="dark" />
              ASC Hub
            </Link>
            <Link to="/signin" className="tap mland6">
              Sign in
            </Link>
          </div>
        </header>
        <main id="main" tabIndex={-1} className="mland7">
          <section className="mland8">
            <div className="mland9">Built for teams who review a lot of code</div>
            <h1 className="mland10">Review code at the speed it is written.</h1>
            <p className="mland11">
              ASC Hub hosts your repositories as stacks of small, reviewable changes. See who made
              each one, why, and whose turn it is.
            </p>
            <div className="mland12">
              <Link to="/signin" className="tap mland13">
                Start free
              </Link>
              <a href="#demo" className="tap mland14">
                Watch a stack get reviewed
              </a>
            </div>
          </section>
          <section className="mland15">
            <div className="mono mland16">
              <span className="mland17">$</span> asc clone https://hub.example.com/acme/payments
            </div>
            <div className="mono mland18">or git clone https://hub.example.com/acme/payments</div>
          </section>
          <section className="mland19" id="demo">
            <div className="mland20">
              acme/payments › stack: <b className="mland21">retry-with-backoff</b>
            </div>
            <div className="mland22">
              <div className="mland23">Stack of 4</div>
              <div className="mland24">
                <span className="mland25" />
                <span className="mland26">Add retry policy type</span>
                <span className="mland27">Approved</span>
              </div>
              <div className="mland24">
                <span className="mland25" />
                <span className="mland26">Backoff with jitter</span>
                <span className="mland27">Approved</span>
              </div>
              <div className="mland24">
                <span className="mland28" />
                <span className="mland26">Wire into the client</span>
                <span className="mland27">Needs your review</span>
              </div>
              <div className="mland24">
                <span className="mland29" />
                <span className="mland26">Docs and metrics</span>
                <span className="mland27">Writing</span>
              </div>
            </div>
            <div className="mland30">
              <b>Intent.</b> Use the retry policy for every outbound call so a flaky provider cannot
              fail a payment. <b>Risk:</b> touches 3 call sites in billing.
            </div>
            <div className="mland31">
              <div className="ln del">
                <span className="n">-</span>
                <span className="c">let res = http.post(url).send().await?;</span>
              </div>
              <div className="ln add">
                <span className="n">+</span>
                <span className="c">let res = retry(policy, || http.post(url).send()).await?;</span>
              </div>
            </div>
            <div className="mland32">
              <b>You:</b> is a 30 s ceiling right for refunds?
            </div>
          </section>
          <section className="mland33">
            <div className="mland34">Works with the tools your team already uses</div>
            <div className="mland35">
              <span className="mono mland36">git</span>
              <span className="mono mland36">VS Code</span>
              <span className="mono mland36">JetBrains</span>
              <span className="mono mland36">MCP</span>
            </div>
          </section>
          <div className="mland37">Stacked changes</div>
          <h2 className="mland38">Small changes. Big features. No giant pull requests.</h2>
          <p className="mland39">
            Break work into a stack. Each change is reviewed on its own, merges in order, and
            restacks itself when something under it moves.
          </p>
          <section className="mland40">
            <h3 className="mland41">Restack on its own</h3>
            <div className="mland42">
              Mid-stack edits restack everything above, automatically. Approvals survive a restack
              when the diff did not change.
            </div>
          </section>
          <section className="mland40">
            <h3 className="mland41">Merge in order</h3>
            <div className="mland42">
              Merge the whole stack, or the bottom N, in one click. Merge order is enforced.
            </div>
          </section>
          <section className="mland40">
            <h3 className="mland41">Small to read</h3>
            <div className="mland42">Reviewers see 80 lines at a time, not 2,000.</div>
          </section>
          <div className="mland37">Review that follows the change</div>
          <h2 className="mland38">Built for reading, not just approving.</h2>
          <section className="mland40">
            <h3 className="mland41">Intent first</h3>
            <div className="mland42">
              Every change opens with its purpose, a risk summary and what changed since your last
              look. Files group by meaning, not alphabet.
            </div>
          </section>
          <section className="mland40">
            <h3 className="mland41">Comments that act</h3>
            <div className="mland42">
              Turn a comment into a request. Get a new version, and compare it to the last in one
              click.
            </div>
          </section>
          <section className="mland40">
            <h3 className="mland41">An inbox that knows whose turn it is</h3>
            <div className="mland42">
              Four lists: needs you, waiting on others, blocked on a decision, and done. Snooze, pin
              and clear with one gesture.
            </div>
          </section>
          <section className="mland40">
            <h3 className="mland41">Checks you can trust</h3>
            <div className="mland42">
              CI results attach to the exact version you are reading. Stale results are marked as
              stale instead of passing silently.
            </div>
          </section>
          {agents ? (
            <>
              <section className="mland40">
                <div className="mland43">Agents, optional</div>
                <h2 className="mland44">Give an agent a task. Keep the steering wheel.</h2>
                <div className="mland42">
                  Every run has a scope, a budget and checkpoints. Pause it, redirect it, or reject
                  a step. Credentials are scoped per run and expire when it ends.
                </div>
                <div className="mland45">
                  <div className="mland46">
                    <b className="mland47">Run 214: add retries to the billing client</b>
                    <span className="mland48">Running</span>
                  </div>
                  <div className="mland49">
                    <div className="mland50" />
                  </div>
                  <div className="mono mland51">58% of token budget · 6 min of 20</div>
                </div>
              </section>
            </>
          ) : null}
          <section className="mland40">
            <h3 className="mland41">Git compatible</h3>
            <div className="mland42">
              Your clients keep working. Clone, fetch and push with git over HTTPS or SSH today.
              Move to the <span className="mono">asc</span> command line when you want stacks, undo,
              and live restack.
            </div>
            <div className="mland52">
              <div className="mono mland53">
                <span className="mland17">$</span> git push origin HEAD:refs/for/main
              </div>
            </div>
          </section>
          <section className="mland54">
            <h2 className="mland55">Ship more, and still understand every line.</h2>
            <p className="mland56">Import a repository in a minute. Keep your git history.</p>
            <Link to="/signin" className="tap mland57">
              Create account
            </Link>
          </section>
          <footer className="mland58">
            <div className="mland59">ASC Hub · Based on Forgejo</div>
            <div className="mland60">
              <a href="#docs" className="tap mland61">
                Docs
              </a>
              <a href="#api" className="tap mland61">
                API
              </a>
              <a href="#status" className="tap mland61">
                Status
              </a>
              <a href="#licenses" className="tap mland61">
                Licenses
              </a>
              <a href="#privacy" className="tap mland62">
                Privacy
              </a>
            </div>
          </footer>
        </main>
        <div className="mland63">
          <div className="mland64">
            <Link to="/signin" className="tap mland13">
              Start free
            </Link>
          </div>
          <nav className="mland65" aria-label="Primary">
            <Link to="/" className="tap mland66" aria-current="page">
              <span className="mland67">
                <svg
                  width="22"
                  height="22"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1.8"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  aria-hidden="true"
                >
                  <path d="M4 11l8-7 8 7v9a1 1 0 0 1-1 1h-4v-6H9v6H5a1 1 0 0 1-1-1z" />
                </svg>
              </span>
              Home
            </Link>
            <Link to="/inbox" className="tap mland68">
              <span className="mland67">
                <svg
                  width="22"
                  height="22"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1.8"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  aria-hidden="true"
                >
                  <path d="M4 13l2-8h12l2 8v6a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1z" />
                  <path d="M4 13h5l1 2h4l1-2h5" />
                </svg>
              </span>
              Inbox
            </Link>
            <Link to="/search" className="tap mland68">
              <span className="mland67">
                <svg
                  width="22"
                  height="22"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1.8"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  aria-hidden="true"
                >
                  <circle cx="11" cy="11" r="6.5" />
                  <path d="M16 16l4.5 4.5" />
                </svg>
              </span>
              Search
            </Link>
            <Link to="/signin" className="tap mland68">
              <span className="mland67">
                <svg
                  width="22"
                  height="22"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1.8"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  aria-hidden="true"
                >
                  <circle cx="12" cy="8" r="4" />
                  <path d="M4 20c1-4 4-6 8-6s7 2 8 6" />
                </svg>
              </span>
              Me
            </Link>
          </nav>
        </div>
      </div>

      <MarketingMenu open={menu} onClose={() => setMenu(false)} />
    </>
  );
}
