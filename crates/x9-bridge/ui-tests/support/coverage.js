// JS coverage of the UI's own modules, from Chromium's V8 block coverage.
//
// Each page saves what it ran (`save`); after the run `summarize` merges
// every page of every test: a byte counts as run if any page ran it, a
// function as entered if any page called it. No coverage library: V8's
// ranges nest (a function's first range is the function, later ones its
// blocks), so applying them outermost first, inner ones overwriting, gives
// each byte the count of the innermost range around it.

import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
export const OUT_DIR = path.resolve(here, "../coverage");
export const RAW_DIR = path.join(OUT_DIR, "raw");
const FILES = ["app.js", "viewer.js"];

const lineAt = (src, offset) => src.slice(0, offset).split("\n").length;

/** One page's V8 entries → the UI files' run byte ranges and functions. */
function reduce(entry) {
  const ranges = entry.functions.flatMap((f) => f.ranges)
    .sort((a, b) => a.startOffset - b.startOffset || b.endOffset - a.endOffset);
  const run = new Uint8Array(entry.source.length);
  for (const r of ranges) run.fill(r.count > 0 ? 1 : 0, r.startOffset, r.endOffset);
  const spans = [];
  for (let i = 0; i < run.length;) {
    if (!run[i]) { i++; continue; }
    const s = i;
    while (i < run.length && run[i]) i++;
    spans.push([s, i]);
  }
  const functions = entry.functions.map((f) => {
    const r = f.ranges[0];
    return { key: `${r.startOffset}-${r.endOffset}`, name: f.functionName || "(anonymous)", line: lineAt(entry.source, r.startOffset), entered: r.count > 0 };
  });
  return { length: entry.source.length, sha: crypto.createHash("sha1").update(entry.source).digest("hex"), spans, functions };
}

/** Save a page's coverage (the entries from `page.coverage.stopJSCoverage()`). */
export function save(entries, label) {
  const out = {};
  for (const e of entries) {
    let file;
    try { file = path.posix.basename(new URL(e.url).pathname); } catch { continue; }
    if (!FILES.includes(file) || !new URL(e.url).pathname.startsWith("/ui/")) continue;
    // A page that reloads loads the module again: one entry per load.
    (out[file] ??= []).push(reduce(e));
  }
  if (!Object.keys(out).length) return;
  fs.mkdirSync(RAW_DIR, { recursive: true });
  const name = `${label.replace(/[^\w.-]+/g, "_").slice(0, 80)}-${crypto.randomUUID().slice(0, 8)}.json`;
  fs.writeFileSync(path.join(RAW_DIR, name), JSON.stringify(out));
}

/** Merge everything saved, print the table, write coverage/ui-coverage.json. */
export function summarize() {
  if (!fs.existsSync(RAW_DIR)) return;
  const merged = {};
  for (const f of fs.readdirSync(RAW_DIR)) {
    const one = JSON.parse(fs.readFileSync(path.join(RAW_DIR, f), "utf8"));
    for (const [file, loads] of Object.entries(one)) {
      const m = merged[file] ??= { length: loads[0].length, sha: loads[0].sha, run: new Uint8Array(loads[0].length), functions: new Map(), pages: 0, loads: 0 };
      m.pages++;
      for (const c of loads) {
        if (m.sha !== c.sha) throw new Error(`${file} changed during the run`);
        m.loads++;
        for (const [s, e] of c.spans) m.run.fill(1, s, e);
        for (const fn of c.functions) {
          const had = m.functions.get(fn.key);
          m.functions.set(fn.key, { ...fn, entered: fn.entered || !!had?.entered });
        }
      }
    }
  }
  const report = { generated_at: new Date().toISOString(), files: {} };
  const rows = [];
  for (const file of FILES) {
    const m = merged[file];
    if (!m) { rows.push([file, "–", "–", "not loaded"]); continue; }
    const bytesRun = m.run.reduce((n, b) => n + b, 0);
    const fns = [...m.functions.values()];
    const entered = fns.filter((f) => f.entered);
    const pct = (a, b) => (b ? (100 * a) / b : 0);
    report.files[file] = {
      bytes: { run: bytesRun, total: m.length, pct: +pct(bytesRun, m.length).toFixed(1) },
      functions: { entered: entered.length, total: fns.length, pct: +pct(entered.length, fns.length).toFixed(1) },
      pages: m.pages,
      loads: m.loads,
      // Named functions never called, by line: where to look for gaps.
      not_entered: fns.filter((f) => !f.entered).sort((a, b) => a.line - b.line).map((f) => `${f.name}:${f.line}`),
    };
    rows.push([file, `${pct(bytesRun, m.length).toFixed(1)}%  (${bytesRun}/${m.length})`, `${pct(entered.length, fns.length).toFixed(1)}%  (${entered.length}/${fns.length})`, String(m.pages)]);
  }
  fs.mkdirSync(OUT_DIR, { recursive: true });
  fs.writeFileSync(path.join(OUT_DIR, "ui-coverage.json"), JSON.stringify(report, null, 2) + "\n");
  const head = ["file", "bytes run", "functions entered", "pages"];
  const w = head.map((h, i) => Math.max(h.length, ...rows.map((r) => r[i].length)));
  const line = (r) => "  " + r.map((c, i) => c.padEnd(w[i])).join("   ");
  console.log(`\nUI JS coverage (merged over every test)\n${line(head)}\n  ${w.map((n) => "-".repeat(n)).join("   ")}\n${rows.map(line).join("\n")}\n  written to ${path.relative(process.cwd(), path.join(OUT_DIR, "ui-coverage.json"))}\n`);
}
