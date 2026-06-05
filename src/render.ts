// Canvas strip-chart renderer. Each bucket is a 1px-wide column (one per pixel of width,
// so the selected time range always fills the canvas). Height = log(worst RTT in the
// slice), color = expectation-aware band. Visual states:
//   - app up + replies      -> colored bar (+ red baseline tick if partial loss)
//   - app up + all lost     -> red baseline tick (gap above)
//   - app NOT running (!up)  -> dim grey column
//   - no data               -> blank

export interface Bucket {
  t: number;
  worst: number | null;
  loss: number;
  count: number;
  up: boolean;
}

export interface TagProfile {
  tag: string;
  good: number;
  ok: number;
  poor: number;
  terrible: number;
}

const BG = "#0d1117";
const GREY = "#2a2f37"; // "app wasn't running"
const GREEN = "#3fb950";
const YELLOW = "#d4c531";
const ORANGE = "#db8b2a";
const RED = "#e5484d";

export function bandColor(rtt: number, p: TagProfile): string {
  if (rtt <= p.good) return GREEN;
  if (rtt <= p.ok) return YELLOW;
  if (rtt <= p.poor) return ORANGE;
  return RED;
}

function logHeight(rtt: number, hi: number, rowH: number): number {
  const LO = 1;
  const r = Math.max(rtt, LO);
  const f = (Math.log(r) - Math.log(LO)) / (Math.log(hi) - Math.log(LO));
  return Math.max(1, Math.max(0, Math.min(1, f)) * rowH);
}

/** Draw one bucket per pixel column, oldest at left, newest at right. */
export function drawBuckets(
  canvas: HTMLCanvasElement,
  buckets: Bucket[],
  profile: TagProfile,
): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  const dpr = window.devicePixelRatio || 1;
  const cssW = canvas.clientWidth;
  const cssH = canvas.clientHeight;
  if (cssW === 0 || cssH === 0) return;

  const bw = Math.round(cssW * dpr);
  const bh = Math.round(cssH * dpr);
  if (canvas.width !== bw || canvas.height !== bh) {
    canvas.width = bw;
    canvas.height = bh;
  }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.fillStyle = BG;
  ctx.fillRect(0, 0, cssW, cssH);

  const hi = profile.terrible * 2.5;
  const n = buckets.length;
  // Buckets map 1:1 to pixels when we asked for floor(width) of them; if not, scale x.
  const scale = n > 0 ? cssW / n : 1;

  for (let i = 0; i < n; i++) {
    const b = buckets[i];
    const x = Math.floor(i * scale);
    const w = Math.max(1, Math.ceil(scale));

    if (!b.up) {
      ctx.fillStyle = GREY;
      ctx.fillRect(x, 0, w, cssH);
      continue;
    }
    if (b.count === 0) continue; // up but no probe -> blank

    if (b.worst == null) {
      // ran but every probe lost -> red baseline tick
      ctx.fillStyle = RED;
      ctx.fillRect(x, cssH - 3, w, 3);
      continue;
    }

    const h = logHeight(b.worst, hi, cssH);
    ctx.fillStyle = bandColor(b.worst, profile);
    ctx.fillRect(x, cssH - h, w, h);
    if (b.loss > 0) {
      ctx.fillStyle = RED;
      ctx.fillRect(x, cssH - 1, w, 1); // partial-loss marker
    }
  }
}

export interface Stats {
  current: number | null;
  median: number | null;
  lossPct: number;
}

/** Window summary derived from the rendered buckets. */
export function statsFromBuckets(buckets: Bucket[]): Stats {
  const worsts: number[] = [];
  let lostWeighted = 0;
  let total = 0;
  let current: number | null = null;
  for (const b of buckets) {
    if (b.count > 0) {
      total += b.count;
      lostWeighted += b.loss * b.count;
    }
    if (b.worst != null) {
      worsts.push(b.worst);
      current = b.worst; // last non-null wins
    }
  }
  worsts.sort((a, b) => a - b);
  const median = worsts.length ? worsts[Math.floor(worsts.length / 2)] : null;
  const lossPct = total > 0 ? (lostWeighted / total) * 100 : 0;
  return { current, median, lossPct };
}
