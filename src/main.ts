import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  drawBuckets,
  statsFromBuckets,
  bandColor,
  type Bucket,
  type TagProfile,
} from "./render";

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

interface Row {
  target: Target;
  rowEl: HTMLElement;
  canvas: HTMLCanvasElement;
  chip: HTMLElement;
  labelEl: HTMLElement;
  hostEl: HTMLElement;
  ipEl: HTMLElement;
  curEl: HTMLElement;
  medEl: HTMLElement;
  lossEl: HTMLElement;
  view: HTMLElement;
  editForm: HTMLFormElement;
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
  anchorFrac: 1, // 0 = oldest window, 1 = now (live)
  oldestMs: Date.now(),
};

const rows = new Map<string, Row>();
let profiles = new Map<string, TagProfile>();
let lastLiveRefresh = 0;

const rowsEl = document.querySelector<HTMLElement>("#rows")!;

// ---- helpers -------------------------------------------------------------

function fmtMs(v: number | null): string {
  if (v == null) return "—";
  return v < 10 ? v.toFixed(1) : String(Math.round(v));
}

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

function currentWindow(): { from: number; to: number } {
  const now = Date.now();
  const rangeMs = state.rangeSec * 1000;
  const minEnd = state.oldestMs + rangeMs;
  let end: number;
  if (state.live) {
    end = now;
  } else {
    end = minEnd + state.anchorFrac * Math.max(0, now - minEnd);
    end = Math.min(now, Math.max(minEnd, end));
  }
  return { from: Math.round(end - rangeMs), to: Math.round(end) };
}

function liveIntervalMs(): number {
  if (state.rangeSec <= 43200) return 1000; // <=12h
  if (state.rangeSec <= 86400) return 3000; // 1d
  return 15000; // 1w
}

// ---- rendering -----------------------------------------------------------

async function refreshRow(row: Row): Promise<void> {
  const { from, to } = currentWindow();
  const n = Math.max(1, Math.floor(row.canvas.clientWidth));
  let buckets: Bucket[];
  try {
    buckets = await invoke<Bucket[]>("get_window", {
      targetId: row.target.id,
      from,
      to,
      buckets: n,
    });
  } catch {
    return;
  }
  const profile = profileFor(row.target.tag);
  drawBuckets(row.canvas, buckets, profile);

  const s = statsFromBuckets(buckets);
  if (s.current != null) {
    row.curEl.textContent = fmtMs(s.current);
    row.curEl.style.color = bandColor(s.current, profile);
  }
  row.medEl.textContent = `med ${fmtMs(s.median)}`;
  row.lossEl.textContent = `${s.lossPct.toFixed(0)}% loss`;
  row.lossEl.classList.toggle("bad", s.lossPct > 0);
}

async function refreshAll(): Promise<void> {
  await Promise.all([...rows.values()].map(refreshRow));
}

let resizeTimer = 0;
function scheduleRefresh(): void {
  clearTimeout(resizeTimer);
  resizeTimer = window.setTimeout(refreshAll, 60);
}

// ---- row construction ----------------------------------------------------

