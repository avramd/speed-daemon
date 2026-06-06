import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { drawBuckets, bandColor, type Bucket, type TagProfile } from "./render";

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
};

const rows = new Map<string, Row>();
let profiles = new Map<string, TagProfile>();
let lastLiveRefresh = 0;
let editingId: string | null = null;

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

// Loss color scale: <0.5% white, 0.5-1% yellow, 1-3% orange, >3% red.
function lossColor(p: number): string {
  if (p < 0.5) return "#e6edf3";
  if (p <= 1) return "#d4c531";
  if (p <= 3) return "#db8b2a";
  return "#e5484d";
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
  const rangeMs = state.rangeSec * 1000;
  const minEnd = state.oldestMs + rangeMs;
  let end: number;
  if (state.paused) {
    end = state.frozenTo;
  } else if (state.live) {
    end = now;
  } else {
    end = minEnd + state.anchorFrac * Math.max(0, now - minEnd);
    end = Math.min(now, Math.max(minEnd, end));
  }
  return { from: Math.round(end - rangeMs), to: Math.round(end) };
}

function liveIntervalMs(): number {
  if (state.rangeSec <= 43200) return 1000;
  if (state.rangeSec <= 86400) return 3000;
  return 15000;
}

// ---- rendering -----------------------------------------------------------

async function refreshRow(row: Row): Promise<void> {
  const { from, to } = currentWindow();
  const n = Math.max(1, Math.floor(row.canvas.clientWidth));
  const id = row.target.id;
  try {
    const [buckets, stats] = await Promise.all([
      invoke<Bucket[]>("get_window", { targetId: id, from, to, buckets: n }),
      invoke<WindowStats>("get_stats", { targetId: id, from, to }),
    ]);
    const profile = profileFor(row.target.tag);
    row.buckets = buckets;
    drawBuckets(row.canvas, buckets, profile);

    row.avgEl.textContent = fmtMs(stats.avg);
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

  // ---- window stats column (avg / jitter / loss) ----
  const wstats = document.createElement("div");
  wstats.className = "wstats";
  const avgEl = document.createElement("span");
  avgEl.className = "val";
  avgEl.textContent = "—";
  const jitEl = document.createElement("span");
  jitEl.className = "val";
  jitEl.textContent = "—";
  const lossEl = document.createElement("span");
  lossEl.className = "val";
  lossEl.textContent = "0%";
  const statLine = (k: string, v: HTMLElement) => {
    const line = document.createElement("div");
    line.className = "wline";
    const key = document.createElement("span");
    key.className = "k";
    key.textContent = k;
    line.append(key, v);
    return line;
  };
  wstats.append(statLine("avg", avgEl), statLine("jit", jitEl), statLine("loss", lossEl));

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
  const intervalMs = Math.max(
    200,
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

function buildControls(): void {
  const rangeWrap = q<HTMLElement>("#ranges");
  for (const r of RANGES) {
    const btn = document.createElement("button");
    btn.className = "range" + (r.sec === state.rangeSec ? " active" : "");
    btn.textContent = r.key;
    btn.addEventListener("click", () => {
      state.rangeSec = r.sec;
      rangeWrap.querySelectorAll(".range").forEach((b) => b.classList.toggle("active", b === btn));
      refreshAll();
    });
    rangeWrap.append(btn);
  }

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
    const apply = (clientX: number) => {
      const x = clientX - rect.left - grab;
      const nl = Math.min(geo.maxLeft, Math.max(0, x));
      state.anchorFrac = geo.maxLeft > 0 ? nl / geo.maxLeft : 1;
      state.live = state.anchorFrac >= 0.999;
      liveBtn.classList.toggle("active", state.live);
      updateScrollbar();
      // Throttle (not debounce) so the graphs scroll live while dragging.
      const now = Date.now();
      if (now - lastRefresh >= 60) {
        lastRefresh = now;
        refreshAll();
      }
    };
    apply(e.clientX);
    const onMove = (ev: PointerEvent) => apply(ev.clientX);
    const onUp = () => {
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
}

// Scrollbar thumb sizing: width = window / total-data-span, position from anchor.
function thumbGeometry(): { thumbW: number; maxLeft: number } {
  const track = q<HTMLElement>("#scrub");
  const trackW = track.clientWidth;
  const span = Math.max(1, Date.now() - state.oldestMs);
  const windowMs = state.rangeSec * 1000;
  const frac = Math.min(1, windowMs / span);
  const thumbW = Math.min(trackW, Math.max(16, frac * trackW));
  const maxLeft = Math.max(0, trackW - thumbW);
  return { thumbW, maxLeft };
}

function updateScrollbar(): void {
  const thumb = q<HTMLElement>("#scrub-thumb");
  const { thumbW, maxLeft } = thumbGeometry();
  const now = Date.now();
  const minEnd = state.oldestMs + state.rangeSec * 1000;
  const denom = now - minEnd;
  const frac = denom <= 0 ? 1 : Math.min(1, Math.max(0, (currentWindow().to - minEnd) / denom));
  thumb.style.width = `${thumbW}px`;
  thumb.style.transform = `translateX(${frac * maxLeft}px)`;
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
  const cfg = await invoke<AppConfig>("get_config");
  profiles = new Map(cfg.tags.map((t) => [t.tag, t]));

  buildControls();
  populateTagOptions();
  wireAddForm();
  wireEditor();
  wireSettings();
  wireCrosshair();
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
