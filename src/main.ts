import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { drawBuckets, bandColor, refreshPalette, type Bucket, type TagProfile } from "./render";

interface Target {
  id: string;
  label: string;
  host: string;
  tag: string;
  intervalMs: number;
}

interface AppConfig {
  targets: Target[];
  tags: TagProfile[];
  theme?: string;
  aggregate?: string;
}

interface SampleEvent {
  targetId: string;
  resolvedIp: string | null;
  sample: { ts: number; rttMs: number | null };
}

interface WindowStats {
  avg: number | null;
  jitter: number | null;
  lossPct: number;
  count: number;
}

interface Row {
  target: Target;
  rowEl: HTMLElement;
  canvas: HTMLCanvasElement;
  chip: HTMLElement;
  labelEl: HTMLElement;
  detailEl: HTMLElement;
  avgEl: HTMLElement;
  jitEl: HTMLElement;
  lossEl: HTMLElement;
  hoverVal: HTMLElement;
  buckets: Bucket[];
  resolvedIp: string | null;
}

const RANGES: { key: string; sec: number }[] = [
  { key: "15m", sec: 900 },
  { key: "1h", sec: 3600 },
  { key: "6h", sec: 21600 },
  { key: "12h", sec: 43200 },
  { key: "1d", sec: 86400 },
  { key: "1w", sec: 604800 },
];

const state = {
  rangeSec: 3600,
  live: true,
  anchorFrac: 1,
  oldestMs: Date.now(),
  paused: false,
  frozenTo: Date.now(),
  oneToOne: false, // window span = strip width in px (one ping per column)
};

function stripWidthPx(): number {
  return firstStripRect()?.width ?? 600;
}
// Effective range: in 1:1 mode it's the strip width in pixels (≈ seconds at 1 ping/s).
function curRangeSec(): number {
  return state.oneToOne ? Math.max(60, Math.floor(stripWidthPx())) : state.rangeSec;
}
function rangeMs(): number {
  return curRangeSec() * 1000;
}

const rows = new Map<string, Row>();
let profiles = new Map<string, TagProfile>();
let lastLiveRefresh = 0;
let editingId: string | null = null;

// Custom stats window: offsets (ms) back from the view's right edge. Because it's relative
// to `to`, it slides with "now" while live and freezes when paused (pause freezes `to`).
let selection: { start: number; end: number } | null = null; // start > end
let selDragging = false;
let selOverlay: HTMLElement | null = null;

const rowsEl = document.querySelector<HTMLElement>("#rows")!;
const modal = document.querySelector<HTMLElement>("#editor")!;
const settingsModal = document.querySelector<HTMLElement>("#settings")!;

// Settings categories map to tag names; gateway mirrors LAN.
const CAT_TAG: Record<string, string> = { lan: "wifi", wan: "isp", inet: "internet" };
const THRESH_DEFAULTS: Record<string, [number, number, number, number]> = {
  lan: [10, 20, 40, 100],
  wan: [25, 40, 80, 150],
  inet: [30, 50, 100, 300],
};

const q = <T extends HTMLElement>(sel: string) => document.querySelector<T>(sel)!;

// ---- helpers -------------------------------------------------------------

function fallbackProfile(): TagProfile {
  return { tag: "?", good: 6, ok: 10, poor: 20, terrible: 40 };
}
function profileFor(tag: string): TagProfile {
  return profiles.get(tag) ?? fallbackProfile();
}

const TAG_PALETTE = ["#4c8dff", "#a371f7", "#2bb0c4", "#c9a227", "#d2679a", "#5fa85f"];
function tagColor(tag: string): string {
  let h = 0;
  for (const c of tag) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return TAG_PALETTE[h % TAG_PALETTE.length];
}

function slug(s: string): string {
  return s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "") || "t";
}

function fmtMs(v: number | null): string {
  if (v == null) return "—";
  return v < 10 ? v.toFixed(1) : String(Math.round(v));
}

function fmtLoss(p: number): string {
  if (p <= 0) return "0%";
  if (p < 1) return p.toFixed(1) + "%";
  return `${Math.round(p)}%`;
}

// Loss color scale: <0.5% normal (theme text), 0.5-1% yellow, 1-3% orange, >3% red.
function lossColor(p: number): string {
  if (p < 0.5) return ""; // inherit theme text color
  if (p <= 1) return "#d4c531";
  if (p <= 3) return "#db8b2a";
  return "#e5484d";
}

// How each pixel-bucket reduces its samples: worst | trimmed | mean | median | best.
let aggregate = "worst";

// ---- theme ----
let themeSetting = "system";
const themeMql = window.matchMedia("(prefers-color-scheme: dark)");

function effectiveDark(): boolean {
  return themeSetting === "dark" || (themeSetting === "system" && themeMql.matches);
}
function applyTheme(): void {
  const root = document.documentElement;
  const dark = effectiveDark();
  root.classList.toggle("theme-dark", dark);
  root.classList.toggle("theme-light", !dark);
  refreshPalette();
  refreshAll();
}
themeMql.addEventListener("change", () => {
  if (themeSetting === "system") applyTheme();
});