function buildRow(target: Target): Row {
  const rowEl = document.createElement("div");
  rowEl.className = "row";
  rowEl.dataset.id = target.id;

  const handle = document.createElement("span");
  handle.className = "handle";
  handle.textContent = "⠿";
  handle.title = "Drag to reorder";
  handle.draggable = true;

  // ---- meta (view + edit form) ----
  const meta = document.createElement("div");
  meta.className = "meta";

  const view = document.createElement("div");
  view.className = "view";

  const top = document.createElement("div");
  top.className = "meta-top";
  const chip = document.createElement("span");
  chip.className = "tag";
  chip.textContent = target.tag;
  chip.style.backgroundColor = tagColor(target.tag);
  const labelEl = document.createElement("span");
  labelEl.className = "label";
  labelEl.textContent = target.label;
  top.append(chip, labelEl);

  const sub = document.createElement("div");
  sub.className = "meta-sub";
  const hostEl = document.createElement("span");
  hostEl.className = "host";
  hostEl.textContent = target.host;
  const ipEl = document.createElement("span");
  ipEl.className = "ip";
  sub.append(hostEl, ipEl);

  const stats = document.createElement("div");
  stats.className = "stats";
  const curEl = document.createElement("span");
  curEl.className = "cur";
  curEl.textContent = "—";
  const medEl = document.createElement("span");
  medEl.className = "med";
  const lossEl = document.createElement("span");
  lossEl.className = "loss";
  stats.append(curEl, medEl, lossEl);

  view.append(top, sub, stats);

  const editForm = buildEditForm(target);

  meta.append(view, editForm);

  const canvas = document.createElement("canvas");
  canvas.className = "strip";

  const actions = document.createElement("div");
  actions.className = "actions";
  const editBtn = document.createElement("button");
  editBtn.className = "icon";
  editBtn.textContent = "✎";
  editBtn.title = "Edit";
  editBtn.addEventListener("click", () => toggleEdit(target.id, true));
  const removeBtn = document.createElement("button");
  removeBtn.className = "icon remove";
  removeBtn.textContent = "×";
  removeBtn.title = "Remove";
  removeBtn.addEventListener("click", () => removeTarget(target.id));
  actions.append(editBtn, removeBtn);

  rowEl.append(handle, meta, canvas, actions);
  rowsEl.append(rowEl);

  wireDrag(handle, rowEl);

  const row: Row = {
    target,
    rowEl,
    canvas,
    chip,
    labelEl,
    hostEl,
    ipEl,
    curEl,
    medEl,
    lossEl,
    view,
    editForm,
  };
  return row;
}

function buildEditForm(target: Target): HTMLFormElement {
  const form = document.createElement("form");
  form.className = "edit-form";
  form.hidden = true;
  form.innerHTML = `
    <input class="e-label" placeholder="label" value="${escapeAttr(target.label)}" />
    <input class="e-host" placeholder="host or IP" value="${escapeAttr(target.host)}" />
    <input class="e-tag" placeholder="tag" value="${escapeAttr(target.tag)}" list="tag-options" />
    <input class="e-interval" type="number" min="200" step="100" value="${target.intervalMs}" />
    <div class="edit-actions">
      <button type="submit" class="save">save</button>
      <button type="button" class="cancel">cancel</button>
    </div>`;
  form.querySelector<HTMLButtonElement>(".cancel")!.addEventListener("click", () =>
    toggleEdit(target.id, false),
  );
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    saveEdit(target.id);
  });
  return form;
}

