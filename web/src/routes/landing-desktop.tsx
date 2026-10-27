// The signed-out front page, as drawn in the Landing board (desktop) and Mobile-Landing (phone).
import { LogoMark } from "../components/icons";
import { Link } from "../lib/router";
import { useTitle } from "../lib/title";
import "../styles/screens/land.css";

export default function LandingDesktop() {
  useTitle("Review code at the speed your agents write it");
  return (
    <div data-board="land" className="land land1">
      <header className="land2">
        <div className="land3">
          <Link to="/" className="land4" aria-label="ASC Hub home">
            <LogoMark size={26} tone="dark" />
            ASC Hub
          </Link>
          <nav className="land5">
            <a href="#stacks" className="navl">
              Stacks
            </a>
            <a href="#review" className="navl">
              Review
            </a>
            <a href="#agents" className="navl">
              Agents
            </a>
            <a href="#search" className="navl">
              Search
            </a>
            <a href="#git" className="navl">
              Git compatible
            </a>
            <Link to="/docs" className="navl">
              Docs
            </Link>
          </nav>
          <div className="land6">
            <Link to="/signin" className="navl land7">
              Sign in
            </Link>
            <Link to="/signin" className="btn btn-p land8">
              Create account
            </Link>
          </div>
        </div>
      </header>
      <main id="main" tabIndex={-1} className="land-main">
        <section className="land9" id="top">
          <div className="land10">
            <div className="land11">
              <span className="land12" />
              Built for teams where agents write a lot of the code
            </div>
            <h1 className="land13">Review code at the speed your agents write it.</h1>
            <p className="land14">
              ASC Hub hosts your repositories as stacks of small, reviewable changes. See who or
              what made each one, why, and whose turn it is. Stay in control of every agent run.
            </p>
            <div className="land15">
              <Link to="/signin" className="btn btn-p">
                Start free
              </Link>
              <a href="#demo" className="btn btn-g">
                Watch a stack get reviewed
              </a>
            </div>
            <div className="mono land16">
              <span className="land17">$</span>
              <span>asc clone https://hub.example.com/acme/payments</span>
              <span className="land18">or</span>
              <span>git clone https://hub.example.com/acme/payments</span>
            </div>
            <div className="land19" id="demo">
              <div className="land20">
                <span className="land21" />
                <span className="land21" />
                <span className="land21" />
                <span className="mono land22">acme/payments › stack: retry-with-backoff</span>
              </div>
              <div className="land23">
                <div className="land24">
                  <div className="land25">Stack of 4</div>
                  <div className="land26">
                    <div className="land27">
                      <span className="land28" />
                      <div>
                        <div className="land29">Add retry policy type</div>
                        <div className="land30">Approved · you</div>
                      </div>
                    </div>
                    <div className="land27">
                      <span className="land28" />
                      <div>
                        <div className="land29">Backoff with jitter</div>
                        <div className="land31">Approved · agent</div>
                      </div>
                    </div>
                    <div className="land32">
                      <span className="land33" />
                      <div>
                        <div className="land29">Wire into the client</div>
                        <div className="land31">Needs your review</div>
                      </div>
                    </div>
                    <div className="land27">
                      <span className="land34" />
                      <div>
                        <div className="land29">Docs and metrics</div>
                        <div className="land30">Agent working…</div>
                      </div>
                    </div>
                  </div>
                </div>
                <div className="land35">
                  <div className="land36">
                    <span className="land37">Wire into the client</span>
                    <span className="land38">Agent: coder-2</span>
                    <span className="land39">42 tests pass</span>
                  </div>
                  <div className="land40">
                    <b className="land41">Intent.</b> Use the retry policy for every outbound call
                    so a flaky provider cannot fail a payment. <b className="land42">Risk:</b>{" "}
                    touches 3 call sites in billing.
                  </div>
                  <div className="mono land43">
                    <div className="land44">src/client.rs</div>
                    <div className="land45">- let res = http.post(url).send().await?;</div>
                    <div className="land46">
                      + let res = retry(policy, || http.post(url).send()).await?;
                    </div>
                    <div className="land47">
                      <b>You:</b> is a 30s ceiling right for refunds?{" "}
                      <span className="land48">Ask agent to change</span>
                    </div>
                    <div className="land46">+ metrics::retry_count(&res).inc();</div>
                  </div>
                </div>
              </div>
            </div>
          </div>
        </section>
        <section className="land49">
          <div className="land50">
            <div className="land51">Works with the tools your team already uses</div>
            <div className="land52">
              <span>git</span>
              <span>VS Code</span>
              <span>JetBrains</span>
              <span>Claude Code</span>
              <span>Cursor</span>
              <span>MCP</span>
            </div>
          </div>
        </section>
        <section className="land53" id="stacks">
          <div className="land54">
            <div className="land55">
              <div className="land56">Stacked changes</div>
              <h2 className="land57">Small changes. Big features. No giant pull requests.</h2>
              <p className="land58">
                Break work into a stack. Each change is reviewed on its own, merges in order, and
                restacks itself when something under it moves. Agents produce stacks naturally, so
                reviewers see 80 lines at a time, not 2,000.
              </p>
              <ul className="land59">
                <li className="land60">
                  <span className="land61">✓</span>
                  Mid-stack edits restack everything above, automatically
                </li>
                <li className="land60">
                  <span className="land61">✓</span>
                  Approvals survive a restack when the diff did not change
                </li>
                <li className="land60">
                  <span className="land61">✓</span>
                  Merge the whole stack, or the bottom N, in one click
                </li>
              </ul>
            </div>
            <div className="land62">
              <div className="land63">
                <div className="land64">
                  <div className="land65">
                    <span className="land66" />
                    <span className="land67" />
                  </div>
                  <div className="land68">
                    <div className="land69">main</div>
                    <div className="land70">trunk moved 3 commits</div>
                  </div>
                </div>
                <div className="land64">
                  <div className="land65">
                    <span className="land71" />
                    <span className="land67" />
                  </div>
                  <div className="land68">
                    <div className="land69">Add retry policy type</div>
                    <div className="land72">Approved</div>
                  </div>
                </div>
                <div className="land64">
                  <div className="land65">
                    <span className="land71" />
                    <span className="land67" />
                  </div>
                  <div className="land68">
                    <div className="land69">Backoff with jitter</div>
                    <div className="land73">Approved · restacked, approval kept</div>
                  </div>
                </div>
                <div className="land64">
                  <div className="land65">
                    <span className="land74" />
                    <span className="land67" />
                  </div>
                  <div className="land68">
                    <div className="land69">Wire into the client</div>
                    <div className="land75">Waiting on you</div>
                  </div>
                </div>
                <div className="land64">
                  <div className="land65">
                    <span className="land76" />
                  </div>
                  <div>
                    <div className="land69">Docs and metrics</div>
                    <div className="land70">Agent working</div>
                  </div>
                </div>
              </div>
            </div>
          </div>
        </section>
        <section className="land77" id="review">
          <div className="land78">
            <div className="land79">
              <div className="land56">Review that follows the change</div>
              <h2 className="land57">Built for reading, not just approving.</h2>
            </div>
            <div className="land80">
              <div className="land81">Intent first</div>
              <p className="land82">
                Every change opens with its purpose, a risk summary and what changed since your last
                look. Files group by meaning, not alphabet: behavior, tests, generated.
              </p>
            </div>
            <div className="land80">
              <div className="land81">Comments that act</div>
              <p className="land82">
                Turn a comment into an instruction. Ask the author agent to change it, get a new
                version, and compare it to the last in one click.
              </p>
            </div>
            <div className="land80">
              <div className="land81">An inbox that knows whose turn it is</div>
              <p className="land82">
                Not a notification pile. Four lists: needs you, waiting on others, agents blocked on
                a decision, and done. Snooze, pin, and clear with one key.
              </p>
            </div>
            <div className="land80">
              <div className="land81">Checks you can trust</div>
              <p className="land82">
                CI results attach to the exact version you are reading, and stale results are marked
                as stale instead of passing silently.
              </p>
            </div>
          </div>
        </section>
        <section className="land83" id="agents">
          <div className="land54">
            <div className="land55">
              <div className="land84">Agents as teammates you can supervise</div>
              <h2 className="land85">Give an agent a task. Keep the steering wheel.</h2>
              <p className="land86">
                Every run has a scope, a budget and checkpoints. Watch the plan unfold, pause it,
                redirect it, or reject a step. Credentials are scoped per run and expire when it
                ends.
              </p>
              <div className="land87">
                <div className="land88">
                  <div className="land89">Scoped access</div>
                  <div className="land90">Paths, branches, and write rights per run</div>
                </div>
                <div className="land88">
                  <div className="land89">Budgets</div>
                  <div className="land90">Tokens, minutes and changes, hard limits</div>
                </div>
                <div className="land88">
                  <div className="land89">Checkpoints</div>
                  <div className="land90">Roll back to any step, nothing is lost</div>
                </div>
                <div className="land88">
                  <div className="land89">Full audit</div>
                  <div className="land90">Every tool call, every file, every decision</div>
                </div>
              </div>
            </div>
            <div className="land91">
              <div className="land36">
                <span className="land92">Run 214: add retries to the billing client</span>
                <span className="land93">Running</span>
              </div>
              <div className="land94">
                <div className="land60">
                  <span className="land95">✓</span>
                  <span>Read the 3 call sites and existing tests</span>
                </div>
                <div className="land60">
                  <span className="land95">✓</span>
                  <span>
                    Plan: 4 changes, bottom-up. <span className="land96">Approved by you</span>
                  </span>
                </div>
                <div className="land60">
                  <span className="land95">✓</span>
                  <span>Change 3 written, 42 tests pass</span>
                </div>
                <div className="land60">
                  <span className="land17">●</span>
                  <span>Writing docs and metrics</span>
                </div>
              </div>
              <div className="land97">
                <div className="land98" />
              </div>
              <div className="mono land99">
                <span>58% of token budget</span>
                <span>6 min of 20</span>
              </div>
              <div className="land100">
                <span className="land101">Pause</span>
                <span className="land101">Steer</span>
                <span className="land102">Stop</span>
              </div>
            </div>
          </div>
        </section>
        <section className="land103" id="search">
          <div className="land78">
            <div className="land104">
              <div className="land56">Code intelligence</div>
              <div className="land105">Find by meaning, by name, or by text.</div>
              <p className="land106">
                Symbols, references and semantic search across every branch and change, ready for
                you and for your agents over MCP.
              </p>
              <div className="mono land107">› where do we retry failed payments?</div>
            </div>
            <div className="land104" id="git">
              <div className="land56">Git compatible</div>
              <div className="land105">Your clients keep working.</div>
              <p className="land106">
                Clone, fetch and push with git over HTTPS or SSH today. Move to the asc command line
                when you want stacks, undo, and live restack.
              </p>
              <div className="mono land108">
                <span className="land17">$</span> git push origin HEAD:refs/for/master
              </div>
            </div>
          </div>
        </section>
        <section className="land109">
          <div className="land110">
            <div className="land111">
              <h2 className="land112">Ship more, and still understand every line.</h2>
              <p className="land113">
                Import a repository in a minute. Keep your git history. Bring your agents.
              </p>
              <div className="land114">
                <Link to="/signin" className="btn btn-p">
                  Create account
                </Link>
                <Link to="/docs" className="btn btn-g">
                  Read the docs
                </Link>
              </div>
            </div>
          </div>
        </section>
      </main>
      <footer className="land115">
        <div className="land116">
          <div className="land117">
            ASC Hub <span className="land118">· Based on Forgejo</span>
          </div>
          <div className="land119">
            <Link to="/docs">Docs</Link>
            <Link to="/install">Install</Link>
            <Link to="/licenses">Licenses</Link>
            <Link to="/privacy">Privacy</Link>
          </div>
        </div>
      </footer>
    </div>
  );
}