// ---- scrub time formatting + snapping ----
// Snap the window edge to natural local-time boundaries per range.
const SNAP_MS: Record<number, number> = {
  900: 5 * 60_000,
  3600: 15 * 60_000,
  21600: 60 * 60_000,
  43200: 2 * 60 * 60_000,
  86400: 4 * 60 * 60_000,
  604800: 6 * 60 * 60_000,
};
function snapEnd(end: number): number {
  const step = SNAP_MS[curRangeSec()]; // 1:1 (dynamic span) isn't in the table -> no snap
  if (!step) return end;
  const offMs = new Date().getTimezoneOffset() * 60_000; // align in local wall-clock time
  const local = end - offMs;
  return Math.round(local / step) * step + offMs;
}
// Range label that drops components shared by start and end:
//   same day & hour  -> "Jun 18 at 10:42-57 am"
//   same day         -> "Jun 18 at 10:42am – 11:42am"
//   same month       -> "Jun 18-19 at 11:45pm – 12:45am"
//   otherwise        -> "Jun 30 at 11:45pm – Jul 1 at 12:45am"
// Compact duration, e.g. "45s", "11m 13s", "1h", "6h", "1d", "7d".
function fmtDur(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return s % 60 ? `${m}m ${s % 60}s` : `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return m % 60 ? `${h}h ${m % 60}m` : `${h}h`;
  const d = Math.floor(h / 24);
  return h % 24 ? `${d}d ${h % 24}h` : `${d}d`;
}

function fmtRange(fromMs: number, toMs: number): string {
  const a = new Date(fromMs);
  const b = new Date(toMs);
  const two = (n: number) => String(n).padStart(2, "0");
  const ap = (h: number) => (h < 12 ? "am" : "pm");
  const h12 = (h: number) => h % 12 || 12;
  const date = (d: Date) => d.toLocaleDateString([], { month: "short", day: "numeric" });
  // Show seconds for short spans (< 5 min) so a tight selection isn't rounded to the minute.
  const secs = toMs - fromMs < 5 * 60 * 1000;
  const time = (d: Date) =>
    secs
      ? `${h12(d.getHours())}:${two(d.getMinutes())}:${two(d.getSeconds())}${ap(d.getHours())}`
      : `${h12(d.getHours())}:${two(d.getMinutes())}${ap(d.getHours())}`;

  const sameDay = a.toDateString() === b.toDateString();
  const sameMonth = a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth();

  // The minute-only collapse ("10:42-57 am") can't carry seconds cleanly; skip it when we
  // need second resolution and fall through to the explicit start – end form below.
  if (!secs && sameDay && a.getHours() === b.getHours()) {
    return `${date(a)} at ${h12(a.getHours())}:${two(a.getMinutes())}-${two(b.getMinutes())} ${ap(a.getHours())}`;
  }
  if (sameDay) {
    return `${date(a)} at ${time(a)} – ${time(b)}`;
  }
  if (sameMonth) {
    const month = a.toLocaleDateString([], { month: "short" });
    return `${month} ${a.getDate()}-${b.getDate()} at ${time(a)} – ${time(b)}`;
  }
  return `${date(a)} at ${time(a)} – ${date(b)} at ${time(b)}`;
}

// Secondary identifier, with nothing repeated: show host only if it differs from the
// label, and the resolved IP only if it differs from the host (so an IP-only target
// shows its address exactly once — as the label).
function detailText(target: Target, resolvedIp: string | null): string {
  const parts: string[] = [];
  if (target.label !== target.host) parts.push(target.host);
  if (resolvedIp && resolvedIp !== target.host) parts.push(resolvedIp);
  return parts.join("  ");
}

function currentWindow(): { from: number; to: number } {
  const now = Date.now();
  const rMs = rangeMs();
  const minEnd = state.oldestMs + rMs;
  let end: number;
  if (state.paused) {
    end = state.frozenTo;
  } else if (state.live) {
    end = now;
  } else {
    end = minEnd + state.anchorFrac * Math.max(0, now - minEnd);
    end = Math.min(now, Math.max(minEnd, end));
  }
  return { from: Math.round(end - rMs), to: Math.round(end) };
}

function liveIntervalMs(): number {
  const r = curRangeSec();
  if (r <= 43200) return 1000;
  if (r <= 86400) return 3000;
  return 15000;
}

// ---- rendering -----------------------------------------------------------

// Avg / jitter / loss computed client-side from 1:1 sample buckets — each bar is one poll, so
// `bucket.worst` IS the raw RTT. Optional [lo, hi) restricts to a drag-selected sub-range.
function statsFrom1to1(
  buckets: Bucket[],
  lo?: number,
  hi?: number,
): { avg: number | null; jitter: number | null; lossPct: number } {
  let sum = 0;
  let sumSq = 0;
  let recv = 0;
  let total = 0;
  for (const b of buckets) {
    if (lo != null && hi != null && (b.t < lo || b.t >= hi)) continue;
    if (b.count === 0) continue;
    total += b.count;
    if (b.worst != null) {
      sum += b.worst;
      sumSq += b.worst * b.worst;
      recv += 1;
    }
  }
  if (recv === 0) return { avg: null, jitter: null, lossPct: total > 0 ? 100 : 0 };
  const avg = sum / recv;
  const jitter = Math.sqrt(Math.max(0, sumSq / recv - avg * avg));
  const lossPct = total > 0 ? ((total - recv) / total) * 100 : 0;
  return { avg, jitter, lossPct };
}

async function refreshRow(row: Row): Promise<void> {
  const { from, to } = currentWindow();
  const sel = selectionWindow(); // stats reflect the custom selection when active
  const n = Math.max(1, Math.floor(row.canvas.clientWidth));
  const id = row.target.id;
  const profile = profileFor(row.target.tag);
  try {
    let buckets: Bucket[];
    let stats: { avg: number | null; jitter: number | null; lossPct: number };
    if (state.oneToOne) {
      // True 1:1 — one raw poll per pixel (no time-bucketing), ending at the shown instant.
      buckets = await invoke<Bucket[]>("get_samples", { targetId: id, before: to, limit: n });
      stats = statsFrom1to1(buckets, sel?.from, sel?.to);
    } else {
      const sFrom = sel ? sel.from : from;
      const sTo = sel ? sel.to : to;
      // Never request more buckets than there are seconds in the window: with 1s polling,
      // sub-second buckets would leave periodic empty columns (a moiré of fake gaps). Capping
      // at one-per-second keeps every column backed by a sample; drawBuckets stretches them.
      const windowSecs = Math.max(1, Math.ceil((to - from) / 1000));
      const nb = Math.min(n, windowSecs);
      const [b, st] = await Promise.all([
        invoke<Bucket[]>("get_window", { targetId: id, from, to, buckets: nb, agg: aggregate }),
        invoke<WindowStats>("get_stats", { targetId: id, from: sFrom, to: sTo }),
      ]);
      buckets = b;
      stats = { avg: st.avg, jitter: st.jitter, lossPct: st.lossPct };
    }
    row.buckets = buckets;
    drawBuckets(row.canvas, buckets, profile);

    row.avgEl.textContent = stats.avg == null ? "—" : `${fmtMs(stats.avg)}ms`;
    row.avgEl.style.color = stats.avg == null ? "" : bandColor(stats.avg, profile);
    row.jitEl.textContent = stats.jitter == null ? "—" : `±${fmtMs(stats.jitter)}`;
    row.lossEl.textContent = fmtLoss(stats.lossPct);
    row.lossEl.style.color = lossColor(stats.lossPct);
  } catch {
    /* ignore */
  }
}

async function refreshAll(): Promise<void> {
  updateScrollbar();
  updateSelectionOverlay();
  await Promise.all([...rows.values()].map(refreshRow));
  if (hoverX != null) updateCrosshairAt(hoverX); // keep readout on the data under the cursor
}

let resizeTimer = 0;
function scheduleRefresh(): void {
  clearTimeout(resizeTimer);
  resizeTimer = window.setTimeout(refreshAll, 60);
}

// ---- row construction (single line) -------------------------------------

function buildRow(target: Target): Row {
  const rowEl = document.createElement("div");
  rowEl.className = "row";
  rowEl.dataset.id = target.id;

  const handle = document.createElement("span");
  handle.className = "handle";
  handle.textContent = "⠿";
  handle.title = "Drag to reorder";

  const chip = document.createElement("span");
  chip.className = "tag";
  chip.textContent = target.tag;
  chip.style.backgroundColor = tagColor(target.tag);

  const labelEl = document.createElement("span");
  labelEl.className = "label";
  labelEl.textContent = target.label;

  const detailEl = document.createElement("span");
  detailEl.className = "detail";
  detailEl.textContent = detailText(target, null);

  const meta = document.createElement("span");
  meta.className = "meta";
  meta.title = "Click to edit";
  meta.append(chip, labelEl, detailEl);
  meta.addEventListener("click", () => openEditor(target.id));

  // ---- window stats: three columns (avg ms / jitter ±σ / loss %); labels via hover ----
  const wstats = document.createElement("div");
  wstats.className = "wstats";
  const avgEl = document.createElement("span");
  avgEl.className = "val s-avg";
  avgEl.title = "avg response time";
  avgEl.textContent = "—";
  const jitEl = document.createElement("span");
  jitEl.className = "val s-jit";
  jitEl.title = "jitter (1σ from mean)";
  jitEl.textContent = "—";
  const lossEl = document.createElement("span");
  lossEl.className = "val s-loss";
  lossEl.title = "packet loss";
  lossEl.textContent = "0%";
  wstats.append(avgEl, jitEl, lossEl);

  const canvas = document.createElement("canvas");
  canvas.className = "strip";

  const editBtn = document.createElement("button");
  editBtn.className = "icon";
  editBtn.textContent = "✎";
  editBtn.title = "Edit";
  editBtn.addEventListener("click", () => openEditor(target.id));

  const hoverVal = document.createElement("span");
  hoverVal.className = "hover-val";
  hoverVal.hidden = true;

  rowEl.append(handle, meta, wstats, canvas, editBtn, hoverVal);
  rowsEl.append(rowEl);

  wireDrag(handle, rowEl);

  return {
    target,
    rowEl,
    canvas,
    chip,
    labelEl,
    detailEl,
    avgEl,
    jitEl,
    lossEl,
    hoverVal,
    buckets: [],
    resolvedIp: null,
  };
}

async function addRow(target: Target): Promise<void> {
  const row = buildRow(target);
  rows.set(target.id, row);
  await refreshRow(row);
}

function removeRowDom(id: string): void {
  rowsEl.querySelector(`.row[data-id="${CSS.escape(id)}"]`)?.remove();
  rows.delete(id);
}

async function removeTarget(id: string): Promise<void> {
  try {
    await invoke("remove_target", { targetId: id });
    removeRowDom(id);
  } catch (e) {
    console.error("remove_target failed", e);
  }
}

// ---- edit pop-up ---------------------------------------------------------

function openEditor(id: string): void {
  const row = rows.get(id);
  if (!row) return;
  editingId = id;
  q<HTMLInputElement>("#m-label").value = row.target.label;
  q<HTMLInputElement>("#m-host").value = row.target.host;
  q<HTMLInputElement>("#m-tag").value = row.target.tag;
  q<HTMLInputElement>("#m-interval").value = String(row.target.intervalMs);
  modal.hidden = false;
  q<HTMLInputElement>("#m-label").focus();
}

function closeEditor(): void {
  modal.hidden = true;
  editingId = null;
}

async function saveEditor(): Promise<void> {
  if (!editingId) return;
  const row = rows.get(editingId);
  if (!row) return;
  const label = q<HTMLInputElement>("#m-label").value.trim();
  const host = q<HTMLInputElement>("#m-host").value.trim();
  const tag = q<HTMLInputElement>("#m-tag").value.trim() || "internet";
  // 1s floor: one poll per wall-clock second per target (matches the daemon's enforcement).
  const intervalMs = Math.max(
    1000,
    parseInt(q<HTMLInputElement>("#m-interval").value, 10) || 1000,
  );
  if (!host) return;

  const updated: Target = { id: editingId, label: label || host, host, tag, intervalMs };
  try {
    await invoke<AppConfig>("update_target", { target: updated });
  } catch (e) {
    console.error("update_target failed", e);
    return;
  }
  row.target = updated;
  row.labelEl.textContent = updated.label;
  row.chip.textContent = updated.tag;
  row.chip.style.backgroundColor = tagColor(updated.tag);
  row.detailEl.textContent = detailText(updated, row.resolvedIp);
  closeEditor();
  refreshRow(row);
}

async function removeFromEditor(): Promise<void> {
  if (!editingId) return;
  const id = editingId;
  closeEditor();
  await removeTarget(id);
}

function wireEditor(): void {
  q<HTMLButtonElement>("#m-save").addEventListener("click", saveEditor);
  q<HTMLButtonElement>("#m-cancel").addEventListener("click", closeEditor);
  q<HTMLButtonElement>("#m-remove").addEventListener("click", removeFromEditor);
  modal.addEventListener("click", (e) => {
    if (e.target === modal) closeEditor(); // click backdrop to dismiss
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !modal.hidden) closeEditor();
  });
}

// ---- settings (thresholds) pop-up ---------------------------------------

function setCat(cat: string, vals: [number, number, number, number]): void {
  q<HTMLInputElement>(`#s-${cat}-good`).value = String(vals[0]);
  q<HTMLInputElement>(`#s-${cat}-ok`).value = String(vals[1]);
  q<HTMLInputElement>(`#s-${cat}-bad`).value = String(vals[2]);
  q<HTMLInputElement>(`#s-${cat}-max`).value = String(vals[3]);
}