function escapeAttr(s: string): string {
  return s.replace(/"/g, "&quot;");
}

function toggleEdit(id: string, editing: boolean): void {
  const row = rows.get(id);
  if (!row) return;
  row.view.hidden = editing;
  row.editForm.hidden = !editing;
  if (editing) row.editForm.querySelector<HTMLInputElement>(".e-label")?.focus();
}

async function saveEdit(id: string): Promise<void> {
  const row = rows.get(id);
  if (!row) return;
  const f = row.editForm;
  const label = f.querySelector<HTMLInputElement>(".e-label")!.value.trim();
  const host = f.querySelector<HTMLInputElement>(".e-host")!.value.trim();
  const tag = f.querySelector<HTMLInputElement>(".e-tag")!.value.trim() || "internet";
  const intervalMs = Math.max(
    200,
    parseInt(f.querySelector<HTMLInputElement>(".e-interval")!.value, 10) || 1000,
  );
  if (!host) return;

  const updated: Target = { id, label: label || host, host, tag, intervalMs };
  try {
    await invoke<AppConfig>("update_target", { target: updated });
  } catch (e) {
    console.error("update_target failed", e);
    return;
  }
  row.target = updated;
  row.labelEl.textContent = updated.label;
  row.hostEl.textContent = updated.host;
  row.chip.textContent = updated.tag;
  row.chip.style.backgroundColor = tagColor(updated.tag);
  toggleEdit(id, false);
  refreshRow(row);
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

// ---- drag reorder --------------------------------------------------------

function wireDrag(handle: HTMLElement, rowEl: HTMLElement): void {
  handle.addEventListener("dragstart", (e) => {
    rowEl.classList.add("dragging");
    e.dataTransfer?.setData("text/plain", rowEl.dataset.id ?? "");
    if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
  });
  handle.addEventListener("dragend", () => {
    rowEl.classList.remove("dragging");
    persistOrder();
  });
}

rowsEl.addEventListener("dragover", (e) => {
  e.preventDefault();
  const dragging = rowsEl.querySelector<HTMLElement>(".row.dragging");
  if (!dragging) return;
  const after = rowAfter(e.clientY);
  if (after == null) rowsEl.appendChild(dragging);
  else rowsEl.insertBefore(dragging, after);
});

function rowAfter(y: number): HTMLElement | null {
  const els = [...rowsEl.querySelectorAll<HTMLElement>(".row:not(.dragging)")];
  for (const el of els) {
    const box = el.getBoundingClientRect();
    if (y < box.top + box.height / 2) return el;
  }
  return null;
}

async function persistOrder(): Promise<void> {
  const order = [...rowsEl.querySelectorAll<HTMLElement>(".row")].map(
    (el) => el.dataset.id!,
  );
  try {
    await invoke("reorder_targets", { order });
  } catch (e) {
    console.error("reorder_targets failed", e);
  }
}

// ---- controls ------------------------------------------------------------

function buildControls(): void {
  const rangeWrap = document.querySelector<HTMLElement>("#ranges")!;
  for (const r of RANGES) {
    const btn = document.createElement("button");
    btn.className = "range" + (r.sec === state.rangeSec ? " active" : "");
    btn.textContent = r.key;
    btn.dataset.sec = String(r.sec);
    btn.addEventListener("click", () => {
      state.rangeSec = r.sec;
      rangeWrap
        .querySelectorAll(".range")
        .forEach((b) => b.classList.toggle("active", b === btn));
      refreshAll();
    });
    rangeWrap.append(btn);
  }

  const slider = document.querySelector<HTMLInputElement>("#scrub")!;
  const liveBtn = document.querySelector<HTMLButtonElement>("#live-btn")!;
  slider.addEventListener("input", () => {
    const v = parseInt(slider.value, 10);
    state.anchorFrac = v / 1000;
    state.live = v >= 1000;
    liveBtn.classList.toggle("active", state.live);
    scheduleRefresh();
  });
  liveBtn.addEventListener("click", () => {
    state.live = true;
    state.anchorFrac = 1;
    slider.value = "1000";
    liveBtn.classList.add("active");
    refreshAll();
  });
  liveBtn.classList.toggle("active", state.live);
}

function wireAddForm(): void {
  const form = document.querySelector<HTMLFormElement>("#add-form")!;
  const labelI = document.querySelector<HTMLInputElement>("#f-label")!;
  const hostI = document.querySelector<HTMLInputElement>("#f-host")!;
  const tagI = document.querySelector<HTMLInputElement>("#f-tag")!;
  const intervalI = document.querySelector<HTMLInputElement>("#f-interval")!;

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const label = labelI.value.trim();
    const host = hostI.value.trim();
    const tag = tagI.value.trim() || "internet";
    const intervalMs = Math.max(200, parseInt(intervalI.value, 10) || 1000);
    if (!host) return;
    const target: Target = {
      id: `${slug(label || host)}-${Math.floor(Math.random() * 1e6)}`,
      label: label || host,
      host,
      tag,
      intervalMs,
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
  const list = document.querySelector<HTMLDataListElement>("#tag-options")!;
  list.innerHTML = "";
  for (const t of profiles.keys()) {
    const opt = document.createElement("option");
    opt.value = t;
    list.append(opt);
  }
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
  await refreshBounds();

  for (const target of cfg.targets) {
    await addRow(target);
  }

  // Live current-value updates between full refreshes.
  await listen<SampleEvent>("sample", (event) => {
    const { targetId, resolvedIp, sample } = event.payload;
    const row = rows.get(targetId);
    if (!row) return;
    if (state.live && sample.rttMs != null) {
      row.curEl.textContent = fmtMs(sample.rttMs);
      row.curEl.style.color = bandColor(sample.rttMs, profileFor(row.target.tag));
    }
    if (resolvedIp && row.ipEl.textContent !== resolvedIp) {
      row.ipEl.textContent = resolvedIp;
    }
  });

  // Redraw to fit on resize (bucket count = canvas width).
  new ResizeObserver(scheduleRefresh).observe(rowsEl);

  // Live refresh loop (tiered by range) + periodic bounds refresh.
  window.setInterval(() => {
    if (state.live && Date.now() - lastLiveRefresh >= liveIntervalMs()) {
      lastLiveRefresh = Date.now();
      refreshAll();
    }
  }, 500);
  window.setInterval(refreshBounds, 30000);
}

window.addEventListener("DOMContentLoaded", () => {
  main().catch((e) => console.error("startup failed", e));
});
