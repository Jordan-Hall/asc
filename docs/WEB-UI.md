# asc web UI

Status: design plus v1 scaffold. This file is the contract for `web/` and `crates/asc-serve/src/web/`. It does not change `SPEC.md` or `PLAN.md`; section "Prerequisites and open decisions" lists the SPEC points it needs amended.

## 1. Goals and non-goals

Goal: a GitHub-quality web UI, served by `asc serve`, that makes the Forgejo server-rendered pages unnecessary for reading and reviewing code.

- GitHub-like information architecture: repo home (file tree plus README), code view (blob, blame, history), changes and stack review (threaded comments, inline diff), CI runs, inbox, search, settings.
- Fast by construction: route-level code splitting, lazy panes, virtualised trees, diffs and logs, streaming where the RPC streams, prefetch on hover and focus, skeletons with reserved space, no layout shift.
- Host-neutral: the app talks only to the asc RPC services (Repo, Changes, Review, Inbox, Ci, CodeIntel, Cloud) through `@asc/asc-client`. No Forgejo REST client, no Forgejo names, no `/api/v1`. ASC Hub (Forgejo) is one backend that happens to serve the same RPC.
- Accessible and keyboard-first: WCAG 2.2 AA minimum, AAA contrast where the asc tokens give it, GitHub-style `t`, `g` chords, `?` help.

Non-goals for v1: editing files in the browser, issue tracking, wiki, organisation and user administration (those stay on the forge), real-time presence, a plugin API.

## 2. Stack

Matches `D:/dev/monorepo` (checked against its `package.json` files) and this repository's tooling.

| Layer | Choice | Why |
|---|---|---|
| UI | React 19.2 | monorepo uses `^19.2.0` |
| Build | Rsbuild 1.7 (`@rsbuild/core`, `@rsbuild/plugin-react`) | monorepo builds every app with Rsbuild, not Vite; content-hashed output and chunk splitting built in |
| CSS | Tailwind 4.1 through `@tailwindcss/postcss`, tokens from asc `theme.css` | monorepo pattern: `@theme` plus RGB-channel custom properties |
| Language | TypeScript 7 (the root pin), `tsconfig.base.json` | repo convention |
| Package manager and tasks | bun 1.4, turbo (`build`, `typecheck`, `test`, `lint`) | repo convention; `web` is a workspace member |
| Lint and format | oxlint (root `.oxlintrc.json`), biome format | repo convention |
| Tests | `bun test`; components are rendered to strings with `react-dom/server`; Playwright for e2e later | `asc-ui` and `asc-client` already use `bun test` and Playwright |
| RPC | `@asc/asc-client` (`callUnary`, `callStream`, `grpcWeb`) | the one TS client |

Routing, the query cache and virtualisation are small in-repo modules (`web/src/lib/*`, each under 200 lines, unit-tested) rather than dependencies: the budget in section 6 is tight, the needs are narrow, and each module can be swapped for a library later behind the same hook signatures.

## 3. Information architecture and routes

The app is mounted under the base path `/ui`. Paths below are relative to it. `:ref` is a branch, tag, change id or commit id; a ref containing `/` is percent-encoded as one segment.

| Route | Screen | Chunk | v1 |
|---|---|---|---|
| `/` | Dashboard: repos you can read, recent changes, inbox count | `home` | yes |
| `/:owner/:repo` | Repo home: branch picker, file tree of the default ref, README, last commit, clone panel | `repo` | yes |
| `/:owner/:repo/tree/:ref/*path` | Directory view (same pane as repo home) | `repo` | yes |
| `/:owner/:repo/blob/:ref/*path` | Code view: virtualised lines, line anchors, raw link | `blob` | yes |
| `/:owner/:repo/blame/:ref/*path` | Blame gutter over the code view | `blame` | later (v1.1) |
| `/:owner/:repo/commits/:ref/*path` | History, streamed, virtualised | `history` | later (v1.1) |
| `/:owner/:repo/commit/:id` | One commit with diff | `diff` | later (v1.1) |
| `/:owner/:repo/changes` | Change list: stacks of the default target, review state, mergeability | `changes` | yes |
| `/:owner/:repo/changes/:id` | Change review: stack panel, versions, files, threads, inline diff | `review` | later (v1.2) |
| `/:owner/:repo/actions` and `/actions/runs/:run` | CI runs and job logs (tail streaming) | `ci` | later (v1.3) |
| `/:owner/:repo/search` and `/search` | Code search, symbols, references | `search` | later (v1.3) |
| `/inbox` | Review requests, mentions, pinned and snoozed | `inbox` | later (v1.2) |
| `/settings` | Token, theme, shortcuts; repo settings under `/:owner/:repo/settings` | `settings` | v1: token and theme only |
| `/signin` | Token entry | in the entry chunk | yes |
| anything else | Not found | entry chunk | yes |