function getCat(cat: string): [number, number, number, number] {
  const num = (id: string) => Math.max(1, parseFloat(q<HTMLInputElement>(id).value) || 0);
  return [
    num(`#s-${cat}-good`),
    num(`#s-${cat}-ok`),
    num(`#s-${cat}-bad`),
    num(`#s-${cat}-max`),
  ];
}

function openSettings(): void {
  for (const cat of Object.keys(CAT_TAG)) {
    const p = profiles.get(CAT_TAG[cat]);
    setCat(cat, p ? [p.good, p.ok, p.poor, p.terrible] : THRESH_DEFAULTS[cat]);
  }
  document.querySelectorAll<HTMLInputElement>('input[name="theme"]').forEach((r) => {
    r.checked = r.value === themeSetting;
  });
  settingsModal.hidden = false;
}

function closeSettings(): void {
  settingsModal.hidden = true;
}

async function saveSettings(): Promise<void> {
  const mk = (tag: string, v: [number, number, number, number]): TagProfile => ({
    tag,
    good: v[0],
    ok: v[1],
    poor: v[2],
    terrible: v[3],
  });
  const lan = getCat("lan");
  const byTag = new Map(profiles);
  byTag.set("wifi", mk("wifi", lan));
  byTag.set("gateway", mk("gateway", lan)); // gateway follows LAN
  byTag.set("isp", mk("isp", getCat("wan")));
  byTag.set("internet", mk("internet", getCat("inet")));
  const tags = [...byTag.values()];
  try {
    await invoke("set_tags", { tags });
  } catch (e) {
    console.error("set_tags failed", e);
    return;
  }
  profiles = new Map(tags.map((t) => [t.tag, t]));
  closeSettings();
  refreshAll();
}

