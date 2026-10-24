// Commit graph DOM view (SPEC §24.4.5 "Graph rendering", §24.4.10). Vanilla DOM; every server
// string is set through `textContent`. Rows are virtualised (a pooled window of absolutely
// positioned elements with overscan 20), one sticky <canvas> draws the lanes for the viewport,
// and at most 5,000 rows stay in memory (PageStore). The minimap reads the first page's total
// and ref markers.
import type { RunOutcome } from "../../ops/queue";
import type { Severity } from "../../platform";
import {
  type GraphPage,
  type GraphPreset,
  type GraphQuery,
  type GraphRow,
  type GraphVerbCall,
  laneCount,
  type MenuItem,
  menuCall,
  menuItems,
  minimapModel,
  minimapRow,
  navigate,
  type NavKey,
  PAGE_SIZE,
  PageStore,
  parseRefList,
  searchRow,
  shortId,
  visibleRange,
} from "./model";
import { LANE_WIDTH, ROW_HEIGHT } from "./style";

export interface GraphHost {
  /** One `CommitGraph` page. Rejects on error; honours `signal`. */
  fetchPage(query: GraphQuery, signal: AbortSignal): Promise<GraphPage>;
  /** Run cherry-pick or revert through `Bridge.Run`; the host composes the confirmation text. */
  run(call: GraphVerbCall): Promise<RunOutcome>;
  copy(text: string): Promise<void>;
  notify(severity: Severity, message: string): void;
  /** A row was selected (details panes follow it). */
  onSelect?(row: GraphRow): void;
  /** Enter on a row. */
  openDetails?(row: GraphRow): void;
  /** Per-view state (scroll and selection); never holds secrets. */
  getState?(): { readonly scrollTop?: number; readonly selected?: string } | undefined;
  setState?(state: { readonly scrollTop: number; readonly selected?: string }): void;
}

export interface GraphView {
  /** `Watch` event: refetch the first page only and splice by commit id. */
  refreshHead(): Promise<void>;
  setFilters(f: { preset?: GraphPreset; include?: readonly string[]; exclude?: readonly string[] }): void;
  select(index: number): void;
  readonly selectedIndex: number;
  /** Rows currently in memory (≤ 5,000). */
  readonly heldRows: number;
  readonly rowCount: number;
  /** Resolves when no page fetch is outstanding. */
  idle(): Promise<void>;
  dispose(): void;
}

const PALETTE = ["#4e9af1", "#e5a13a", "#4cb782", "#d6609b", "#9b7fe8", "#3cc0c8", "#d9715b", "#98b540"];
const OVERSCAN = 20;

const el = <K extends keyof HTMLElementTagNameMap>(doc: Document, tag: K, cls?: string): HTMLElementTagNameMap[K] => {
  const n = doc.createElement(tag);
  if (cls) n.className = cls;
  return n;
};

const laneX = (c: number): number => 8 + c * LANE_WIDTH + LANE_WIDTH / 2;

const dateText = (t: number): string => (t > 0 ? new Date(t).toISOString().slice(0, 10) : "");