Rows marked "later" exist in the route table as a lazy `Planned` screen, so links, shortcuts and the nav are final from v1 and only the screen body changes.

Shell: top bar (logo, global search `/`, inbox, theme, user menu), repo header with tabs (Code, Changes, Actions, Search, Settings), skip link, `main` landmark. The shell is in the entry chunk and renders before any RPC returns.

### Keyboard

A single key manager (`web/src/lib/keys.ts`) owns all shortcuts; it ignores keys typed in inputs, textareas and `contenteditable`, and any chord with a modifier except where stated.

| Keys | Action |
|---|---|
| `?` | Shortcut help dialog |
| `/` | Focus global search |
| `t` | File finder for the current repo and ref (fuzzy, over the recursive tree, lazy chunk) |
| `g` then `c` | Go to Code |
| `g` then `p` | Go to Changes |
| `g` then `a` | Go to Actions |
| `g` then `i` | Go to Inbox |
| `g` then `s` | Go to Settings |
| `g` then `h` | Go to Dashboard |
| `j` / `k` | Next and previous row in a list |
| `Enter` | Open the focused row |
| `y` | Permalink: replace the ref with the commit id |
| `.` | Reserved: open in the editor (later) |

A chord expires after 1.5 s. Every shortcut also has a visible control. Focus is managed on route change (the page heading receives focus and is announced through a polite live region).

## 4. Data loading and caching

### 4.1 Transport

```
screen -> hook (useQuery / useStream) -> api module -> callUnary / callStream -> BridgeTransport
                                                         grpcWeb (production) | mock transport (tests, `?mock`)
```

- The api modules (`web/src/api/repo.ts`, `changes.ts`) are the only code that knows service names. They return plain view models, not protobuf messages, so screens never import generated code.
- `Api` is created once with a `BridgeTransport`. Production: `grpcWeb({ baseUrl: location.origin, headers })`, where `headers()` adds `authorization: Bearer <token>` per call. Tests and the dev build with `?mock`: `mockTransport(fixtures)`, which implements `BridgeTransport` by decoding the request with the generated schema and answering with encoded fixture messages, so the real encode/decode path is exercised.
- Repos are addressed with `RepoRef.name = "owner/repo"` and `id_mode = ID_MODE_GIT` for git-compatible repos; the default is the repo's own mode from `RepoConfig`.

### 4.2 Content addressing is the cache

asc ids are content hashes. The loader resolves a ref to an id once (`Resolve`, short stale time), then every read keyed by that id (`ReadTree`, `ReadFile`, `Log` from a commit, `Diff` between two ids) is immutable and cached forever. Mutable reads are only ref lookups: `ListRefs`, `Stack`, `Resolve`, `Mergeability`, `Inbox.Count`.

| Class | Examples | Fresh for | Revalidate |
|---|---|---|---|
| Immutable | tree, file, log, diff by id | forever (LRU by bytes) | never |
| Ref lookup | `ListRefs`, `Resolve`, `Stack` | 10 s | on focus, on navigation, on `WatchRefs` event |
| Expensive derived | `Mergeability`, `InterDiff` | 60 s | on ref event for the change |
| Counters | `Inbox.Count` | 30 s poll while visible | on focus |

### 4.3 Query cache (`web/src/lib/query.ts`)