function wireSettings(): void {
  q<HTMLButtonElement>("#settings-btn").addEventListener("click", openSettings);
  q<HTMLButtonElement>("#s-save").addEventListener("click", saveSettings);
  q<HTMLButtonElement>("#s-cancel").addEventListener("click", closeSettings);
  q<HTMLButtonElement>("#s-reset").addEventListener("click", () => {
    for (const cat of Object.keys(THRESH_DEFAULTS)) setCat(cat, THRESH_DEFAULTS[cat]);
  });
  document.querySelectorAll<HTMLInputElement>('input[name="theme"]').forEach((r) =>
    r.addEventListener("change", async (e) => {
      themeSetting = (e.target as HTMLInputElement).value;
      try {
        await invoke("set_theme", { theme: themeSetting });
      } catch {
        /* ignore */
      }
      applyTheme();
    }),
  );
  settingsModal.addEventListener("click", (e) => {
    if (e.target === settingsModal) closeSettings();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !settingsModal.hidden) closeSettings();
  });
}

// ---- drag reorder --------------------------------------------------------

// Pointer-based reorder. (HTML5 drag-and-drop is unreliable inside the macOS WKWebView,
// where Tauri's OS-level drag-drop handler intercepts the events.)
function wireDrag(handle: HTMLElement, rowEl: HTMLElement): void {
  handle.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    rowEl.classList.add("dragging");

    const onMove = (ev: PointerEvent) => {
      const after = rowAfter(ev.clientY);
      if (after == null) rowsEl.appendChild(rowEl);
      else if (after !== rowEl) rowsEl.insertBefore(rowEl, after);
    };
    const onUp = () => {
      document.removeEventListener("pointermove", onMove);
      document.removeEventListener("pointerup", onUp);
      rowEl.classList.remove("dragging");
      persistOrder();
    };
    document.addEventListener("pointermove", onMove);
    document.addEventListener("pointerup", onUp);
  });
}

function rowAfter(y: number): HTMLElement | null {
  const els = [...rowsEl.querySelectorAll<HTMLElement>(".row:not(.dragging)")];
  for (const el of els) {
    const box = el.getBoundingClientRect();
    if (y < box.top + box.height / 2) return el;
  }
  return null;
}

async function persistOrder(): Promise<void> {
  const order = [...rowsEl.querySelectorAll<HTMLElement>(".row")].map((el) => el.dataset.id!);
  try {
    await invoke("reorder_targets", { order });
  } catch (e) {
    console.error("reorder_targets failed", e);
  }
}

// ---- controls ------------------------------------------------------------

function updateRangeActive(): void {
  q<HTMLElement>("#ranges")
    .querySelectorAll<HTMLElement>(".range")
    .forEach((b) => {
      const active = state.oneToOne
        ? b.dataset.one === "1"
        : b.dataset.sec !== undefined && Number(b.dataset.sec) === state.rangeSec;
      b.classList.toggle("active", active);
    });
}

