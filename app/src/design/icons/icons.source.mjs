// =============================================================================
// Prism icon source — the single source of truth for the icon set.
// 24×24 grid · 1.75 stroke · round caps & joins · currentColor.
// Keylines: circle r=9 (3–21), square 16 (4–20), portrait 14×17, landscape 18×14.
// Live area 2–22; stroke edges stay ≥ 1.5 px from the canvas edge.
//
// Run `node app/src/design/icons/generate.mjs` after editing to regenerate:
//   icons/svg/*.svg, icons/index.tsx, icons/names.ts, design/assets/icons.js
// =============================================================================

const f = (n) => +n.toFixed(2);

// ---- primitive node builders ------------------------------------------------
export const p = (d) => ["path", { d }];
export const c = (cx, cy, r, extra = {}) => ["circle", { cx, cy, r, ...extra }];
export const r = (x, y, width, height, rx = 0, extra = {}) => ["rect", { x, y, width, height, rx, ...extra }];
/** Filled dot (no stroke) — used for "i", "!", ellipsis, drag handles. */
export const dot = (cx, cy, rr = 1.1) => ["circle", { cx, cy, r: rr, fill: "currentColor", stroke: "none" }];
/** Filled shape (no stroke). */
export const fill = (d) => ["path", { d, fill: "currentColor", stroke: "none" }];

// ---- geometric helpers ------------------------------------------------------
/** Closed polygon with circular-arc rounded corners. Points in drawing order. */
export function roundedPoly(pts, rad) {
  const n = pts.length;
  let d = "";
  for (let i = 0; i < n; i++) {
    const p0 = pts[(i - 1 + n) % n], p1 = pts[i], p2 = pts[(i + 1) % n];
    const v1 = [p0[0] - p1[0], p0[1] - p1[1]];
    const v2 = [p2[0] - p1[0], p2[1] - p1[1]];
    const l1 = Math.hypot(...v1), l2 = Math.hypot(...v2);
    const u1 = [v1[0] / l1, v1[1] / l1], u2 = [v2[0] / l2, v2[1] / l2];
    const theta = Math.acos(Math.max(-1, Math.min(1, u1[0] * u2[0] + u1[1] * u2[1])));
    let t = rad / Math.tan(theta / 2);
    t = Math.min(t, l1 / 2, l2 / 2);
    const rr = t * Math.tan(theta / 2);
    const a = [p1[0] + u1[0] * t, p1[1] + u1[1] * t];
    const b = [p1[0] + u2[0] * t, p1[1] + u2[1] * t];
    const cross = (p1[0] - p0[0]) * (p2[1] - p1[1]) - (p1[1] - p0[1]) * (p2[0] - p1[0]);
    const sweep = cross > 0 ? 1 : 0;
    d += `${i === 0 ? "M" : "L"}${f(a[0])} ${f(a[1])}A${f(rr)} ${f(rr)} 0 0 ${sweep} ${f(b[0])} ${f(b[1])}`;
  }
  return d + "Z";
}