export function mountGraph(container: HTMLElement, host: GraphHost): GraphView {
  const doc = container.ownerDocument;
  const store = new PageStore();
  let preset: GraphPreset = "all";
  let include: readonly string[] = [];
  let exclude: readonly string[] = [];
  let gen = 0;
  let abort = new AbortController();
  let pumping: Promise<void> | undefined;
  let selected = -1;
  let selectedId: string | undefined;
  let lanes = 1;
  let rafPending = false;
  let disposed = false;
  let painted = false;
  let canvasWarm = false;
  let menu: HTMLElement | undefined;
  let matchIdx = -1;
  const pool = new Map<number, HTMLElement>();

  const root = el(doc, "div", "asc-graph");
  root.dataset.testid = "graph";
  const bar = el(doc, "div", "asc-graph-bar");
  const presetSel = el(doc, "select");
  presetSel.setAttribute("aria-label", "Preset");
  for (const p of ["all", "current", "smart", "stack"] as const) {
    const o = el(doc, "option");
    o.value = p;
    o.textContent = p;
    presetSel.append(o);
  }
  const includeIn = el(doc, "input");
  includeIn.placeholder = "include refs";
  includeIn.setAttribute("aria-label", "Include refs");
  const excludeIn = el(doc, "input");
  excludeIn.placeholder = "exclude refs";
  excludeIn.setAttribute("aria-label", "Exclude refs");
  const searchIn = el(doc, "input");
  searchIn.placeholder = "search (/)";
  searchIn.setAttribute("aria-label", "Search loaded rows");
  const status = el(doc, "span", "asc-gstatus");
  status.dataset.testid = "graph-status";
  status.setAttribute("role", "status");
  bar.append(presetSel, includeIn, excludeIn, searchIn, status);

  const main = el(doc, "div", "asc-graph-main");
  const scroller = el(doc, "div", "asc-graph-scroll");
  scroller.tabIndex = 0;
  scroller.setAttribute("role", "listbox");
  scroller.setAttribute("aria-label", "Commit graph");
  scroller.dataset.testid = "graph-scroll";
  const lanesBox = el(doc, "div", "asc-graph-lanes");
  const canvas = el(doc, "canvas");
  canvas.setAttribute("aria-hidden", "true");
  lanesBox.append(canvas);
  const spacer = el(doc, "div");
  spacer.style.position = "relative";
  scroller.append(lanesBox, spacer);
  const mini = el(doc, "div", "asc-minimap");
  mini.dataset.testid = "minimap";
  const miniCanvas = el(doc, "canvas");
  miniCanvas.setAttribute("aria-hidden", "true");
  mini.append(miniCanvas);
  main.append(scroller, mini);
  root.append(bar, main);
  container.append(root);

  const setStatus = (text: string): void => {
    status.textContent = text;
  };

  const lanesWidth = (): number => 8 + lanes * LANE_WIDTH;

  function query(token: string): GraphQuery {
    return { preset, include, exclude, pageToken: token, limit: PAGE_SIZE };
  }

  function viewportRows(): { top: number; rows: number } {
    const h = scroller.clientHeight || 400;
    return { top: Math.floor(scroller.scrollTop / ROW_HEIGHT), rows: Math.ceil(h / ROW_HEIGHT) };
  }

  // ---- loading ----
  function missingPage(): number | undefined {
    const { top, rows } = viewportRows();
    const count = store.rowCount;
    const r = visibleRange(scroller.scrollTop, scroller.clientHeight || 400, ROW_HEIGHT, count, OVERSCAN);
    const first = Math.floor(r.first / PAGE_SIZE);
    const last = Math.floor(Math.max(r.first, r.end - 1) / PAGE_SIZE);
    store.setFocus(top + Math.floor(rows / 2));
    for (let p = first; p <= last; p++) {
      if (!store.hasPage(p) && !store.isBeyondEnd(p)) return p;
    }
    return undefined;
  }

  async function fetchOne(p: number, myGen: number): Promise<void> {
    const token = store.tokenFor(p);
    if (token === undefined) return;
    const page = await host.fetchPage(query(token), abort.signal);
    if (myGen !== gen || disposed) return;
    store.add(p, page);
  }

  function pump(): Promise<void> {
    if (pumping) return pumping;
    const myGen = gen;
    pumping = (async () => {
      try {
        for (;;) {
          const want = missingPage();
          if (want === undefined || myGen !== gen || disposed) break;
          // Page tokens form a chain: reach page `want` by walking from the frontier.
          const p = Math.min(want, store.reachablePage);
          if (store.hasPage(p) && p < want) break; // chain ended before the wanted page
          await fetchOne(p, myGen);
          if (myGen !== gen || disposed) break;
          afterStore();
        }
        if (myGen === gen) setStatus(statusText());
      } catch (e) {
        if (myGen === gen && !disposed && !(e instanceof DOMException && e.name === "AbortError")) {
          setStatus(`Could not load commits: ${e instanceof Error ? e.message : String(e)}`);
          host.notify("error", "Could not load the commit graph");
        }
      } finally {
        pumping = undefined;
      }
    })();
    return pumping;
  }

  function statusText(): string {
    const total = store.total;
    return total === undefined ? `${store.heldRows} rows loaded` : `${total} commits`;
  }

  function afterStore(): void {
    const rows: (GraphRow | undefined)[] = [];
    for (const p of store.heldPages) {
      for (let k = 0; k < store.pageLength(p); k++) rows.push(store.rowAt(p * PAGE_SIZE + k));
    }
    lanes = laneCount(rows);
    if (selectedId !== undefined) {
      const idx = store.indexOf(selectedId);
      if (idx !== undefined) selected = idx;
    }
    // The first rows paint synchronously with the data; later changes coalesce on a frame.
    if (!painted && store.rowAt(0) !== undefined) {
      painted = true;
      render();
      return;
    }
    schedule();
  }

  // ---- rendering ----
  function schedule(): void {
    if (rafPending || disposed) return;
    rafPending = true;
    const win = doc.defaultView;
    const run = (): void => {
      rafPending = false;
      if (!disposed) render();
    };
    if (win) win.requestAnimationFrame(run);
    else setTimeout(run, 16);
  }

  function fillRow(node: HTMLElement, i: number, row: GraphRow | undefined): void {
    const key = row ? `${row.id}|${i === selected}|${i === matchIdx}|${lanes}` : `pending|${lanes}`;
    if (node.dataset.key === key) return;
    node.dataset.key = key;
    node.textContent = "";
    node.style.paddingLeft = `${lanesWidth() + 4}px`;
    node.classList.toggle("asc-pending", row === undefined);
    node.classList.toggle("asc-selected", i === selected);
    node.classList.toggle("asc-match", i === matchIdx);
    node.classList.toggle("asc-wip", row?.wip === true);
    node.setAttribute("aria-selected", String(i === selected));
    if (!row) {
      const s = el(doc, "span", "asc-gsubject");
      s.textContent = "…";
      node.append(s);
      node.removeAttribute("data-testid");
      return;
    }
    node.dataset.testid = `grow-${shortId(row.id)}`;
    for (const r of row.refs) {
      const b = el(doc, "span", "asc-gref");
      b.textContent = r;
      node.append(b);
    }
    const subject = el(doc, "span", "asc-gsubject");
    subject.textContent = row.subject;
    node.append(subject);
    if (row.review) {
      const pr = el(doc, "span", "asc-gref");
      pr.dataset.badge = "pr";
      pr.textContent = `#${row.review.number} ${row.review.draft ? "draft" : row.review.state}`;
      node.append(pr);
    }
    const author = el(doc, "span", "asc-gmeta");
    author.textContent = row.author;
    const date = el(doc, "span", "asc-gmeta");
    date.textContent = dateText(row.time);
    const id = el(doc, "span", "asc-gmeta asc-gid");
    id.textContent = row.wip ? "" : shortId(row.id);
    node.append(author, date, id);
  }

  function render(): void {
    const count = store.rowCount;
    const vp = scroller.clientHeight || 400;
    spacer.style.height = `${count * ROW_HEIGHT}px`;
    const r = visibleRange(scroller.scrollTop, vp, ROW_HEIGHT, count, OVERSCAN);
    for (const [i, node] of pool) {
      if (i < r.first || i >= r.end) {
        node.remove();
        pool.delete(i);
      }
    }
    for (let i = r.first; i < r.end; i++) {
      let node = pool.get(i);
      if (!node) {
        node = el(doc, "div", "asc-grow");
        node.setAttribute("role", "option");
        node.dataset.index = String(i);
        node.id = `asc-grow-${i}`;
        node.style.top = `${i * ROW_HEIGHT}px`;
        spacer.append(node);
        pool.set(i, node);
      }
      fillRow(node, i, store.rowAt(i));
    }
    if (selected >= 0) scroller.setAttribute("aria-activedescendant", `asc-grow-${selected}`);
    if (canvasWarm) {
      drawLanes(vp);
      drawMinimap();
    } else {
      // The first 2D context can take a long time to create on a cold renderer: let the rows
      // reach the screen first, then draw the lanes.
      canvasWarm = true;
      setTimeout(() => {
        if (disposed) return;
        drawLanes(scroller.clientHeight || 400);
        drawMinimap();
      }, 0);
    }
    if (missingPage() !== undefined) void pump();
  }

  function drawLanes(vp: number): void {
    const dpr = doc.defaultView?.devicePixelRatio ?? 1;
    const w = lanesWidth();
    if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(vp * dpr)) {
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(vp * dpr);
      canvas.style.width = `${w}px`;
      canvas.style.height = `${vp}px`;
    }
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, vp);
    const st = scroller.scrollTop;
    const firstRow = Math.max(0, Math.floor(st / ROW_HEIGHT));
    const lastRow = Math.min(store.rowCount, Math.ceil((st + vp) / ROW_HEIGHT));
    ctx.lineWidth = 1.5;
    for (let i = firstRow; i < lastRow; i++) {
      const row = store.rowAt(i);
      if (!row) continue;
      const y0 = i * ROW_HEIGHT - st;
      const cy = y0 + ROW_HEIGHT / 2;
      const nx = laneX(Math.min(row.column, lanes - 1));
      for (const c of row.pass ?? []) {
        ctx.strokeStyle = PALETTE[c % PALETTE.length] as string;
        ctx.beginPath();
        ctx.moveTo(laneX(Math.min(c, lanes - 1)), y0);
        ctx.lineTo(laneX(Math.min(c, lanes - 1)), y0 + ROW_HEIGHT);
        ctx.stroke();
      }
      for (const c of row.incoming ?? []) {
        ctx.strokeStyle = PALETTE[c % PALETTE.length] as string;
        ctx.beginPath();
        ctx.moveTo(laneX(Math.min(c, lanes - 1)), y0);
        ctx.lineTo(nx, cy);
        ctx.stroke();
      }
      for (const e of row.edges) {
        ctx.strokeStyle = PALETTE[e.toColumn % PALETTE.length] as string;
        ctx.beginPath();
        ctx.moveTo(nx, cy);
        ctx.lineTo(laneX(Math.min(e.toColumn, lanes - 1)), y0 + ROW_HEIGHT);
        ctx.stroke();
      }
      const color = PALETTE[row.column % PALETTE.length] as string;
      ctx.beginPath();
      ctx.arc(nx, cy, 4, 0, Math.PI * 2);
      if (row.wip) {
        ctx.strokeStyle = color;
        ctx.stroke();
      } else {
        ctx.fillStyle = color;
        ctx.fill();
      }
      if (row.overflow) {
        ctx.fillStyle = color;
        ctx.fillRect(nx + 6, cy - 1, 6, 2);
      }
    }
  }

  function drawMinimap(): void {
    const h = mini.clientHeight || 0;
    const w = mini.clientWidth || 14;
    if (h === 0) return;
    const dpr = doc.defaultView?.devicePixelRatio ?? 1;
    if (miniCanvas.width !== Math.round(w * dpr) || miniCanvas.height !== Math.round(h * dpr)) {
      miniCanvas.width = Math.round(w * dpr);
      miniCanvas.height = Math.round(h * dpr);
    }
    const ctx = miniCanvas.getContext("2d");
    if (!ctx) return;
    const css = getComputedStyle(root);
    const accent = css.getPropertyValue("--asc-accent").trim() || "#4e9af1";
    const muted = css.getPropertyValue("--asc-fg-muted").trim() || "#888";
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    const m = minimapModel(store);
    ctx.globalAlpha = 0.25;
    ctx.fillStyle = muted;
    for (const l of m.loaded) ctx.fillRect(0, l.from * h, w, Math.max(1, (l.to - l.from) * h));
    ctx.globalAlpha = 1;
    ctx.fillStyle = accent;
    for (const t of m.ticks) ctx.fillRect(0, Math.round(t.at * h), w, 2);
    const vpFrom = scroller.scrollTop / ROW_HEIGHT / m.span;
    const vpTo = (scroller.scrollTop + (scroller.clientHeight || 0)) / ROW_HEIGHT / m.span;
    ctx.strokeStyle = accent;
    ctx.strokeRect(0.5, vpFrom * h + 0.5, w - 1, Math.max(3, (vpTo - vpFrom) * h));
  }

  // ---- selection and navigation ----
  function scrollToRow(i: number, center = false): void {
    const vp = scroller.clientHeight || 400;
    const top = i * ROW_HEIGHT;
    if (center) scroller.scrollTop = Math.max(0, top - vp / 2);
    else if (top < scroller.scrollTop) scroller.scrollTop = top;
    else if (top + ROW_HEIGHT > scroller.scrollTop + vp) scroller.scrollTop = top + ROW_HEIGHT - vp;
  }

  function select(i: number): void {
    const count = store.rowCount;
    if (count === 0) return;
    selected = Math.max(0, Math.min(count - 1, i));
    const row = store.rowAt(selected);
    selectedId = row?.id;
    scrollToRow(selected);
    persist();
    schedule();
    if (row) host.onSelect?.(row);
  }

  function persist(): void {
    host.setState?.({ scrollTop: scroller.scrollTop, ...(selectedId === undefined ? {} : { selected: selectedId }) });
  }

  function nav(key: NavKey): void {
    if (selected < 0) {
      select(key === "last" ? store.rowCount - 1 : 0);
      return;
    }
    select(navigate(store, selected, key, store.rowCount));
  }

  function closeMenu(): void {
    menu?.remove();
    menu = undefined;
  }

  function openMenu(i: number, x: number, y: number): void {
    closeMenu();
    const row = store.rowAt(i);
    if (!row) return;
    const ul = el(doc, "ul", "asc-gmenu");
    ul.setAttribute("role", "menu");
    ul.dataset.testid = "graph-menu";
    const mainRect = main.getBoundingClientRect();
    ul.style.left = `${Math.max(0, x - mainRect.left)}px`;
    ul.style.top = `${Math.max(0, y - mainRect.top)}px`;
    for (const item of menuItems(row)) {
      const li = el(doc, "li");
      const b = el(doc, "button");
      b.type = "button";
      b.textContent = item.label;
      b.dataset.action = item.id;
      b.setAttribute("role", "menuitem");
      if (!item.enabled) {
        b.disabled = true;
        b.title = item.reason ?? "";
      }
      b.addEventListener("click", () => {
        closeMenu();
        void activate(item, row);
      });
      li.append(b);
      ul.append(li);
    }
    main.append(ul);
    menu = ul;
    ul.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
  }

  async function activate(item: MenuItem, row: GraphRow): Promise<void> {
    if (!item.enabled) return;
    if (item.id === "copy-id") {
      await host.copy(row.id);
      return;
    }
    const out = await host.run(menuCall(item.id, row));
    if (!out.ok) host.notify("error", `${item.label} failed`);
  }

  // ---- events ----
  scroller.addEventListener("scroll", () => {
    schedule();
    persist();
  });

  scroller.addEventListener("click", (ev) => {
    const row = (ev.target as Element).closest<HTMLElement>(".asc-grow");
    if (row?.dataset.index) select(Number(row.dataset.index));
    closeMenu();
  });
  scroller.addEventListener("dblclick", (ev) => {
    const node = (ev.target as Element).closest<HTMLElement>(".asc-grow");
    const row = node?.dataset.index ? store.rowAt(Number(node.dataset.index)) : undefined;
    if (row) host.openDetails?.(row);
  });
  scroller.addEventListener("contextmenu", (ev) => {
    const node = (ev.target as Element).closest<HTMLElement>(".asc-grow");
    if (!node?.dataset.index) return;
    ev.preventDefault();
    const i = Number(node.dataset.index);
    select(i);
    openMenu(i, ev.clientX, ev.clientY);
  });

  const keys: Record<string, NavKey> = {
    ArrowDown: "down",
    ArrowUp: "up",
    ArrowLeft: "parent",
    ArrowRight: "child",
    Home: "first",
    End: "last",
    f: "nextFork",
    F: "prevFork",
    r: "nextRef",
    R: "prevRef",
  };
  scroller.addEventListener("keydown", (ev) => {
    if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
    if (ev.key === "/") {
      ev.preventDefault();
      searchIn.focus();
      searchIn.select();
      return;
    }
    if (ev.key === "Escape") {
      closeMenu();
      return;
    }
    if (ev.key === "Enter") {
      const row = store.rowAt(selected);
      if (row) host.openDetails?.(row);
      return;
    }
    if (ev.key === "m" || ev.key === "ContextMenu") {
      ev.preventDefault();
      if (selected >= 0) {
        const rect = scroller.getBoundingClientRect();
        openMenu(selected, rect.left + lanesWidth() + 16, rect.top + selected * ROW_HEIGHT - scroller.scrollTop + ROW_HEIGHT);
      }
      return;
    }
    if (ev.key === "PageDown" || ev.key === "PageUp") {
      ev.preventDefault();
      const step = Math.max(1, viewportRows().rows - 1) * (ev.key === "PageDown" ? 1 : -1);
      select((selected < 0 ? 0 : selected) + step);
      return;
    }
    const nk = keys[ev.key];
    if (nk) {
      ev.preventDefault();
      nav(nk);
    }
  });

  searchIn.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      searchIn.value = "";
      matchIdx = -1;
      scroller.focus();
      schedule();
      return;
    }
    if (ev.key !== "Enter") return;
    ev.preventDefault();
    const hit = searchRow(store, selected < 0 ? -1 : selected, searchIn.value, ev.shiftKey ? -1 : 1);
    if (hit === undefined) {
      setStatus("No match in loaded rows");
      matchIdx = -1;
      return;
    }
    matchIdx = hit;
    select(hit);
    scrollToRow(hit, true);
    setStatus(statusText());
  });

  function filtersChanged(): void {
    preset = presetSel.value as GraphPreset;
    include = parseRefList(includeIn.value);
    exclude = parseRefList(excludeIn.value);
    reload();
  }
  presetSel.addEventListener("change", filtersChanged);
  includeIn.addEventListener("change", filtersChanged);
  excludeIn.addEventListener("change", filtersChanged);

  function reload(): void {
    abort.abort();
    abort = new AbortController();
    gen++;
    pumping = undefined;
    store.reset();
    painted = false;
    for (const n of pool.values()) n.remove();
    pool.clear();
    selected = -1;
    selectedId = undefined;
    matchIdx = -1;
    scroller.scrollTop = 0;
    setStatus("Loading…");
    schedule();
    void pump();
  }

  const onDocClick = (ev: Event): void => {
    if (menu && !menu.contains(ev.target as Node)) closeMenu();
  };
  doc.addEventListener("mousedown", onDocClick);

  mini.addEventListener("click", (ev) => {
    const rect = mini.getBoundingClientRect();
    if (rect.height === 0) return;
    const m = minimapModel(store);
    const row = minimapRow((ev.clientY - rect.top) / rect.height, m.span);
    scrollToRow(row, true);
    schedule();
  });
  mini.addEventListener("mousemove", (ev) => {
    const rect = mini.getBoundingClientRect();
    if (rect.height === 0) return;
    const m = minimapModel(store);
    const y = (ev.clientY - rect.top) / rect.height;
    const near = m.ticks.find((t) => Math.abs(t.at - y) * rect.height <= 3);
    mini.title = near?.name ?? "";
  });

  const resize = new (doc.defaultView?.ResizeObserver ?? class { observe() {} disconnect() {} })(() => schedule());
  resize.observe(scroller);

  // restore saved state once the first page is in
  const saved = host.getState?.();
  void pump().then(() => {
    if (disposed) return;
    if (saved?.scrollTop !== undefined) scroller.scrollTop = saved.scrollTop;
    if (saved?.selected !== undefined) {
      const idx = store.indexOf(saved.selected);
      if (idx !== undefined) {
        selected = idx;
        selectedId = saved.selected;
      }
    }
    schedule();
  });
  setStatus("Loading…");
  schedule();

  return {
    async refreshHead() {
      const myGen = gen;
      const anchorIdx = Math.floor(scroller.scrollTop / ROW_HEIGHT);
      const anchorId = store.rowAt(anchorIdx)?.id;
      const page = await host.fetchPage(query(""), abort.signal);
      if (myGen !== gen || disposed) return;
      store.spliceHead(page);
      if (anchorId !== undefined) {
        const now = store.indexOf(anchorId);
        if (now !== undefined && now !== anchorIdx) scroller.scrollTop += (now - anchorIdx) * ROW_HEIGHT;
      }
      afterStore();
      setStatus(statusText());
      await pump();
    },
    setFilters(f) {
      if (f.preset !== undefined) {
        preset = f.preset;
        presetSel.value = f.preset;
      }
      if (f.include !== undefined) {
        include = f.include;
        includeIn.value = f.include.join(" ");
      }
      if (f.exclude !== undefined) {
        exclude = f.exclude;
        excludeIn.value = f.exclude.join(" ");
      }
      reload();
    },
    select,
    get selectedIndex() {
      return selected;
    },
    get heldRows() {
      return store.heldRows;
    },
    get rowCount() {
      return store.rowCount;
    },
    async idle() {
      for (let p = pumping; p; p = pumping) await p;
    },
    dispose() {
      disposed = true;
      abort.abort();
      resize.disconnect();
      doc.removeEventListener("mousedown", onDocClick);
      root.remove();
    },
  };
}