function buildControls(): void {
  const rangeWrap = q<HTMLElement>("#ranges");
  // 1:1 (leftmost) — window span = strip width in px, so each column is one ping.
  const oneBtn = document.createElement("button");
  oneBtn.className = "range";
  oneBtn.textContent = "1:1";
  oneBtn.dataset.one = "1";
  oneBtn.title = "One ping per pixel (window = strip width)";
  oneBtn.addEventListener("click", () => {
    state.oneToOne = true;
    selection = null;
    updateSelectionOverlay();
    updateRangeActive();
    refreshAll();
  });
  rangeWrap.append(oneBtn);

  for (const r of RANGES) {
    const btn = document.createElement("button");
    btn.className = "range";
    btn.textContent = r.key;
    btn.dataset.sec = String(r.sec);
    btn.addEventListener("click", () => {
      state.rangeSec = r.sec;
      state.oneToOne = false;
      selection = null; // a selection's offsets don't carry meaning across ranges
      updateSelectionOverlay();
      updateRangeActive();
      refreshAll();
    });
    rangeWrap.append(btn);
  }
  updateRangeActive();

  const track = q<HTMLElement>("#scrub");
  const liveBtn = q<HTMLButtonElement>("#live-btn");

  track.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    setPaused(false); // manual navigation resumes from any freeze
    const rect = track.getBoundingClientRect();
    const geo = thumbGeometry();
    const thumb = q<HTMLElement>("#scrub-thumb");
    const pressX = e.clientX - rect.left;
    const thumbLeft = (state.live ? 1 : state.anchorFrac) * geo.maxLeft;
    const onThumb = e.target === thumb && pressX >= thumbLeft && pressX <= thumbLeft + geo.thumbW;
    const grab = onThumb ? pressX - thumbLeft : geo.thumbW / 2;

    let lastRefresh = 0;
    let preciseMode = false;
    let fineTimer = 0;
    let lastX = e.clientX;
    let lastClientX = e.clientX;

    const apply = (clientX: number) => {
      lastClientX = clientX;
      const now = Date.now();
      const minEnd = state.oldestMs + rangeMs();
      const span = Math.max(1, now - minEnd);
      const nl = Math.min(geo.maxLeft, Math.max(0, clientX - rect.left - grab));
      const rawFrac = geo.maxLeft > 0 ? nl / geo.maxLeft : 1;
      const rawEnd = minEnd + rawFrac * span;
      const end = preciseMode ? rawEnd : snapEnd(rawEnd); // snap unless precise
      state.anchorFrac = Math.min(1, Math.max(0, (end - minEnd) / span));
      state.live = state.anchorFrac >= 0.999;
      liveBtn.classList.toggle("active", state.live);
      updateScrollbar();
      // Throttle (not debounce) so the graphs scroll live while dragging.
      if (now - lastRefresh >= 60) {
        lastRefresh = now;
        refreshAll();
      }
    };

    apply(e.clientX);

    const onMove = (ev: PointerEvent) => {
      const delta = Math.abs(ev.clientX - lastX);
      lastX = ev.clientX;
      // Sustained fine movement (<=2px) for 250ms unlocks precise (un-snapped) placement.
      if (!preciseMode) {
        if (delta <= 2) {
          if (!fineTimer) {
            fineTimer = window.setTimeout(() => {
              preciseMode = true;
              fineTimer = 0;
              apply(lastClientX);
            }, 250);
          }
        } else if (fineTimer) {
          clearTimeout(fineTimer);
          fineTimer = 0;
        }
      }
      apply(ev.clientX);
    };
    const onUp = () => {
      if (fineTimer) clearTimeout(fineTimer);
      document.removeEventListener("pointermove", onMove);
      document.removeEventListener("pointerup", onUp);
      refreshAll(); // settle on the final position
    };
    document.addEventListener("pointermove", onMove);
    document.addEventListener("pointerup", onUp);
  });

  liveBtn.addEventListener("click", () => {
    state.live = true;
    state.anchorFrac = 1;
    setPaused(false);
    liveBtn.classList.add("active");
    updateScrollbar();
    refreshAll();
  });
  liveBtn.classList.toggle("active", state.live);

  q<HTMLButtonElement>("#pause-btn").addEventListener("click", togglePause);

  const exportBtn = q<HTMLButtonElement>("#export-csv");
  exportBtn.addEventListener("click", async () => {
    const sw = selectionWindow();
    if (!sw) return;
    exportBtn.disabled = true;
    const label = exportBtn.textContent;
    try {
      await invoke<string>("export_csv", { from: sw.from, to: sw.to });
      exportBtn.textContent = "✓ saved"; // file also reveals in Finder
    } catch (e) {
      console.error("export failed", e);
      exportBtn.textContent = "✗ failed";
    } finally {
      window.setTimeout(() => {
        exportBtn.textContent = label;
        exportBtn.disabled = false;
      }, 2000);
    }
  });

  const aggSel = q<HTMLSelectElement>("#agg-select");
  aggSel.value = aggregate;
  aggSel.addEventListener("change", async (e) => {
    aggregate = (e.target as HTMLSelectElement).value;
    try {
      await invoke("set_aggregate", { aggregate });
    } catch {
      /* ignore */
    }
    refreshAll(); // re-render every row with the new reduction
  });
}

// Scrollbar thumb sizing: width = window / total-data-span, position from anchor.
function thumbGeometry(): { thumbW: number; maxLeft: number } {
  const track = q<HTMLElement>("#scrub");
  const trackW = track.clientWidth;
  const span = Math.max(1, Date.now() - state.oldestMs);
  const windowMs = rangeMs();
  const frac = Math.min(1, windowMs / span);
  const thumbW = Math.min(trackW, Math.max(16, frac * trackW));
  const maxLeft = Math.max(0, trackW - thumbW);
  return { thumbW, maxLeft };
}

function updateScrollbar(): void {
  const thumb = q<HTMLElement>("#scrub-thumb");
  const { thumbW, maxLeft } = thumbGeometry();
  const now = Date.now();
  const minEnd = state.oldestMs + rangeMs();
  const denom = now - minEnd;
  const frac = denom <= 0 ? 1 : Math.min(1, Math.max(0, (currentWindow().to - minEnd) / denom));
  thumb.style.width = `${thumbW}px`;
  thumb.style.transform = `translateX(${frac * maxLeft}px)`;

  // Top-bar readout: selection (with duration) if active; otherwise the window's real
  // duration, plus the absolute range when scrubbed/paused.
  const wr = q<HTMLElement>("#window-range");
  const sw = selectionWindow();
  q<HTMLButtonElement>("#export-csv").hidden = !sw; // only exportable with a selection
  if (sw) {
    wr.textContent = `sel ${fmtDur(sw.to - sw.from)} · ${fmtRange(sw.from, sw.to)}`;
  } else if (state.live && !state.paused) {
    wr.textContent = fmtDur(rangeMs()); // real span (esp. useful in 1:1)
  } else {
    const w = currentWindow();
    wr.textContent = `${fmtDur(w.to - w.from)} · ${fmtRange(w.from, w.to)}`;
  }
}

function setPaused(on: boolean): void {
  state.paused = on;
  const btn = q<HTMLButtonElement>("#pause-btn");
  btn.textContent = on ? "▶" : "⏸";
  btn.classList.toggle("active", on);
  btn.title = on ? "Resume" : "Pause (freeze view)";
}

function togglePause(): void {
  if (!state.paused) {
    state.frozenTo = currentWindow().to; // freeze whatever is currently shown
    setPaused(true);
  } else {
    setPaused(false);
  }
  updateScrollbar();
  refreshAll();
}

function wireAddForm(): void {
  const form = q<HTMLFormElement>("#add-form");
  const labelI = q<HTMLInputElement>("#f-label");
  const hostI = q<HTMLInputElement>("#f-host");
  const tagI = q<HTMLInputElement>("#f-tag");

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const label = labelI.value.trim();
    const host = hostI.value.trim();
    const tag = tagI.value.trim() || "internet";
    if (!host) return;
    const target: Target = {
      id: `${slug(label || host)}-${Math.floor(Math.random() * 1e6)}`,
      label: label || host,
      host,
      tag,
      intervalMs: 1000,
    };
    try {
      await invoke<AppConfig>("add_target", { target });
      await addRow(target);
      labelI.value = "";
      hostI.value = "";
    } catch (err) {
      console.error("add_target failed", err);
    }
  });
}