/** Crescent: circle A minus circle B (the "bite"). */
export function crescent(ax, ay, ar, bx, by, br) {
  const dx = bx - ax, dy = by - ay, dd = Math.hypot(dx, dy);
  const a = (ar * ar - br * br + dd * dd) / (2 * dd);
  const h = Math.sqrt(ar * ar - a * a);
  const mx = ax + (a * dx) / dd, my = ay + (a * dy) / dd;
  const P = [mx + (h * -dy) / dd, my + (h * dx) / dd];
  const Q = [mx - (h * -dy) / dd, my - (h * dx) / dd];
  const ang = (cx, cy, pt) => Math.atan2(pt[1] - cy, pt[0] - cx);
  const TAU = Math.PI * 2;
  // outer arc on A from P to Q whose midpoint lies OUTSIDE B
  const aP = ang(ax, ay, P), aQ = ang(ax, ay, Q);
  const cw = (((aQ - aP) % TAU) + TAU) % TAU; // clockwise (screen) span P→Q
  const midCw = aP + cw / 2;
  const mCw = [ax + ar * Math.cos(midCw), ay + ar * Math.sin(midCw)];
  const outsideB = Math.hypot(mCw[0] - bx, mCw[1] - by) > br;
  const outerSweep = outsideB ? 1 : 0;
  const outerSpan = outsideB ? cw : TAU - cw;
  // inner arc on B from Q back to P whose midpoint lies INSIDE A
  const bQ = ang(bx, by, Q), bP = ang(bx, by, P);
  const cw2 = (((bP - bQ) % TAU) + TAU) % TAU;
  const mid2 = bQ + cw2 / 2;
  const m2 = [bx + br * Math.cos(mid2), by + br * Math.sin(mid2)];
  const insideA = Math.hypot(m2[0] - ax, m2[1] - ay) < ar;
  const innerSweep = insideA ? 1 : 0;
  const innerSpan = insideA ? cw2 : TAU - cw2;
  return (
    `M${f(P[0])} ${f(P[1])}` +
    `A${ar} ${ar} 0 ${outerSpan > Math.PI ? 1 : 0} ${outerSweep} ${f(Q[0])} ${f(Q[1])}` +
    `A${br} ${br} 0 ${innerSpan > Math.PI ? 1 : 0} ${innerSweep} ${f(P[0])} ${f(P[1])}Z`
  );
}

/** Cog outline: `teeth` trapezoid teeth between radii ri and ro. */
export function gear(cx, cy, teeth, ro, ri, toothDeg = 22, taperDeg = 9, round = 0.7) {
  const pts = [];
  const step = (Math.PI * 2) / teeth;
  const tw = (toothDeg * Math.PI) / 180 / 2;
  const tp = (taperDeg * Math.PI) / 180;
  const pt = (ang, rad) => [cx + rad * Math.cos(ang), cy + rad * Math.sin(ang)];
  for (let i = 0; i < teeth; i++) {
    const a = -Math.PI / 2 + i * step;
    pts.push(pt(a - tw - tp, ri), pt(a - tw, ro), pt(a + tw, ro), pt(a + tw + tp, ri));
  }
  return roundedPoly(pts, round);
}

/** Radial rays (sun, sparkle). */
export function rays(cx, cy, r1, r2, n, offsetDeg = 0) {
  let d = "";
  for (let i = 0; i < n; i++) {
    const a = ((offsetDeg + (360 / n) * i) * Math.PI) / 180;
    d += `M${f(cx + r1 * Math.cos(a))} ${f(cy + r1 * Math.sin(a))}L${f(cx + r2 * Math.cos(a))} ${f(cy + r2 * Math.sin(a))}`;
  }
  return d;
}

// ---- shared parts -----------------------------------------------------------
const FILE = "M14 3.5H7.5A2.5 2.5 0 0 0 5 6v12a2.5 2.5 0 0 0 2.5 2.5h9A2.5 2.5 0 0 0 19 18V8.5z";
const FOLD = "M14 3.5V7a1.5 1.5 0 0 0 1.5 1.5H19";
const file = (...glyph) => [p(FILE), p(FOLD), ...glyph];

const FOLDER = "M3.5 7A2.5 2.5 0 0 1 6 4.5h3.17a1.5 1.5 0 0 1 1.06.44L12 6.75h6a2.5 2.5 0 0 1 2.5 2.5v8.25A2.5 2.5 0 0 1 18 20H6a2.5 2.5 0 0 1-2.5-2.5z";
const TRASH_LID = "M4 6.5h16";
const TRASH_HANDLE = "M9.5 6.5V5A1.5 1.5 0 0 1 11 3.5h2A1.5 1.5 0 0 1 14.5 5v1.5";
const TRASH_BODY = "M6 6.5l.9 12.1a2 2 0 0 0 2 1.9h6.2a2 2 0 0 0 2-1.9L18 6.5";
const RING = c(12, 12, 9);
const LINK = [
  p("M9 15l6-6"),
  p("M11 6.5l1.2-1.2a4 4 0 0 1 5.6 5.6L16.6 12.1"),
  p("M13 17.5l-1.2 1.2a4 4 0 0 1-5.6-5.6L7.4 11.9"),
];
const LINES3 = p("M4 6.5h12M4 11.5h8M4 16.5h5");
const SHIELD = "M12 3.25l7.25 2.9v5.6c0 4.3-3 7.9-7.25 9.25-4.25-1.35-7.25-4.95-7.25-9.25v-5.6z";