- Keyed by `[service, method, canonical request]`; one in-flight promise per key (dedupe); stale-while-revalidate; `useSyncExternalStore` so components re-render only for their key.
- `useQuery(key, fetcher, opts)` for unary calls. `useStream(key, streamFn, reducer)` for server streams: batches are appended with `requestAnimationFrame` coalescing, the first batch renders the first screenful, the rest fills in without layout change (rows have fixed heights).
- Abort: leaving a route aborts its streams through the `AbortSignal` that `callStream` already takes.
- Errors carry the gRPC code (`BridgeError.code`): `UNAUTHENTICATED` goes to `/signin`, `PERMISSION_DENIED` and `NOT_FOUND` render the same "not found" page (no existence oracle), `UNAVAILABLE` retries with backoff and shows an inline retry.
- Persistence (IndexedDB for immutable entries) is v1.1.

### 4.4 Route loaders and prefetch

- Each route declares `load(params, ctx)` that starts its queries and `component: () => import(...)`. Navigation starts both in parallel; the old page stays until the new chunk is ready or 120 ms passes, then the new route's skeleton shows (never a blank page).
- Prefetch triggers: pointer hover or keyboard focus on a `Link` for 65 ms (intent), `touchstart`, a row entering the viewport for tree rows (loads the directory listing, not the file), and idle-time `modulepreload` of the likely next chunk (repo home preloads `blob` and `changes`).
- Prefetch only touches the query cache; it never mutates, and it is skipped under `navigator.connection.saveData`.

### 4.5 Rendering rules that prevent layout shift

- Skeletons are drawn with the real dimensions of the thing they replace (row height, header height); the asc `--color-skeleton-rgb` tokens are used so they are visible in both themes.
- Fixed row heights for tree rows (36 px), code lines (20 px), history rows (56 px), change rows (64 px); the virtualiser needs no measuring.
- Images and avatars have explicit width and height; fonts are system fonts in v1 (no font swap shift).
- Scrollbar gutter is reserved (`scrollbar-gutter: stable`).

## 5. Virtualisation

`web/src/lib/virtual.ts` exposes `windowRange(scrollTop, viewport, rowHeight, count, overscan)` (pure, tested) and `useVirtualRows`. Rules:

- Any list that can exceed 100 rows is virtualised: file tree (recursive tree of 100k paths), blob lines, history, change list, diff hunks, CI logs.
- The DOM holds at most about 200 rows regardless of data size; rows are absolutely positioned inside a spacer whose height is `count * rowHeight` (set through the CSSOM, which the strict CSP allows).
- Data arrives in pages or stream batches into a chunked store (array of arrays), so appending a batch does not copy the whole list.
- Find-in-page: the code view offers its own in-view search (`Ctrl+F` handoff to the lazy search pane) because off-screen rows are not in the DOM; the browser's native find still works on the visible window.
- Code lines render as text nodes. Syntax highlighting is a lazy worker chunk (v1.1) that returns token ranges per line window; v1 ships plain monospace.

## 6. Performance budgets

Budgets are enforced by `bun run --cwd web size` (part of `build`), which fails the build when exceeded. All JS and CSS numbers are gzip.

| Budget | Limit |
|---|---|
| Initial JS (entry plus every chunk the entry statically needs) | < 150 KB (target 100 KB: React and react-dom about 60 KB, shell, router, cache, theme) |
| Initial CSS | < 20 KB |
| Any route chunk | < 40 KB; diff, syntax and markdown chunks may be 80 KB, loaded only on demand |
| Total transfer to first meaningful content on repo home, cold cache | < 250 KB including RPC replies |
| LCP, repo home, p75, mid-tier mobile on a fast 4G profile | < 1.8 s cold, < 1.0 s with a warm HTTP cache |
| INP | < 200 ms |
| CLS | 0 target, < 0.02 hard limit |
| Scroll through 1M-line file or 100k-row tree | 60 fps; DOM rows < 300 |
| Time from hover to navigation complete after prefetch | < 100 ms for cached routes |

How the budgets are met: no UI kit, no icon font (inline SVG sprite, only used icons), no moment/lodash, protobuf descriptors imported per service from subpaths (`@asc/asc-client/repo`) so only the services a route uses are in its chunk, markdown and syntax highlighting behind dynamic imports, shell critical CSS is the same file (one CSS asset, `preload`ed), `theme-init.js` is a tiny external blocking script that sets the theme class before first paint.

