// Generate app-mobile/apps/com.mochi.moymoy/icon.png (512px) from the design's
// EmeGem (emerald-cut gem) on the MoyMoy accent background. Pure Node (zlib only)
// — no native image deps. Run: node tools/make-icon.mjs
import { deflateSync } from "node:zlib";
import { writeFileSync } from "node:fs";

const S = 512;
const buf = Buffer.alloc(S * S * 4); // RGBA

function setPx(x, y, [r, g, b, a = 255]) {
  if (x < 0 || y < 0 || x >= S || y >= S) return;
  const i = (y * S + x) * 4;
  // simple source-over onto existing
  const ia = a / 255, na = 1 - ia;
  buf[i] = Math.round(r * ia + buf[i] * na);
  buf[i + 1] = Math.round(g * ia + buf[i + 1] * na);
  buf[i + 2] = Math.round(b * ia + buf[i + 2] * na);
  buf[i + 3] = Math.max(buf[i + 3], a);
}

function fillRect(x0, y0, x1, y1, color) {
  for (let y = y0; y < y1; y++) for (let x = x0; x < x1; x++) setPx(x, y, color);
}

// Scanline polygon fill (even-odd). pts: [[x,y],...] in pixel space.
function fillPoly(pts, color) {
  let minY = Infinity, maxY = -Infinity;
  for (const [, y] of pts) { minY = Math.min(minY, y); maxY = Math.max(maxY, y); }
  minY = Math.max(0, Math.floor(minY));
  maxY = Math.min(S - 1, Math.ceil(maxY));
  for (let y = minY; y <= maxY; y++) {
    const yc = y + 0.5;
    const xs = [];
    for (let i = 0; i < pts.length; i++) {
      const [x1, y1] = pts[i];
      const [x2, y2] = pts[(i + 1) % pts.length];
      if ((y1 <= yc && y2 > yc) || (y2 <= yc && y1 > yc)) {
        xs.push(x1 + ((yc - y1) / (y2 - y1)) * (x2 - x1));
      }
    }
    xs.sort((a, b) => a - b);
    for (let k = 0; k + 1 < xs.length; k += 2) {
      const xa = Math.round(xs[k]), xb = Math.round(xs[k + 1]);
      for (let x = xa; x < xb; x++) setPx(x, y, color);
    }
  }
}

const hex = (h) => [parseInt(h.slice(1, 3), 16), parseInt(h.slice(3, 5), 16), parseInt(h.slice(5, 7), 16), 255];

// Background — MoyMoy accent, with a darker band at the bottom for depth.
fillRect(0, 0, S, S, hex("#16A35A"));
fillRect(0, Math.round(S * 0.62), S, S, hex("#0E8A47"));

// Gem geometry (EmeGem 0..32 viewBox) → centered, scale to ~64% of canvas.
const scale = (S * 0.62) / 32;
const ox = (S - 32 * scale) / 2;
const oy = (S - 32 * scale) / 2;
const P = (pts) => pts.map(([x, y]) => [ox + x * scale, oy + y * scale]);

const polys = [
  [[11,2],[21,2],[30,11],[30,21],[21,30],[11,30],[2,21],[2,11], "#0B7A41"],
  [[11.5,3],[20.5,3],[29,11.5],[29,20.5],[20.5,29],[11.5,29],[3,20.5],[3,11.5], "#1B9E54"],
  [[11.5,3],[20.5,3],[24.5,9],[7.5,9], "#3FD981"],
  [[11.5,29],[20.5,29],[24.5,23],[7.5,23], "#0E8A47"],
  [[3,11.5],[7.5,9],[7.5,23],[3,20.5], "#16A35A"],
  [[29,11.5],[24.5,9],[24.5,23],[29,20.5], "#127D43"],
  [[7.5,9],[24.5,9],[24.5,23],[7.5,23], "#2ECC71"],
  [[7.5,9],[24.5,9],[16,16], "#5CEB95"],
  [[7.5,9],[16,16],[7.5,23], "#3FD981"],
  [[11,11],[14.5,11],[13,13.5], "#D6FFE8"],
];
for (const poly of polys) {
  const color = poly[poly.length - 1];
  const pts = poly.slice(0, -1);
  fillPoly(P(pts), hex(color));
}

// ── PNG encode ──
function crc32(buf) {
  let c = ~0;
  for (let i = 0; i < buf.length; i++) {
    c ^= buf[i];
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return (~c) >>> 0;
}
function chunk(type, data) {
  const len = Buffer.alloc(4); len.writeUInt32BE(data.length, 0);
  const t = Buffer.from(type, "latin1");
  const body = Buffer.concat([t, data]);
  const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}
const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(S, 0); ihdr.writeUInt32BE(S, 4);
ihdr[8] = 8; ihdr[9] = 6; ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0; // 8-bit RGBA
// filtered scanlines (filter 0)
const raw = Buffer.alloc((S * 4 + 1) * S);
for (let y = 0; y < S; y++) {
  raw[y * (S * 4 + 1)] = 0;
  buf.copy(raw, y * (S * 4 + 1) + 1, y * S * 4, (y + 1) * S * 4);
}
const png = Buffer.concat([
  Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
  chunk("IHDR", ihdr),
  chunk("IDAT", deflateSync(raw, { level: 9 })),
  chunk("IEND", Buffer.alloc(0)),
]);
const out = new URL("../app-mobile/apps/com.mochi.moymoy/icon.png", import.meta.url);
writeFileSync(out, png);
console.log("wrote", out.pathname, png.length, "bytes");