function populateTagOptions(): void {
  const list = q<HTMLDataListElement>("#tag-options");
  list.innerHTML = "";
  for (const t of profiles.keys()) {
    const opt = document.createElement("option");
    opt.value = t;
    list.append(opt);
  }
}

// ---- hover crosshair -----------------------------------------------------

let crosshairLine: HTMLElement | null = null;
let hoverX: number | null = null; // last cursor X (viewport), re-read on every refresh

function wireCrosshair(): void {
  crosshairLine = document.createElement("div");
  crosshairLine.className = "crosshair-line";
  crosshairLine.hidden = true;
  document.body.append(crosshairLine);

  rowsEl.addEventListener("pointermove", (e) => {
    if (selDragging) return; // selection drag owns the pointer
    hoverX = e.clientX;
    updateCrosshairAt(hoverX);
  });
  rowsEl.addEventListener("pointerleave", () => {
    hoverX = null;
    hideCrosshair();
  });
}

// Map a viewport X to a bucket index and refresh the line + per-row values. Called both
// on pointer move and after each data refresh, so the readout tracks live data under it.
function updateCrosshairAt(clientX: number): void {
  const first = rows.values().next().value as Row | undefined;
  if (!first) {
    hideCrosshair();
    return;
  }
  // All strips are left-aligned and share the time window, so one strip's geometry maps
  // the cursor to the same bucket index in every row.
  const rect = first.canvas.getBoundingClientRect();
  if (clientX < rect.left || clientX > rect.right || rect.width === 0) {
    hideCrosshair();
    return;
  }
  const n = first.buckets.length || Math.floor(rect.width);
  if (n === 0) return;
  const i = Math.max(0, Math.min(n - 1, Math.floor(((clientX - rect.left) / rect.width) * n)));
  showCrosshair(clientX, rect.right, i);
}

function showCrosshair(clientX: number, stripRight: number, i: number): void {
  if (!crosshairLine) return;
  const rrect = rowsEl.getBoundingClientRect();
  crosshairLine.style.left = `${clientX}px`;
  crosshairLine.style.top = `${rrect.top}px`;
  crosshairLine.style.height = `${rrect.height}px`;
  crosshairLine.hidden = false;

  const flip = clientX > stripRight - 44; // near right edge -> draw label to the left
  for (const row of rows.values()) {
    const b = row.buckets[i];
    row.hoverVal.textContent = b && b.worst != null ? fmtMs(b.worst) : "-";
    row.hoverVal.style.left = `${clientX - row.rowEl.getBoundingClientRect().left}px`;
    row.hoverVal.classList.toggle("flip", flip);
    row.hoverVal.hidden = false;
  }
}

function hideCrosshair(): void {
  if (crosshairLine) crosshairLine.hidden = true;
  for (const row of rows.values()) row.hoverVal.hidden = true;
}

// ---- network / pairing ---------------------------------------------------

interface NodeInfo {
  id: string;
  name: string;
  mode: string;
}
interface DiscoPeer {
  id: string;
  name: string;
  addr: string;
  lastSeen: number;
}
interface PeerInfo {
  id: string;
  name: string;
  role: string;
  addr?: string | null;
}

let node: NodeInfo = { id: "", name: "", mode: "client" };
const netModal = document.querySelector<HTMLElement>("#network")!;
const inviteModal = document.querySelector<HTMLElement>("#invite")!;
let inviteServerId: string | null = null;
let inviteTimer = 0;

function applyMode(): void {
  const badge = q<HTMLElement>("#mode-badge");
  badge.textContent = node.mode;
  badge.classList.toggle("server", node.mode === "server");
  q<HTMLElement>("#server-pane").hidden = node.mode !== "server";
  document.querySelectorAll<HTMLInputElement>('input[name="mode"]').forEach((r) => {
    r.checked = r.value === node.mode;
  });
}

function renderDiscovered(list: DiscoPeer[]): void {
  const el = q<HTMLElement>("#n-discovered");
  el.innerHTML = "";
  if (!list.length) {
    el.innerHTML = '<div class="net-empty">none yet — press Scan</div>';
    return;
  }
  for (const p of list) {
    const row = document.createElement("div");
    row.className = "net-item";
    const name = document.createElement("span");
    name.textContent = p.name;
    const addr = document.createElement("span");
    addr.className = "net-addr";
    addr.textContent = p.addr;
    const btn = document.createElement("button");
    btn.textContent = "invite";
    btn.addEventListener("click", async () => {
      const msg = prompt(
        `Message to ${p.name}:`,
        `${node.name} would like to run probes on your machine.`,
      );
      if (msg == null) return;
      await invoke("net_invite", { peerId: p.id, message: msg });
    });
    row.append(name, addr, btn);
    el.append(row);
  }
}

function renderPeers(list: PeerInfo[]): void {
  const el = q<HTMLElement>("#n-peers");
  el.innerHTML = "";
  if (!list.length) {
    el.innerHTML = '<div class="net-empty">no paired peers</div>';
    return;
  }
  for (const p of list) {
    const row = document.createElement("div");
    row.className = "net-item";
    const name = document.createElement("span");
    name.textContent = p.name;
    const role = document.createElement("span");
    role.className = "net-addr";
    role.textContent = p.role; // "client" = we manage it; "server" = it manages us
    row.append(name, role);
    el.append(row);
  }
}

async function openNetwork(): Promise<void> {
  q<HTMLInputElement>("#n-name").value = node.name;
  applyMode();
  renderPeers(await invoke<PeerInfo[]>("get_peers"));
  renderDiscovered(await invoke<DiscoPeer[]>("net_discovered"));
  netModal.hidden = false;
}

function showInvite(p: { fromId: string; fromName: string; msg: string }): void {
  inviteServerId = p.fromId;
  q<HTMLElement>("#inv-text").textContent = `“${p.fromName}” wants to run probes on this machine: ${
    p.msg || "(no message)"
  }`;
  inviteModal.hidden = false;
  let left = 300;
  const tick = () => {
    q<HTMLElement>("#inv-timer").textContent = `Expires in ${Math.floor(left / 60)}:${String(
      left % 60,
    ).padStart(2, "0")}`;
    if (left <= 0) respondInvite(false);
    left -= 1;
  };
  clearInterval(inviteTimer);
  tick();
  inviteTimer = window.setInterval(tick, 1000);
}