## 7. Theme

ASC tokens from `packages/ui/src/theme.css` of monorepo, trimmed to those the app uses, in `web/src/styles/theme.css`: RGB-channel custom properties (`--color-primary: 18 84 105`, `--color-primary-button: 21 94 117` which is `#155e75`) in `:root`, overridden in `.dark` (primary `34 211 238`, which is `#22d3ee`). Tailwind colours are mapped as `rgb(var(--color-x) / <alpha-value>)`. Modes: light, dark, system (default); the choice is stored in `localStorage` (a per-viewer convenience, wrapped in try/catch) and applied before paint by `theme-init.js`. `prefers-reduced-motion` and `forced-colors` are honoured. Contrast is checked by a unit test over the token pairs the app uses (4.5:1 text, 3:1 controls) in both modes.

## 8. Embedding in `asc serve`

Code: `crates/asc-serve/src/web/` (`mod.rs`, `assets.rs`; the asset table is generated by `crates/asc-serve/build.rs`).

Build and embed:

1. `bun run --cwd web build` runs Rsbuild, then `scripts/post-build.ts`: writes `.br` and `.gz` siblings for every compressible file and `dist/manifest.json` (path, size, sha256, content type, hashed or not).
2. `build.rs` reads `web/dist` (override: `ASC_WEB_DIST`), and generates `$OUT_DIR/web_assets.rs`: a static table of `include_bytes!` for identity, br and gz bodies, with the content type, a strong ETag (sha256 prefix) and the hashed flag. It emits `cargo:rerun-if-changed` for the directory. With no `dist`, the table is empty and the crate still builds, so `cargo check` and CI jobs without bun are unaffected.
3. `web::register` mounts the routes when `[web] enabled` is true. If it is enabled and the table is empty and `[web] dir` is unset, startup fails with a clear error (an enabled UI that serves nothing is a misconfiguration).

Serving rules:

| Request | Response |
|---|---|
| `GET /ui/static/**` (content-hashed) | 200, `Cache-Control: public, max-age=31536000, immutable`, `ETag`, `Vary: Accept-Encoding`; best of br, gzip, identity |
| `GET /ui/`, any extension-less `GET /ui/**` that accepts `text/html` | `index.html`: `Cache-Control: no-cache`, strong `ETag`, `If-None-Match` gives 304 |
| other unhashed files (`/ui/theme-init.js`, `/ui/favicon.svg`) | `Cache-Control: no-cache` plus ETag |
| unknown path with an extension | 404 (never the SPA shell) |
| non-GET/HEAD | 405 with `Allow: GET, HEAD` |

Security headers on every `/ui` response:

```
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self';
  img-src 'self' data:; font-src 'self'; connect-src 'self'; manifest-src 'self';
  worker-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Resource-Policy: same-origin
Permissions-Policy: camera=(), microphone=(), geolocation=(), interest-cohort=()
X-Frame-Options: DENY
```

No inline script and no `unsafe-inline` or `unsafe-eval`: the HTML has no inline script, the theme bootstrap is an external file, the app sets styles through the CSSOM (allowed under `style-src 'self'`), and does not use `dangerouslySetInnerHTML` (markdown renders to React elements). A repo test asserts the built `index.html` has no inline `<script>` or `style=`. `connect-src 'self'` means the UI can only talk to the origin that served it; a deployment that fronts RPC elsewhere must set `[web] connect_src` (v1.1) rather than loosen the default. HSTS is left to the TLS terminator.

Paths: the UI base is `/ui`. It cannot collide with `/<owner>/<repo>/info/refs`, the pack paths, `/asc.v1.*` RPC paths, `/healthz`, `/readyz` or `/.well-known/asc`. Mounting at `/` with GitHub-style `/owner/repo` URLs is later: it needs the fallback to exclude those paths explicitly and an owner namespace reserved word list (`ui`, `static`).

Config (`asc-serve.toml`):

```toml
[web]
enabled = false   # serve the web UI under /ui; default off (on in the staging example)
# dir = "/opt/asc/web"   # serve this directory instead of the embedded build (development, hotfix)
```