// ---- the set ----------------------------------------------------------------
// group → name → nodes. Groups are only used for the specimen sheet & docs.
export const groups = {
  toolbar: {
    "add-url": [
      p("M17.5 3v7M14 6.5h7"),
      p("M7.75 16.25l4.5-4.5"),
      p("M9.25 9.88l.9-.9a3 3 0 0 1 4.2 4.2l-.9.9"),
      p("M10.75 18.13l-.9.9a3 3 0 0 1-4.2-4.2l.9-.9"),
    ],
    resume: [p(roundedPoly([[8, 4.75], [20.25, 12], [8, 19.25]], 1.6))],
    pause: [r(6.25, 5, 4, 14, 1.25), r(13.75, 5, 4, 14, 1.25)],
    stop: [r(5.5, 5.5, 13, 13, 2.5)],
    "stop-all": [r(3.5, 8.5, 12, 12, 2.5), p("M7.5 5.5A2 2 0 0 1 9.5 3.5h9a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2")],
    delete: [p(TRASH_LID), p(TRASH_HANDLE), p(TRASH_BODY), p("M10 10.5v6M14 10.5v6")],
    "delete-completed": [p(TRASH_LID), p(TRASH_HANDLE), p(TRASH_BODY), p("M9.5 13.4l1.75 1.75 3.25-3.5")],
    options: [p("M4 7h9M17 7h3M4 17h3M11 17h9"), c(15, 7, 2), c(9, 17, 2)],
    settings: [p(gear(12, 12, 8, 9.25, 6.9, 22, 10, 0.75)), c(12, 12, 2.75)],
    scheduler: [c(12, 13, 7.25), p("M12 9.5V13l2.5 1.75"), p("M4.5 5.75L7 3.75M19.5 5.75L17 3.75"), p("M7.25 19.25L6 20.75M16.75 19.25L18 20.75")],
    "start-queue": [LINES3, p(roundedPoly([[14, 13], [20.5, 16.75], [14, 20.5]], 0.9))],
    "stop-queue": [LINES3, r(14, 13.75, 6.5, 6.5, 1.5)],
    grabber: [c(9.5, 9.5, 6), ["ellipse", { cx: 9.5, cy: 9.5, rx: 2.5, ry: 6 }], p("M3.5 9.5h12"), p("M17.75 12v8.25M15 17.5l2.75 2.75 2.75-2.75")],
    "speed-limiter": [p("M4.64 16.75A8.5 8.5 0 1 1 19.36 16.75"), p("M12 12.5l3.75-4.25"), c(12, 12.5, 1.25)],
    batch: [p("M12 3.5l8.5 4.5-8.5 4.5L3.5 8z"), p("M3.5 12l8.5 4.5 8.5-4.5"), p("M3.5 16l8.5 4.5 8.5-4.5")],
    // An AI model (Hugging Face): a small network.
    model: [c(5.5, 7, 2), c(5.5, 17, 2), c(12, 12, 2.25), c(18.5, 7, 2), c(18.5, 17, 2), p("M7.2 8.3l2.95 2.35M7.2 15.7l2.95-2.35M13.85 10.65l2.95-2.35M13.85 13.35l2.95 2.35")],
    import: [p("M14 4.5h3.5A2.5 2.5 0 0 1 20 7v10a2.5 2.5 0 0 1-2.5 2.5H14"), p("M4 12h11M11 8l4 4-4 4")],
    export: [p("M10 4.5H6.5A2.5 2.5 0 0 0 4 7v10a2.5 2.5 0 0 0 2.5 2.5H10"), p("M9 12h11M16 8l4 4-4 4")],
    search: [c(10.75, 10.75, 6.5), p("M15.5 15.5l4.75 4.75")],
    filter: [p(roundedPoly([[4, 5], [20, 5], [14, 12.5], [14, 18.25], [10, 20.25], [10, 12.5]], 1))],
    more: [dot(5.5, 12, 1.4), dot(12, 12, 1.4), dot(18.5, 12, 1.4)],
    "more-vertical": [dot(12, 5.5, 1.4), dot(12, 12, 1.4), dot(12, 18.5, 1.4)],
    columns: [r(3.5, 4.5, 17, 15, 2.5), p("M9.25 4.5v15M14.75 4.5v15")],
    download: [p("M12 3.5v11M7.5 10l4.5 4.5 4.5-4.5"), p("M4.5 15.5V18A2.5 2.5 0 0 0 7 20.5h10a2.5 2.5 0 0 0 2.5-2.5v-2.5")],
    upload: [p("M12 14.5v-11M7.5 8L12 3.5 16.5 8"), p("M4.5 15.5V18A2.5 2.5 0 0 0 7 20.5h10a2.5 2.5 0 0 0 2.5-2.5v-2.5")],
  },

  categories: {
    all: [r(4, 4, 6.75, 6.75, 2), r(13.25, 4, 6.75, 6.75, 2), r(4, 13.25, 6.75, 6.75, 2), r(13.25, 13.25, 6.75, 6.75, 2)],
    video: [r(3, 5, 18, 14, 3), p(roundedPoly([[10.25, 9.25], [15, 12], [10.25, 14.75]], 0.7))],
    music: [p("M9 17.5V6.25l10.5-2v11.25"), p("M9 9.75l10.5-2"), c(6.5, 17.5, 2.5), c(17, 15.5, 2.5)],
    documents: file(p("M8.5 12.5h7M8.5 16h4.5")),
    programs: [p(roundedPoly([[12, 3], [20, 7.5], [20, 16.5], [12, 21], [4, 16.5], [4, 7.5]], 1.4)), p("M4.4 7.75L12 12l7.6-4.25M12 12v8.6")],
    compressed: [r(3.5, 4, 17, 5, 1.5), p("M5 9v8.5A2.5 2.5 0 0 0 7.5 20h9a2.5 2.5 0 0 0 2.5-2.5V9"), p("M10 13h4")],
    images: [r(3.5, 4.5, 17, 15, 2.5), c(9, 9.75, 1.75), p("M3.75 16.75l4.4-4.4a1.2 1.2 0 0 1 1.7 0l6.65 6.65M14 15l1.65-1.65a1.2 1.2 0 0 1 1.7 0L20.25 16.25")],
    other: [c(8, 8, 3.75), r(13, 13, 7, 7, 1.75), p(roundedPoly([[16.5, 3.5], [20.5, 10.25], [12.5, 10.25]], 0.8)), p("M4.5 16.5h7M8 13v7")],
    folder: [p(FOLDER)],
    "custom-folder": [p(FOLDER), p("M12 10.5v6M9 13.5h6")],
    unfinished: [p("M12 3a9 9 0 1 1-9 9"), p("M12 7.75v6.75M9.25 11.75L12 14.5l2.75-2.75"), dot(3.9, 8.1, 0.95), dot(6.6, 4.9, 0.95)],
    finished: [RING, p("M8 12.25l2.75 2.75L16 9.5")],
    queue: [dot(5, 6.5, 1.25), dot(5, 12, 1.25), dot(5, 17.5, 1.25), p("M9.5 6.5h10M9.5 12h10M9.5 17.5h6.5")],
    "grabber-project": [r(9, 3.5, 6, 4.5, 1.25), r(3.25, 16, 5.5, 4.5, 1.25), r(9.25, 16, 5.5, 4.5, 1.25), r(15.25, 16, 5.5, 4.5, 1.25), p("M12 8v8M6 16v-2a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v2")],
    trash: [p(TRASH_LID), p(TRASH_HANDLE), p(TRASH_BODY)],
  },

  files: {
    file: file(),
    "file-video": file(p(roundedPoly([[10.25, 11.5], [14.75, 14.25], [10.25, 17]], 0.6))),
    "file-audio": file(p("M12.75 16.75v-6c1.2.15 2.3.9 2.6 2.1"), c(11.25, 16.75, 1.5)),
    "file-image": file(c(10, 12.25, 1.25), p("M8 18l2.6-2.6a1 1 0 0 1 1.4 0l1.4 1.4.6-.6a1 1 0 0 1 1.4 0L17 17.8")),
    "file-archive": file(p("M11 5.5h1M12 7.5h1M11 9.5h1"), r(10.5, 12, 3, 4, 1)),
    "file-document": file(p("M8.5 12.5h7M8.5 16h4.5")),
    "file-pdf": file(p("M10 17.5v-6h2.25a1.75 1.75 0 0 1 0 3.5H10")),
    "file-exe": file(r(8.5, 12, 7, 5.5, 1), p("M8.5 14.25h7")),
    "file-code": file(p("M10.25 12.25L8.5 14.25l1.75 2M13.75 12.25l1.75 2-1.75 2")),
    "file-font": file(p("M9.25 17.5L12 11l2.75 6.5M10.2 15.25h3.6")),
    "disk-image": [RING, c(12, 12, 2.5), p("M12 6.25a5.75 5.75 0 0 1 5.75 5.75")],
    torrent: [c(12, 5.5, 2.25), c(5.5, 17.5, 2.25), c(18.5, 17.5, 2.25), p("M10.45 8.36L7.05 14.64M13.55 8.36l3.4 6.28M8.75 17.5h6.5")],
  },

  status: {
    "status-downloading": [RING, p("M12 7.5v8.5M8.5 12.75L12 16.25l3.5-3.5")],
    "status-paused": [RING, p("M10 9v6M14 9v6")],
    "status-queued": [RING, dot(8, 12, 1.15), dot(12, 12, 1.15), dot(16, 12, 1.15)],
    "status-completed": [RING, p("M8 12.25l2.75 2.75L16 9.5")],
    "status-error": [RING, p("M12 7.5v5.25"), dot(12, 16.1, 1.15)],
    "status-scheduled": [RING, p("M12 7.5V12l3 2")],
    "status-verifying": [c(12, 12, 9, { "stroke-dasharray": "0.85 3.19" }), p("M8 12.25l2.75 2.75L16 9.5")],
    "status-merging": [RING, p("M6 12h4.5M8.25 9.75L10.5 12l-2.25 2.25M18 12h-4.5M15.75 9.75L13.5 12l2.25 2.25")],
    "status-waiting": [RING, p(roundedPoly([[9.25, 7.5], [14.75, 7.5], [12, 12]], 0.6)), p(roundedPoly([[12, 12], [14.75, 16.5], [9.25, 16.5]], 0.6))],
  },

  actions: {
    "folder-open": [p("M3.5 18.5V7A2.5 2.5 0 0 1 6 4.5h3.17a1.5 1.5 0 0 1 1.06.44L12 6.75h5.5A2.5 2.5 0 0 1 20 9.25V11"), p(roundedPoly([[3.5, 20], [6.6, 11], [21.25, 11], [18.15, 20]], 1.6))],
    "open-file": file(p("M9.5 17l4.5-4.5M10.75 12.5H14v3.25")),
    copy: [r(8.5, 8.5, 12, 12, 2.5), p("M15.5 8.5V6A2.5 2.5 0 0 0 13 3.5H6A2.5 2.5 0 0 0 3.5 6v7A2.5 2.5 0 0 0 6 15.5h2.5")],
    link: LINK,
    "refresh-url": [p("M19.5 12a7.5 7.5 0 0 1-13.1 5"), p("M4.5 12a7.5 7.5 0 0 1 13.1-5"), p("M17.6 3.5V7h-3.5M6.4 20.5V17h3.5")],
    retry: [p("M20 12a8 8 0 1 1-2.34-5.66"), p("M20.25 3.75V8H16")],
    move: [p(FOLDER), p("M8.5 13.5h7M12.5 10.5l3 3-3 3")],
    rename: [p("M15.4 4.6a2.1 2.1 0 0 1 3 3L8.5 17.5l-4 1 1-4z"), p("M13.5 6.5l3 3"), p("M13 19.75h7")],
    properties: [r(4, 3.5, 16, 17, 2.5), dot(8.25, 8.5, 1.05), dot(8.25, 12, 1.05), dot(8.25, 15.5, 1.05), p("M11.25 8.5H16M11.25 12H16M11.25 15.5H16")],
    checksum: [p("M10 4L8.5 20M15.5 4L14 20M5 9h14.5M4.5 15H19")],
    shield: [p(SHIELD), p("M9 12.25l2.1 2.1 4-4.1")],
    power: [p("M12 3.5v8"), p("M7.05 6.4A7.85 7.85 0 1 0 16.95 6.4")],
    sleep: [p(crescent(10.5, 13.5, 7, 15.75, 8.25, 5.6)), p("M15 3.5h4l-4 4.5h4")],
    lock: [r(5, 10.5, 14, 10, 2.5), p("M8 10.5V8a4 4 0 0 1 8 0v2.5"), p("M12 14.5v2")],
    "lock-closed": [r(5, 10.5, 14, 10, 2.5), p("M8 10.5V8a4 4 0 0 1 8 0v2.5")],
    "lock-open": [r(5, 10.5, 14, 10, 2.5), p("M8 10.5V8a4 4 0 0 1 7.75-1.4")],
    key: [c(8.25, 15.75, 4.25), p("M11.25 12.75l8.25-8.25M16.5 7.5l2.5 2.5M14.25 9.75l2 2")],
    sound: [p("M4 9.5v5h3.5l4.5 4v-13l-4.5 4z"), p("M15.5 9a4 4 0 0 1 0 6M18 6.5a7.5 7.5 0 0 1 0 11")],
    "sound-off": [p("M4 9.5v5h3.5l4.5 4v-13l-4.5 4z"), p("M15.5 9.5l5 5M20.5 9.5l-5 5")],
    clipboard: [p("M9 4.5H7.5A2.5 2.5 0 0 0 5 7v11.5A2.5 2.5 0 0 0 7.5 21h9a2.5 2.5 0 0 0 2.5-2.5V7a2.5 2.5 0 0 0-2.5-2.5H15"), r(9, 3, 6, 3.5, 1.25), p("M8.75 12h6.5M8.75 15.5h4")],
    browser: [r(3, 4.5, 18, 15, 2.5), p("M3 9h18"), dot(6, 6.75, 0.85), dot(8.5, 6.75, 0.85)],
    puzzle: [p("M6 6h3.75a2.25 2.25 0 0 1 4.5 0h2.25A1.5 1.5 0 0 1 18 7.5v2.25a2.25 2.25 0 0 1 0 4.5V18a1.5 1.5 0 0 1-1.5 1.5H6A1.5 1.5 0 0 1 4.5 18V7.5A1.5 1.5 0 0 1 6 6z")],
    globe: [RING, ["ellipse", { cx: 12, cy: 12, rx: 3.75, ry: 9 }], p("M3 12h18")],
    proxy: [c(5, 12, 2), c(19, 12, 2), r(9.25, 9.25, 5.5, 5.5, 1.5), p("M7 12h2.25M14.75 12H17")],
    "hard-drive": [r(3, 13, 18, 7, 2.5), p("M3.6 13.4l2.3-6.9A2 2 0 0 1 7.8 5.2h8.4a2 2 0 0 1 1.9 1.3l2.3 6.9"), dot(16.75, 16.5, 1.05), p("M6.75 16.5h4.5")],
    activity: [p("M3 12h3.5l2.5-6.5 4.5 13 2.5-6.5H21")],
    bolt: [p(roundedPoly([[13.5, 2.75], [5, 13.5], [11.25, 13.5], [10.5, 21.25], [19, 10.5], [12.75, 10.5]], 0.9))],
    calendar: [r(3.5, 5, 17, 15.5, 2.5), p("M3.5 9.75h17M8 3v4M16 3v4"), dot(8, 13.5, 1), dot(12, 13.5, 1), dot(16, 13.5, 1), dot(8, 17, 1), dot(12, 17, 1)],
    clock: [RING, p("M12 7v5l3.5 2")],
    history: [p("M3.5 12a8.5 8.5 0 1 0 2.5-6"), p("M6 2.75V6H2.75"), p("M12 8v4.5l3 1.75")],
    eye: [p("M2.75 12C4.6 8 8 5.75 12 5.75S19.4 8 21.25 12C19.4 16 16 18.25 12 18.25S4.6 16 2.75 12z"), c(12, 12, 3)],
    user: [c(12, 8, 4), p("M4.5 20.5a7.5 7.5 0 0 1 15 0")],
    bell: [p("M6 10.5a6 6 0 0 1 12 0v4.25l1.75 2.75H4.25L6 14.75z"), p("M10 20.5h4")],
    tag: [p("M3.5 12.1V5.5a2 2 0 0 1 2-2h6.6a2 2 0 0 1 1.4.6l7.4 7.4a2 2 0 0 1 0 2.8l-6.6 6.6a2 2 0 0 1-2.8 0l-7.4-7.4a2 2 0 0 1-.6-1.4z"), dot(8, 8, 1.3)],
    star: [p(roundedPoly([[12, 3.25], [14.6, 8.7], [20.5, 9.4], [16.2, 13.5], [17.3, 19.4], [12, 16.5], [6.7, 19.4], [7.8, 13.5], [3.5, 9.4], [9.4, 8.7]], 0.8))],
    list: [dot(5, 6.5, 1.15), dot(5, 12, 1.15), dot(5, 17.5, 1.15), p("M9.5 6.5h10M9.5 12h10M9.5 17.5h10")],
    grid: [r(4, 4, 6.75, 6.75, 2), r(13.25, 4, 6.75, 6.75, 2), r(4, 13.25, 6.75, 6.75, 2), r(13.25, 13.25, 6.75, 6.75, 2)],
    sidebar: [r(3, 4.5, 18, 15, 2.5), p("M9.5 4.5v15")],
    "x-circle": [RING, p("M9 9l6 6M15 9l-6 6")],
    prism: [p(roundedPoly([[10, 4.25], [16.25, 18.5], [3.75, 18.5]], 1.3)), p("M1.75 14L6 12.6"), p("M14.6 11.4L21.5 8.5M15.3 13.3H21.5M16 15.2l5.5 2.8")],
    // Tools: one format into another (two opposing arrows) …
    convert: [p("M4.5 8.25h14.5M15.25 4.5L19 8.25l-3.75 3.75"), p("M19.5 15.75H5M8.75 12L5 15.75l3.75 3.75")],
    // … and a file made smaller (corners pulled in).
    shrink: [p("M4 4l5.75 5.75M9.75 4.75v5H4.75"), p("M20 20l-5.75-5.75M14.25 19.25v-5h5")],
  },

  window: {
    "chevron-down": [p("M6.5 9.25l5.5 5.5 5.5-5.5")],
    "chevron-up": [p("M6.5 14.75l5.5-5.5 5.5 5.5")],
    "chevron-left": [p("M14.75 6.5L9.25 12l5.5 5.5")],
    "chevron-right": [p("M9.25 6.5l5.5 5.5-5.5 5.5")],
    "chevrons-up-down": [p("M8 9.5l4-4 4 4M8 14.5l4 4 4-4")],
    "chevrons-right": [p("M6 6.5l5.5 5.5L6 17.5M12.5 6.5L18 12l-5.5 5.5")],
    "arrow-down": [p("M12 4.5v15M6 13.5l6 6 6-6")],
    "arrow-up": [p("M12 19.5v-15M6 10.5l6-6 6 6")],
    "arrow-left": [p("M19.5 12h-15M10.5 6l-6 6 6 6")],
    "arrow-right": [p("M4.5 12h15M13.5 6l6 6-6 6")],
    close: [p("M6.5 6.5l11 11M17.5 6.5l-11 11")],
    minimize: [p("M6.5 12h11")],
    maximize: [r(5.5, 5.5, 13, 13, 2)],
    restore: [r(5, 8, 11, 11, 2), p("M8 5h8.5A2.5 2.5 0 0 1 19 7.5V16")],
    pin: [p("M9 3.5h6M10 3.5v6L7 13h10l-3-3.5v-6M12 13v7.5")],
    "mini-mode": [r(3, 5, 18, 14, 2.5), r(11.5, 11, 6.5, 5, 1)],
    sun: [c(12, 12, 3.75), p(rays(12, 12, 6.6, 8.75, 8, -90))],
    moon: [p(crescent(11.5, 12.5, 8.25, 17.25, 6.75, 6.75))],
    monitor: [r(3, 4, 18, 12.5, 2.5), p("M8.5 20h7M12 16.5V20")],
    check: [p("M5 12.5l4.5 4.5L19 7.5")],
    plus: [p("M12 5v14M5 12h14")],
    minus: [p("M5 12h14")],
    info: [RING, p("M12 11v5.5"), dot(12, 7.9, 1.15)],
    warning: [p(roundedPoly([[12, 3.5], [21, 19.5], [3, 19.5]], 2)), p("M12 9.5v4.25"), dot(12, 16.6, 1.1)],
    play: [RING, p(roundedPoly([[10.4, 8.5], [16.1, 12], [10.4, 15.5]], 0.9))],
    external: [p("M14 4.5h5.5V10M19.5 4.5L11 13"), p("M18 14v3.5a2.5 2.5 0 0 1-2.5 2.5h-9A2.5 2.5 0 0 1 4 17.5v-9A2.5 2.5 0 0 1 6.5 6H10")],
    "drag-handle": [dot(9, 6, 1.3), dot(15, 6, 1.3), dot(9, 12, 1.3), dot(15, 12, 1.3), dot(9, 18, 1.3), dot(15, 18, 1.3)],
    "sort-asc": [p("M7 19.5v-15M3.5 8L7 4.5 10.5 8"), p("M13.5 6.5h3M13.5 11.5h5M13.5 16.5h7")],
    "sort-desc": [p("M7 4.5v15M3.5 16L7 19.5l3.5-3.5"), p("M13.5 6.5h7M13.5 11.5h5M13.5 16.5h3")],
  },

  media: {
    magnet: [p("M5 4.5h4.5V12a2.5 2.5 0 0 0 5 0V4.5H19V12a7 7 0 0 1-14 0z"), p("M5 8.5h4.5M14.5 8.5H19")],
    subtitle: [r(3, 5, 18, 14, 2.5), p("M7 13.25h4M14 13.25h3M7 16h7M16.75 16h.25")],
    hdr: [r(3, 5, 18, 14, 2.5), c(12, 12, 4), fill("M12 8a4 4 0 0 1 0 8z")],
    "music-note": [p("M10 17.5V4.5c2.5.3 5.5 1.9 6.5 4.75"), c(7.5, 17.5, 2.5)],
    playlist: [p("M4 6h11M4 11h11M4 16h6"), p("M17.5 17.25V9.5l3 1"), c(15.5, 17.25, 2)],
    thumbnail: [r(3, 5, 18, 14, 2.5), r(6, 8, 7, 5.5, 1), p("M15.5 9h2.5M15.5 12h2.5M6 16h12")],
    "picture-in-picture": [r(3, 5, 18, 14, 2.5), r(11.5, 11, 6.5, 5, 1)],
  },
};

export const icons = Object.assign({}, ...Object.values(groups));