async function respondInvite(accept: boolean): Promise<void> {
  clearInterval(inviteTimer);
  inviteModal.hidden = true;
  if (inviteServerId) await invoke("net_respond_invite", { serverId: inviteServerId, accept });
  inviteServerId = null;
}

async function reloadTargets(): Promise<void> {
  const cfg = await invoke<AppConfig>("get_config");
  profiles = new Map(cfg.tags.map((t) => [t.tag, t]));
  for (const t of cfg.targets) {
    if (!rows.has(t.id)) await addRow(t);
  }
}

function wireNetwork(): void {
  q<HTMLButtonElement>("#net-btn").addEventListener("click", openNetwork);
  q<HTMLButtonElement>("#n-close").addEventListener("click", () => (netModal.hidden = true));
  netModal.addEventListener("click", (e) => {
    if (e.target === netModal) netModal.hidden = true;
  });
  q<HTMLInputElement>("#n-name").addEventListener("change", async (e) => {
    node = await invoke<NodeInfo>("set_node_name", { name: (e.target as HTMLInputElement).value });
    applyMode();
  });
  document.querySelectorAll<HTMLInputElement>('input[name="mode"]').forEach((r) =>
    r.addEventListener("change", async (e) => {
      node = await invoke<NodeInfo>("set_mode", { mode: (e.target as HTMLInputElement).value });
      applyMode();
    }),
  );
  q<HTMLButtonElement>("#n-discover").addEventListener("click", async () => {
    await invoke("net_discover_now");
    setTimeout(async () => renderDiscovered(await invoke<DiscoPeer[]>("net_discovered")), 900);
  });
  q<HTMLButtonElement>("#inv-accept").addEventListener("click", () => respondInvite(true));
  q<HTMLButtonElement>("#inv-decline").addEventListener("click", () => respondInvite(false));

  listen<DiscoPeer[]>("net-discovered", (e) => renderDiscovered(e.payload));
  listen<PeerInfo[]>("net-peers", (e) => renderPeers(e.payload));
  listen<{ fromId: string; fromName: string; msg: string }>("net-invite", (e) =>
    showInvite(e.payload),
  );
  listen("targets-changed", () => reloadTargets());
}

// ---- remote results (source-tagged) --------------------------------------

interface RemoteResult {
  id: number;
  host: string;
  tag: string;
  label: string;
  rttMs: number | null;
  lossPct: number;
  intervalMs: number;
}
interface RemoteEvent {
  fromId: string;
  fromName: string;
  ts: number;
  results: RemoteResult[];
}
interface RNode {
  fromId: string;
  fromName: string;
  host: string;
  tag: string;
  label: string;
  rttMs: number | null;
  lossPct: number;
  ts: number;
  buffer: Bucket[];
}

const remotes = new Map<string, RNode>();

function onRemote(ev: RemoteEvent): void {
  for (const r of ev.results) {
    const key = `${ev.fromId}:${r.id}`;
    let n = remotes.get(key);
    if (!n) {
      n = {
        fromId: ev.fromId,
        fromName: ev.fromName,
        host: r.host,
        tag: r.tag,
        label: r.label || r.host,
        rttMs: r.rttMs,
        lossPct: r.lossPct,
        ts: ev.ts,
        buffer: [],
      };
      remotes.set(key, n);
    }
    Object.assign(n, {
      fromName: ev.fromName,
      host: r.host,
      tag: r.tag,
      label: r.label || r.host,
      rttMs: r.rttMs,
      lossPct: r.lossPct,
      ts: ev.ts,
    });
    n.buffer.push({ t: ev.ts, worst: r.rttMs, loss: (r.lossPct || 0) / 100, count: 1, up: true });
    if (n.buffer.length > 300) n.buffer.shift();
  }
  refreshFilterOptions();
  renderRemote();
}

function refreshFilterOptions(): void {
  const sel = q<HTMLSelectElement>("#r-filter");
  const sources = new Map<string, string>();
  for (const n of remotes.values()) sources.set(n.fromId, n.fromName);
  const cur = sel.value;
  sel.innerHTML = '<option value="">all</option>';
  for (const [id, name] of sources) {
    const o = document.createElement("option");
    o.value = id;
    o.textContent = name;
    sel.append(o);
  }
  sel.value = cur;
}

function remoteRowEl(n: RNode): HTMLElement {
  const profile = profileFor(n.tag);
  const row = document.createElement("div");
  row.className = "row remote-row";
  row.style.borderLeftColor = tagColor(n.fromId);

  const src = document.createElement("span");
  src.className = "src";
  src.style.backgroundColor = tagColor(n.fromId);
  src.textContent = n.fromName;

  const meta = document.createElement("span");
  meta.className = "meta";
  const chip = document.createElement("span");
  chip.className = "tag";
  chip.textContent = n.tag;
  chip.style.backgroundColor = tagColor(n.tag);
  const label = document.createElement("span");
  label.className = "label";
  label.textContent = n.label;
  const detail = document.createElement("span");
  detail.className = "detail";
  detail.textContent = n.label !== n.host ? n.host : "";
  meta.append(chip, label, detail);

  const wrap = document.createElement("div");
  wrap.className = "wstats";
  const mk = (cls: string, text: string, color: string, title: string) => {
    const v = document.createElement("span");
    v.className = `val ${cls}`;
    v.title = title;
    v.textContent = text;
    if (color) v.style.color = color;
    return v;
  };
  wrap.append(
    mk(
      "s-avg",
      n.rttMs == null ? "—" : `${fmtMs(n.rttMs)}ms`,
      n.rttMs == null ? "" : bandColor(n.rttMs, profile),
      "avg response time",
    ),
    mk("s-jit", "—", "", "jitter (not reported by clients)"),
    mk("s-loss", fmtLoss(n.lossPct), lossColor(n.lossPct), "packet loss"),
  );

  const canvas = document.createElement("canvas");
  canvas.className = "strip";

  row.append(src, meta, wrap, canvas);
  requestAnimationFrame(() => drawBuckets(canvas, n.buffer, profile));
  return row;
}