The default in code is `false`. The staging example (`deploy/asc-serve.toml.example`) shows it enabled, since the UI is read-only and anonymous visitors only receive static files; every data call needs a credential.

## 9. Authentication

The UI is a pure client of the RPC; it has no session of its own and holds no ambient credential, so there is no CSRF surface.

- v1: the sign-in screen takes an asc token (created with `asc serve token create` against the token store, or a forge-issued token when `[auth] provider = "forge"`). The token is held in memory and, if "stay signed in for this tab" is ticked, in `sessionStorage`; never `localStorage`, never a URL or query string (the server rejects tokens in queries anyway, `authn.rs`). Sent only as `Authorization: Bearer` by the `grpcWeb` `headers()` callback. Sign out clears both. An `UNAUTHENTICATED` reply clears the token and routes to `/signin`.
- Anonymous: public repos work without a token if the authorizer allows anonymous reads; the shell shows "Sign in".
- Later: OAuth 2 authorization-code with PKCE as a public client (the same pattern as the `asc-ide` client, P06) so users never paste a token; a short-lived token minted by the token store; passkeys through the forge when it is the identity provider.
- The UI never prints, logs or echoes a token; error reports redact `authorization`.

## 10. Prerequisites and open decisions

These need a decision from the SPEC owner before the UI can leave staging:

1. **Browser RPC endpoint.** SPEC section 24 records "no `tonic-web`; the browser reaches asc through the Forgejo proxy P04". This design needs a gRPC-Web endpoint on the server the UI is served from. Options: (a) add a `tonic-web` layer to `asc serve` (read allowlist only, same `allowlist.json` as P04), (b) keep the proxy and point `grpcWeb` at it (`[web] rpc_base`), losing host neutrality. The scaffold works with either because the transport is a `BridgeTransport`; the UI is tested against a mock until one exists.
2. **Generated clients.** `review`, `inbox`, `ci` have no generated TS yet (`proto/buf.gen.ts.yaml` generates all protos but the `@asc/asc-client` exports list only repo, changes, cloud, codeintel, projects, workspace). Add subpath exports for the three before v1.2.
3. **Change titles.** `ChangeInfo` has no title; the change list joins `Stack` with `Log` (`LogEntry.change`, `message`). A title field on `ChangeInfo` would remove the second call.
4. **Mount path.** `/ui` now, `/` later (section 8).
5. **Allowlist ownership.** The write RPCs the UI needs from v1.2 (`AddComment`, `ResolveThread`, `SetState`, `RequestMerge`) need the same per-method allowlist and idempotency (`x-asc-op-id`) treatment as the IDE relay.

## 11. v1 and later

v1 (this scaffold): shell, router, theme, sign-in, dashboard stub, repo home with virtualised file tree and README (lazy markdown renderer), blob view (virtualised, streamed), change list over `Changes.Stack`, settings (token, theme), shortcuts and help, mock transport, tests, size gate, embed and serving in `asc serve`, CSP.

v1.1: history, blame, commit view and diff (virtualised, split and unified), syntax highlighting worker, `WatchRefs` live updates, IndexedDB cache, `[web] connect_src`.
v1.2: change review (stack panel, versions and interdiff, threaded comments, resolve, request review, merge), inbox.
v1.3: CI runs and live log tail, search and code intelligence (definitions, references, outline), repo settings.
Later: OAuth/PKCE sign-in, mount at `/`, command palette, i18n, offline shell (service worker), Trusted Types enforcement.

## 12. Testing

- Unit: router matching, query cache (dedupe, SWR, abort), virtual window maths, key chords, token contrast, markdown renderer (including hostile input), theme resolution.
- Integration: screens rendered with `react-dom/server` against the mock transport (repo home lists entries and README; change list orders a stack bottom to top; blob view streams chunks).
- Build gate: `size` (budgets above), `no inline script` check on `dist/index.html`.
- Server: `cargo test -p asc-serve web::` covers ETag/304, immutable cache headers, SPA fallback rules, 404 for missing assets, CSP header presence, encoding negotiation, method handling, and that the router is absent when `[web] enabled = false`.
- Later: Playwright with axe against `asc serve` plus the mock backend; Lighthouse CI for LCP, CLS and INP against the budgets.