function renderRemote(): void {
  const section = q<HTMLElement>("#remote-section");
  const host = q<HTMLElement>("#remote-rows");
  section.hidden = remotes.size === 0;
  const filter = q<HTMLSelectElement>("#r-filter").value;
  const sort = q<HTMLSelectElement>("#r-sort").value;

  let list = [...remotes.values()];
  if (filter) list = list.filter((n) => n.fromId === filter);
  list.sort((a, b) => {
    if (sort === "name") return a.label.localeCompare(b.label);
    if (sort === "rtt") return (b.rttMs ?? -1) - (a.rttMs ?? -1);
    if (sort === "loss") return b.lossPct - a.lossPct;
    return a.fromName.localeCompare(b.fromName) || a.label.localeCompare(b.label);
  });

  host.innerHTML = "";
  for (const n of list) host.append(remoteRowEl(n));
}

function wireRemote(): void {
  q<HTMLSelectElement>("#r-filter").addEventListener("change", renderRemote);
  q<HTMLSelectElement>("#r-sort").addEventListener("change", renderRemote);
  listen<RemoteEvent>("net-remote", (e) => onRemote(e.payload));
}

// ---- custom selection window ---------------------------------------------

function firstStripRect(): DOMRect | null {
  const r = rows.values().next().value as Row | undefined;
  if (!r) return null;
  const rect = r.canvas.getBoundingClientRect();
  return rect.width > 0 ? rect : null;
}

// Map a viewport X within the strips to an absolute time, clamped to the window ends
// (so dragging past an end stops at it).
function timeAtX(clientX: number, rect: DOMRect): number {
  const xf = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
  const w = currentWindow();
  return w.from + xf * (w.to - w.from);
}

// Absolute [from,to] of the selection given the current right edge, or null if inactive.
function selectionWindow(): { from: number; to: number } | null {
  if (!selection) return null;
  const end = currentWindow().to;
  const rMs = rangeMs();
  const s = Math.min(rMs, Math.max(0, selection.start));
  const e = Math.min(rMs, Math.max(0, selection.end));
  if (s - e < 1000) return null; // collapsed
  return { from: Math.round(end - s), to: Math.round(end - e) };
}

function updateSelectionOverlay(): void {
  if (!selOverlay) return;
  const sw = selectionWindow();
  const ref = firstStripRect();
  if (!sw || !ref) {
    selOverlay.hidden = true;
    return;
  }
  const w = currentWindow();
  const span = Math.max(1, w.to - w.from);
  const xA = ref.left + ((sw.from - w.from) / span) * ref.width;
  const xB = ref.left + ((sw.to - w.from) / span) * ref.width;
  const rrect = rowsEl.getBoundingClientRect();
  selOverlay.style.left = `${xA}px`;
  selOverlay.style.width = `${Math.max(1, xB - xA)}px`;
  selOverlay.style.top = `${rrect.top}px`;
  selOverlay.style.height = `${rrect.height}px`;
  selOverlay.hidden = false;
}

function wireSelection(): void {
  selOverlay = document.createElement("div");
  selOverlay.className = "selection";
  selOverlay.hidden = true;
  document.body.append(selOverlay);

  rowsEl.addEventListener("pointerdown", (e) => {
    if ((e.target as HTMLElement).tagName !== "CANVAS") return; // only the graph area
    const ref = firstStripRect();
    if (!ref) return;
    e.preventDefault();
    selDragging = true;
    let moved = false;
    let last = 0;
    const anchorT = timeAtX(e.clientX, ref);

    const onMove = (ev: PointerEvent) => {
      moved = true;
      const r = firstStripRect();
      if (!r) return;
      const curT = timeAtX(ev.clientX, r);
      const end = currentWindow().to;
      selection = {
        start: end - Math.min(anchorT, curT),
        end: end - Math.max(anchorT, curT),
      };
      updateSelectionOverlay();
      const now = Date.now();
      if (now - last >= 80) {
        last = now;
        refreshAll();
      }
    };
    const onUp = () => {
      document.removeEventListener("pointermove", onMove);
      document.removeEventListener("pointerup", onUp);
      selDragging = false;
      if (!moved) {
        selection = null; // a plain click clears the selection
        updateSelectionOverlay();
      }
      refreshAll();
    };
    document.addEventListener("pointermove", onMove);
    document.addEventListener("pointerup", onUp);
  });
}

// ---- bootstrap -----------------------------------------------------------

async function refreshBounds(): Promise<void> {
  try {
    const oldest = await invoke<number>("get_bounds");
    if (oldest > 0) state.oldestMs = oldest;
  } catch {
    /* ignore */
  }
}

async function main(): Promise<void> {
  // Mark the dev instance so it's easy to tell apart from the real collector.
  if ((import.meta as { env?: { DEV?: boolean } }).env?.DEV) {
    const b = document.createElement("span");
    b.className = "dev-badge";
    b.textContent = "DEV";
    document.querySelector(".topbar")?.prepend(b);
    document.title = "Speed Daemon (dev)";
  }

  const cfg = await invoke<AppConfig>("get_config");
  profiles = new Map(cfg.tags.map((t) => [t.tag, t]));
  themeSetting = cfg.theme || "system";
  aggregate = cfg.aggregate || "worst";
  applyTheme();

  buildControls();
  populateTagOptions();
  wireAddForm();
  wireEditor();
  wireSettings();
  wireCrosshair();
  wireSelection();
  wireNetwork();
  wireRemote();
  node = await invoke<NodeInfo>("get_node");
  applyMode();
  await refreshBounds();

  for (const target of cfg.targets) {
    await addRow(target);
  }

  await listen<SampleEvent>("sample", (event) => {
    const { targetId, resolvedIp } = event.payload;
    const row = rows.get(targetId);
    if (!row) return;
    if (resolvedIp && resolvedIp !== row.resolvedIp) {
      row.resolvedIp = resolvedIp;
      row.detailEl.textContent = detailText(row.target, resolvedIp);
    }
  });

  new ResizeObserver(scheduleRefresh).observe(rowsEl);
  window.addEventListener("resize", updateScrollbar);
  updateScrollbar();

  window.setInterval(() => {
    if (!state.paused && state.live && Date.now() - lastLiveRefresh >= liveIntervalMs()) {
      lastLiveRefresh = Date.now();
      refreshAll();
    }
  }, 500);
  window.setInterval(refreshBounds, 30000);
}

window.addEventListener("DOMContentLoaded", () => {
  main().catch((e) => console.error("startup failed", e));
});
