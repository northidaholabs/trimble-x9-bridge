// X9 Bridge web UI. Plain modules, no build step, no network beyond the bridge:
// the Pi has no internet in the field. Every API call sends the token; the
// event stream is read with fetch so it can send it too.

import { createViewer } from "./viewer.js";

const PHASES = {
  waiting: "Waiting for the scanner to clear the last task",
  leveling: "Levelling",
  sweeping: "Scanning",
  imaging: "Taking pictures",
  color: "Taking pictures",
  downloading: "Downloading from the scanner",
  decoding: "Decoding points",
  writing: "Writing the file",
  measuring: "Measuring the opening",
};

// ---------------------------------------------------------------- storage ---
const store = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { v == null ? localStorage.removeItem(k) : localStorage.setItem(k, v); } catch { /* private mode */ } },
};

// ------------------------------------------------------------------ state ---
const S = {
  token: store.get("x9.token"),
  me: null,
  live: false,
  status: null,
  jobs: [],
  runs: [],
  recipes: [],
  recipe: {},      // recipe_id -> full recipe
  projects: [],
  modes: [],
  log: [],
  scans: [],
  scanById: {},    // every scan seen, so a link to a scan in another project still opens
  project: store.get("x9.project"),
  previews: new Map(), // scan_id -> object URL | "loading" | {error}
  dims: {},        // scan_id -> [w, h]
  boxes: {},       // picker key -> {x0, y0, x1, y1}
  drawMode: {},    // picker key -> bool
  boxFrom: {},     // picker key -> proposal time the box was loaded from
  userDrew: {},    // picker key -> true once a person drew their own box
  boxShown: {},    // picker key -> which box was last scrolled into view
  aim: {},         // scan_id -> aim result
  exports: {},     // export_id -> export record
  measurements: {}, // measurement_id -> measurement record
  grants: [],      // active grants
  nextStations: [], // the agent's next-station proposals, newest first
  missions: [],     // each project's mission, newest first
  nsImages: new Map(), // scan_id|at -> object URL | "loading" | {error}
  regs: {},            // project_id -> registration (null while loading)
  mapImages: new Map(), // project|registration|stations|ledger -> object URL | "loading" | {error}
  mapLast: {},          // project -> the last minimap drawn, shown while the next one loads
  evidence: new Map(), // measurement_id -> object URL | "loading" | {error}
  askImages: new Map(), // project|target|checked_at -> object URL | "loading" | {error}
  asking: {},           // project|target -> true while an ask runs
  plans: {},            // scan_id -> the bridge's next-station plan | "loading" | {error}
  workers: [],          // laptops doing the heavy work, seen in the last 15 s
  spotScrolled: {},     // spots picture -> true once scrolled to spot 1
  overlays: {},         // scan|plan|registration|mission -> projected overlay | "loading" | {error}
  spotFocus: null,      // the spot picked out on the picture
  // Calm by default: the picture shows the next spot only; "Show more" adds
  // the other spots, earlier stations, the faces asked about and distances.
  showMore: store.get("x9.more") === "1",
  otherSpots: false,    // the other spots' list unfolded
  qOpen: {},            // project|target -> a question's detail unfolded
  watchMeasure: {}, // scan_id -> job_id
  watchExport: {}, // scan_id -> job_id
  sheet: null,
  toast: null,
  busy: false,
  route: { tab: "now" },
  signinError: "",
};

// ------------------------------------------------------------------ utils ---
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const deg = (rad) => `${(rad * 180 / Math.PI).toFixed(1)}°`;
const isOp = () => S.me && (S.me.role === "operator" || S.me.role === "admin");
const canAct = () => S.me && S.me.role !== "viewer";

function clock(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  return isNaN(d) ? "" : d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}
function ago(iso) {
  if (!iso) return "";
  const s = Math.max(0, (Date.now() - new Date(iso)) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return new Date(iso).toLocaleDateString();
}
function elapsed(iso) {
  if (!iso) return "";
  const s = Math.max(0, Math.round((Date.now() - new Date(iso)) / 1000));
  return s < 60 ? `${s} s` : `${Math.floor(s / 60)} min ${s % 60} s`;
}
const span = (b, w) => (b.x1 >= b.x0 ? b.x1 - b.x0 : b.x1 + w - b.x0);
const gb = (b) => (b == null ? "–" : `${(b / 1e9).toFixed(1)} GB`);
const num = (n) => (n == null ? "–" : Number(n).toLocaleString());

function stateChip(state) {
  const cls = { succeeded: "good", done: "good", failed: "bad", cancelled: "", awaiting_approval: "warn", waiting_for_person: "warn", running: "info", queued: "info" }[state] ?? "";
  const label = { awaiting_approval: "needs approval", waiting_for_person: "needs you" }[state] ?? state;
  return `<span class="chip ${cls}">${esc(label)}</span>`;
}

function describeJob(j) {
  if (j.kind === "measure") return "Opening measurement";
  if (j.kind === "register") return "Registering stations";
  if (j.kind === "export") return `Export${j.result?.export_id && S.exports[j.result.export_id] ? " " + S.exports[j.result.export_id].format?.toUpperCase() : ""}`;
  const r = j.request;
  if (!r) return "Scan";
  const what = r.kind === "area" ? "Area scan" : "Full-dome scan";
  const extra = [r.mode, r.color ? "colour" : null].filter(Boolean).join(" · ");
  return `${what} · ${extra}`;
}
function jobDetail(j) {
  const r = j.request;
  const bits = [];
  if (j.project_name) bits.push(j.project_name);
  if (r?.window) bits.push(`window ${deg(r.window.delta_ha)} × ${deg(r.window.delta_va)}`);
  if (r?.note) bits.push(`“${r.note}”`);
  return bits.join(" · ");
}

function problemText(p) {
  if (!p) return "";
  if (typeof p === "string") return p;
  return [p.title, p.detail].filter(Boolean).join(": ") + (p.hint ? ` (${p.hint})` : "");
}

// -------------------------------------------------------------------- api ---
class ApiError extends Error {
  constructor(problem, status) {
    super(problemText(problem) || `HTTP ${status}`);
    this.problem = problem;
    this.status = status;
  }
}

async function api(path, { method = "GET", body, headers = {}, raw = false } = {}) {
  const h = { Authorization: `Bearer ${S.token}`, ...headers };
  if (body !== undefined) h["Content-Type"] = "application/json";
  const r = await fetch("/v1" + path, { method, headers: h, body: body === undefined ? undefined : JSON.stringify(body) });
  if (r.status === 401) {
    signOut("That token was not accepted.");
    throw new ApiError({ title: "Not signed in" }, 401);
  }
  if (!r.ok) {
    let p = null;
    try { p = await r.json(); } catch { /* not a problem document */ }
    throw new ApiError(p, r.status);
  }
  if (raw) return r;
  if (r.status === 204) return null;
  return (r.headers.get("content-type") || "").includes("json") ? r.json() : r.text();
}

async function act(fn, done) {
  if (S.busy) return;
  S.busy = true;
  render(true);
  try {
    const out = await fn();
    if (done) toast(done);
    return out;
  } catch (e) {
    if (e.status !== 401) toast(e.message, true);
  } finally {
    S.busy = false;
    render(true);
  }
}

// A line in the field log for the agent (and the record), shown at once.
async function logNote(text, projectId) {
  const e = await api("/log", { method: "POST", body: { text, project_id: projectId } });
  if (e && !S.log.some((x) => x.entry_id === e.entry_id)) S.log.unshift(e);
  return e;
}

function toast(text, bad = false) {
  S.toast = { text, bad };
  render(true);
  clearTimeout(toast.t);
  toast.t = setTimeout(() => { S.toast = null; render(true); }, bad ? 6000 : 2800);
}

// ---------------------------------------------------------------- loading ---
const upsert = (list, item, key) => {
  const i = list.findIndex((x) => x[key] === item[key]);
  if (i >= 0) list[i] = item; else list.unshift(item);
};
const byNewest = (a, b) => String(b.created_at).localeCompare(String(a.created_at));
// A run only moves forward. A reply to a request can arrive after the event
// stream has already delivered a newer state (a sign-off that finishes the run
// at once), so never let an older copy replace a newer one.
const runProgress = (r) => (["succeeded", "failed", "cancelled"].includes(r.state) ? 1e6 : 0)
  + 10 * (r.steps || []).filter((s) => s.state === "done").length + (r.interventions?.total ?? 0);
function upsertRun(r) {
  const e = S.runs.find((x) => x.run_id === r.run_id);
  if (e && runProgress(r) < runProgress(e)) return;
  upsert(S.runs, r, "run_id");
}

async function load(what) {
  try {
    switch (what) {
      case "status": S.status = await api("/status"); break;
      case "jobs": S.jobs = (await api("/jobs")).sort(byNewest); break;
      case "runs": S.runs = (await api("/runs")).sort(byNewest); break;
      case "recipes": S.recipes = await api("/recipes"); break;
      case "projects":
        S.projects = await api("/projects");
        if (!S.project || !S.projects.some((p) => p.project_id === S.project)) {
          const latest = S.jobs.find((j) => j.request?.project_id && S.projects.some((p) => p.project_id === j.request.project_id));
          if (latest) { S.project = latest.request.project_id; store.set("x9.project", S.project); load("scans"); }
        }
        break;
      case "modes": S.modes = await api("/scan-modes"); break;
      case "log": S.log = (await api("/log")).reverse(); break;
      case "grants": S.grants = await api("/grants"); break;
      case "next-stations": S.nextStations = await api("/next-stations"); break;
      case "missions": S.missions = await api("/missions"); break;
      case "workers": S.workers = await api("/workers"); break;
      case "scans":
        S.scans = S.project ? await api(`/scans?project_id=${encodeURIComponent(S.project)}`) : [];
        for (const sc of S.scans) S.scanById[sc.scan_id] = sc;
        break;
    }
  } catch (e) {
    if (e.status !== 401 && what !== "status") console.warn(what, e.message);
    if (what === "status") S.status = { error: e.message };
  }
  render();
}

async function recipe(id) {
  if (!S.recipe[id]) {
    S.recipe[id] = "loading";
    try { S.recipe[id] = await api(`/recipes/${encodeURIComponent(id)}`); } catch (e) { S.recipe[id] = { error: e.message }; }
    render();
  }
  return typeof S.recipe[id] === "object" ? S.recipe[id] : null;
}

function preview(scanId) {
  const p = S.previews.get(scanId);
  if (p) return p;
  S.previews.set(scanId, "loading");
  api(`/scans/${encodeURIComponent(scanId)}/preview`, { raw: true })
    .then((r) => r.blob())
    .then((b) => S.previews.set(scanId, URL.createObjectURL(b)))
    .catch((e) => S.previews.set(scanId, { error: e.message }))
    .finally(() => render());
  return "loading";
}

async function loadMeasurement(id) {
  try { S.measurements[id] = await api(`/measurements/${encodeURIComponent(id)}`); } catch (e) { S.measurements[id] = { error: e.message }; }
  render();
}

function nsImage(p) {
  const key = `${p.scan_id}|${p.at}`;
  const got = S.nsImages.get(key);
  if (got) return got;
  S.nsImages.set(key, "loading");
  api(`/scans/${encodeURIComponent(p.scan_id)}/next-station/image`, { raw: true })
    .then((r) => r.blob())
    .then((b) => S.nsImages.set(key, URL.createObjectURL(b)))
    .catch((e) => S.nsImages.set(key, { error: e.message }))
    .finally(() => render());
  return "loading";
}

// What the job is for, the agent's ledger, and the one sign-off before leaving.
const TARGET_CHIP = { open: ["warn", "open"], covered: ["good", "covered"], measured: ["good", "measured"], declined: ["", "declined"] };
function missionCard(m) {
  const state = { active: ["info", "in progress"], ready: ["warn", "ready to leave"], signed_off: ["good", "signed off"] }[m.state] ?? ["", m.state];
  const rows = m.targets.map((t) => {
    if (t.question) return questionRow(m, t);
    const [cls, word] = TARGET_CHIP[t.status] ?? ["", t.status];
    return `<li><span class="chip ${cls}">${esc(word)}</span><div class="grow"><strong>${esc(t.name)}</strong>
      <div class="meta">${esc(t.kind)}${t.stations.length ? ` · ${t.stations.length} station${t.stations.length === 1 ? "" : "s"}` : ""}${t.geometry?.seen_share !== undefined ? ` · ${Math.round(t.geometry.seen_share * 100)}% of its length seen` : ""}${t.note ? ` · ${esc(t.note)}` : ""}</div></div></li>`;
  }).join("");
  const n = (s) => m.targets.filter((t) => t.status === s).length;
  const tally = ["covered", "measured", "declined", "open"].filter((s) => n(s)).map((s) => `${n(s)} ${s}`).join(" · ") || "no targets yet";
  return `<div class="card mission ${m.state === "ready" ? "attn" : ""}">
    <div class="line top-align"><strong class="grow">${esc(m.goal)}</strong><span class="chip ${state[0]}">${esc(state[1])}</span></div>
    <div class="meta">Done when: ${esc(m.done_when)}</div>
    <div class="line"><span class="meta">${esc(tally)}</span><span class="ask-chat quiet">${CHAT_ICON}Ask in the chat for more</span></div>
    ${rows ? `<ul class="ledger">${rows}</ul>` : ""}
    ${m.state === "ready" ? `<div class="note">${esc(m.ready_summary ?? "")}</div>
      ${isOp() ? `<button class="btn good wide" data-act="mission-sign-off" data-id="${esc(m.project_id)}" ${S.busy ? "disabled" : ""}>Sign off the job</button>` : `<p class="hint">Waiting for an operator to sign it off.</p>`}` : ""}
    ${m.state === "signed_off" ? `<div class="meta">Signed off by ${esc(m.signed_off_by)} · ${esc(ago(m.signed_off_at))}${m.confirmed_via ? ` · ${esc(m.confirmed_via)}` : ""}</div>` : ""}
  </div>`;
}

// ------------------------------------------------------------- questions ---
// A question target's row: the bridge's verdict, in its words. Figures only
// with an answer, and only as the bridge said them (`said`): the page does
// no arithmetic of its own, so it and the agent never disagree.
const QWORD = { answer: ["good", "answered"], not_yet: ["warn", "not yet"], does_not_fit: ["bad", "doesn't fit"] };
function reasonText(q) {
  if (q.reason === "settling") return "Passed once. One more station that sees this face confirms it.";
  const r = (q.reason || "").trim();
  return r ? r[0].toUpperCase() + r.slice(1).replace(/\.?$/, ".") : "";
}
function askImage(pid, tid, at) {
  const key = `${pid}|${tid}|${at}`;
  const got = S.askImages.get(key);
  if (got) return got;
  S.askImages.set(key, "loading");
  api(`/projects/${encodeURIComponent(pid)}/mission/targets/${encodeURIComponent(tid)}/ask/image`, { raw: true })
    .then((r) => r.blob())
    .then((b) => S.askImages.set(key, URL.createObjectURL(b)))
    .catch((e) => S.askImages.set(key, { error: e.message }))
    .finally(() => render());
  return "loading";
}
// One line per question; tap it for the full wording, the heat map and Ask again.
function questionLine(t) {
  const q = t.question;
  if (q.verdict === "answer" && q.said) {
    const call = t.tolerance ? (q.said.within === true ? " · within ✓" : q.said.within === false ? " · outside ✗" : "") : "";
    return `${Math.abs(q.said.out_of_plumb_mm).toFixed(1)} mm out of plumb${call}`;
  }
  if (q.verdict === "not_yet") return q.reason === "settling" ? "needs one more look" : (q.reason || "not yet");
  if (q.verdict === "does_not_fit") return q.reason || "doesn't fit";
  return "not asked yet";
}
function questionRow(m, t) {
  const q = t.question;
  const [cls, word] = q.verdict ? QWORD[q.verdict] ?? ["", q.verdict] : ["", "not asked"];
  const key = `${m.project_id}|${t.target_id}`;
  const open = !!S.qOpen[key];
  const head = `<button class="q-line" data-act="q-open" data-key="${esc(key)}" aria-expanded="${open}"><strong>${esc(t.name.replace(/ \(.*\)$/, ""))}</strong> · ${esc(questionLine(t))}<span class="caret">${open ? "▾" : "▸"}</span></button>`;
  if (!open) return `<li class="question"><span class="chip ${cls}">${esc(word)}</span><div class="grow">${head}</div></li>`;
  let body = "";
  if (q.verdict === "answer" && q.said) {
    const a = q.said;
    const over = a.over_m != null ? ` over ${a.over_m.toFixed(1)} m` : "";
    const inches = a.out_of_plumb_in === "0 in" ? "under 1/16 in" : a.out_of_plumb_in;
    body += `<div class="answer">${esc(Math.abs(a.out_of_plumb_mm).toFixed(1))} mm out of plumb (${esc(inches)})${esc(over)}</div>`;
    if (a.leans) body += `<div class="meta">The top leans ${esc(a.leans)}.</div>`;
    if (t.tolerance) {
      const call = a.within === true ? `<span class="chip good">within</span> ${esc(a.allowed_in)} allowed`
        : a.within === false ? `<span class="chip bad">outside</span> ${esc(a.allowed_in)} allowed`
        : esc(a.tolerance_note ?? "");
      body += `<div class="meta tol">${call}<br>${esc(t.tolerance.words)}</div>`;
    } else {
      body += `<div class="meta">No tolerance set, so no call on within or outside.</div>`;
    }
    body += `<div class="meta">Flat to ${esc(a.flat_rms_mm.toFixed(0))} mm RMS · stations ${esc(a.stations.join(", "))}</div>`;
  } else if (q.verdict) {
    const seen = q.verdict === "not_yet" && q.seen_share != null && q.seen_share < 1 ? ` ${Math.round(q.seen_share * 100)}% of the face seen so far.` : "";
    body += `<div class="meta">${esc(reasonText(q))}${esc(seen)}</div>`;
    if (t.tolerance) body += `<div class="meta">Tolerance: ${esc(t.tolerance.words)}</div>`;
  } else {
    body += `<div class="meta">Not asked yet.</div>`;
  }
  // The evidence: the face's heat map, kept from the last ask.
  let pic = "";
  if (q.asked && (q.verdict === "answer" || q.verdict === "does_not_fit")) {
    const img = askImage(m.project_id, t.target_id, q.checked_at);
    if (typeof img === "string" && img !== "loading") {
      pic = `<a href="${esc(img)}" target="_blank" rel="noopener" title="Open full size"><img class="heat" src="${esc(img)}" alt="${q.verdict === "answer" ? "Heat map of the face: how far each part stands proud of or sits back from the fitted plane" : "Where the face stands proud or sits back; no figures, as it doesn't fit one plane"}"></a>`;
    }
  }
  const ask = canAct() && m.state !== "signed_off"
    ? `<button class="btn small" data-act="ask" data-pid="${esc(m.project_id)}" data-id="${esc(t.target_id)}" ${S.asking[key] ? "disabled" : ""}>${S.asking[key] ? "Asking… (up to a minute and a half)" : q.verdict ? "Ask again" : "Ask now"}</button>` : "";
  return `<li class="question open"><span class="chip ${cls}">${esc(word)}</span><div class="grow">${head}
    ${body}${pic}${ask ? `<div class="btns-row">${ask}</div>` : ""}</div></li>`;
}

// ------------------------------------------------------------ next spots ---
// The bridge's own plan from the latest full dome: up to three spots a
// person can walk to, each with a landmark. The person picks one or says why
// not; the answer goes into the field log, where the agent reads it.
const SPOT_LOG = /^Next station from station (\d+): (setting up at|can't stand at) spot (\d)/;
function spotDecisions(pid) {
  const out = {};
  for (const e of [...S.log].reverse()) {
    if (e.project_id !== pid) continue;
    const m = SPOT_LOG.exec(e.text || "");
    if (m) out[`${m[1]}|${m[3]}`] = { use: m[2] === "setting up at", text: e.text, by: e.author, at: e.at };
  }
  return out;
}
function latestDome(pid) {
  const r = S.regs[pid];
  const placed = new Set((r?.stations ?? []).filter((s) => s.placed).map((s) => s.scan_id));
  return S.scans.filter((s) => s.project_id === pid && s.kind === "full_dome" && placed.has(s.scan_id))
    .sort((a, b) => (b.station ?? 0) - (a.station ?? 0))[0];
}
function loadPlan(scanId) {
  if (S.plans[scanId]) return S.plans[scanId];
  S.plans[scanId] = "loading";
  api(`/scans/${encodeURIComponent(scanId)}/next-station/plan`)
    .then((p) => { S.plans[scanId] = p; })
    .catch((e) => { S.plans[scanId] = { error: e.message }; })
    .finally(() => render());
  return "loading";
}
function spotWords(c) {
  const lm = c.landmark;
  const by = lm ? `, ${lm.distance_m.toFixed(0)} m from ${lm.shape === "post" ? `a post ${lm.height_m.toFixed(1)} m tall` : `something large (${lm.height_m.toFixed(0)} m tall: a tree, vehicle or building)`}` : "";
  return `${c.distance_m.toFixed(0)} m out, about ${c.walk_m.toFixed(0)} m to walk${by}`;
}
// ------------------------------------------------------ the station's view ---
// Real-size things drawn into the station's panorama: the bridge projects
// each 3D point (on the ground, or so many metres up) onto the picture and
// says whether the scan saw something nearer, so hidden parts draw dashed.
const RINGS_M = [5, 10, 20, 40];
const LEG_SPREAD_M = 0.45;
const FOOTPRINT_M = 0.6;
const circle = (cx, cy, r, n, up = 0) => Array.from({ length: n + 1 }, (_, i) => ({ xy: [cx + r * Math.cos(2 * Math.PI * i / n), cy + r * Math.sin(2 * Math.PI * i / n)], up }));
const line3 = (a, b, n) => Array.from({ length: n + 1 }, (_, i) => [a[0] + (b[0] - a[0]) * i / n, a[1] + (b[1] - a[1]) * i / n, a[2] + (b[2] - a[2]) * i / n]);
// A tripod at (x, y): three feet, the head, and the scanner on top, `h` m up.
const tripodPts = (x, y, h) => [
  ...[0, 1, 2].map((k) => ({ xy: [x + LEG_SPREAD_M * Math.cos(Math.PI / 2 + 2 * Math.PI * k / 3), y + LEG_SPREAD_M * Math.sin(Math.PI / 2 + 2 * Math.PI * k / 3)], up: 0 })),
  { xy: [x, y], up: h - 0.32 }, { xy: [x, y], up: h }, { xy: [x, y], up: 0 },
];

function overlayFor(pid, dome, plan) {
  const reg = S.regs[pid];
  const m = S.missions.find((x) => x.project_id === pid);
  const key = `${dome.scan_id}|${reg?.updated_at ?? ""}|${m ? m.targets.map((t) => `${t.target_id}:${t.question?.verdict ?? t.status}`).join() : ""}`;
  const got = S.overlays[key];
  if (got) return got;
  S.overlays[key] = "loading";
  (async () => {
    const scanPts = [];
    const take = (pts) => { const i = scanPts.length; scanPts.push(...pts); return [i, pts.length]; };
    const rings = RINGS_M.map((r) => ({ r, at: take(circle(0, 0, r, 144)) }));
    // The scanner's own height above its ground first: tripods are drawn that tall.
    const probe = await api(`/scans/${encodeURIComponent(dome.scan_id)}/project`, { method: "POST", body: { points: [{ xy: [1, 0], up: 0 }] } });
    const H = probe.scanner_height_m || 1.5;
    const spots = (plan.candidates ?? []).map((c) => {
      const [x, y] = c.xy_m;
      const d = Math.hypot(x, y);
      const n = Math.max(2, Math.round(d / 0.5));
      const path = Array.from({ length: n + 1 }, (_, i) => ({ xy: [x * (1.2 / d + (1 - 1.2 / d) * i / n), y * (1.2 / d + (1 - 1.2 / d) * i / n)], up: 0 }));
      const ticks = [];
      for (let t = 5; t < d - 1; t += 5) ticks.push({ xy: [x * t / d, y * t / d], up: 0 });
      const lm = c.landmark ? { at: take([{ xy: c.landmark.xy_m, up: 0 }, { xy: c.landmark.xy_m, up: c.landmark.height_m }]), height: c.landmark.height_m, shape: c.landmark.shape } : null;
      return { rank: c.rank, dist: d, foot: take(circle(x, y, FOOTPRINT_M, 28)), tripod: take(tripodPts(x, y, H)), path: take(path), ticks: take(ticks), lm };
    });
    const projPts = [];
    const takeP = (pts) => { const i = projPts.length; projPts.push(...pts); return [i, pts.length]; };
    const here = reg?.stations?.find((s) => s.scan_id === dome.scan_id);
    const stations = here?.placed ? (reg.stations ?? []).filter((s) => s.placed && s.scan_id !== dome.scan_id && s.xyz_m)
      .map((s) => ({ station: s.station, verdict: s.verdict, tripod: takeP(tripodPts(s.xyz_m[0], s.xyz_m[1], H)) })) : [];
    const faces = here?.placed && m ? m.targets.filter((t) => t.question && t.face && t.geometry?.height_m).map((t) => {
      const [z0, z1] = t.geometry.height_m;
      const a = [t.face.from[0], t.face.from[1]], b = [t.face.to[0], t.face.to[1]];
      const n = Math.max(8, Math.round(Math.hypot(b[0] - a[0], b[1] - a[1]) / 0.5));
      const outline = [...line3([...a, z0], [...b, z0], n), ...line3([...b, z0], [...b, z1], 12).slice(1), ...line3([...b, z1], [...a, z1], n).slice(1), ...line3([...a, z1], [...a, z0], 12).slice(1)];
      return { t, outline: takeP(outline), label: takeP([[(a[0] + b[0]) / 2, (a[1] + b[1]) / 2, z1]]) };
    }) : [];
    const [s1, p1] = await Promise.all([
      api(`/scans/${encodeURIComponent(dome.scan_id)}/project`, { method: "POST", body: { points: scanPts } }),
      projPts.length ? api(`/scans/${encodeURIComponent(dome.scan_id)}/project`, { method: "POST", body: { frame: "project", points: projPts } }) : null,
    ]);
    const cut = (res, [i, n]) => res.points.slice(i, i + n);
    S.overlays[key] = {
      size: s1.preview_size, height: H,
      rings: rings.map((r) => ({ r: r.r, pts: cut(s1, r.at) })),
      spots: spots.map((s) => ({ rank: s.rank, dist: s.dist, foot: cut(s1, s.foot), tripod: cut(s1, s.tripod), path: cut(s1, s.path), ticks: cut(s1, s.ticks),
        lm: s.lm && { base: cut(s1, s.lm.at)[0], top: cut(s1, s.lm.at)[1], height: s.lm.height, shape: s.lm.shape } })),
      stations: p1 ? stations.map((s) => ({ ...s, tripod: cut(p1, s.tripod) })) : [],
      faces: p1 ? faces.map((f) => ({ t: f.t, outline: cut(p1, f.outline), label: cut(p1, f.label)[0] })) : [],
    };
  })().catch((e) => { S.overlays[key] = { error: e.message }; }).finally(() => render());
  return "loading";
}

// Projected points to SVG paths: split where a point is off the picture,
// across the panorama's seam, and where it goes from seen to hidden.
function runs(pts, w) {
  const out = [];
  let cur = null;
  for (const p of pts) {
    if (!p) { cur = null; continue; }
    const hid = p.seen === "hidden";
    if (cur && (cur.hidden !== hid || Math.abs(p.px[0] - cur.last[0]) > w / 2)) {
      if (cur.hidden !== hid && Math.abs(p.px[0] - cur.last[0]) <= w / 2) cur.d += ` L${p.px[0]},${p.px[1]}`;
      cur = null;
    }
    if (!cur) { cur = { hidden: hid, d: `M${p.px[0]},${p.px[1]}`, last: p.px }; out.push(cur); }
    else { cur.d += ` L${p.px[0]},${p.px[1]}`; cur.last = p.px; }
  }
  return out;
}
const strokes = (pts, w, cls) => runs(pts, w).map((r) => `<path class="${cls}${r.hidden ? " hid" : ""}" d="${r.d}"/>`).join("");
const tag = (p, text, cls, fs) => p ? `<text class="${cls}${p.seen === "hidden" ? " hid" : ""}" x="${p.px[0]}" y="${p.px[1]}" font-size="${fs}">${esc(text)}</text>` : "";
const VERDICT_CLS = { answer: "good", not_yet: "warn", does_not_fit: "bad" };

function overlaySvg(o, decided, st, next) {
  const [w, h] = o.size;
  const fs = Math.max(14, w / 70);
  // Calm: the next spot only. More: the other spots, earlier stations, the
  // faces asked about and distance rings.
  const more = S.showMore;
  const L = { distances: more, questions: more, stations: more, spots: true };
  let g = "";
  // Labels that would overlap one already placed move up, with a leader line
  // back to what they name.
  const placed = [];
  const label = (p, text, cls, size) => {
    if (!p) return "";
    const bw = text.length * size * 0.56, bh = size * 1.15;
    const anchor = p.px;
    let y = anchor[1];
    const hits = (yy) => placed.some((b) => Math.abs(b.x - anchor[0]) < (b.w + bw) / 2 && Math.abs(b.y - yy) < (b.h + bh) / 2);
    let moved = 0;
    while (hits(y) && moved < 8) { y -= bh * 1.05; moved++; }
    placed.push({ x: anchor[0], y, w: bw, h: bh });
    const lead = moved ? `<path class="lead" d="M${anchor[0]},${anchor[1] - size * 0.2} L${anchor[0]},${y + size * 0.25}"/>` : "";
    return lead + tag({ ...p, px: [anchor[0], y] }, text, cls, size);
  };
  const ringLabels = [];
  if (L.distances) {
    for (const r of o.rings) {
      g += strokes(r.pts, w, "ring");
      // Labelled where the ring is lowest on the picture (nearest the viewer).
      const vis = r.pts.filter((p) => p && p.seen !== "hidden");
      for (const q of [0.125, 0.375, 0.625, 0.875]) {
        const p = r.pts[Math.round(q * (r.pts.length - 1))];
        if (p && p.seen !== "hidden" && vis.length) ringLabels.push([{ ...p, px: [p.px[0], p.px[1] - fs * 0.3] }, `${r.r} m`]);
      }
    }
  }
  if (L.questions) {
    for (const f of o.faces) {
      const q = f.t.question;
      const cls = VERDICT_CLS[q.verdict] ?? "none";
      const runsF = runs(f.outline, w);
      const closed = runsF.length === 1 && !runsF[0].hidden ? `${runsF[0].d} Z` : null;
      if (closed) g += `<path class="face-fill ${cls}" d="${closed}"/>`;
      g += strokes(f.outline, w, `face ${cls}`);
      const said = q.verdict === "answer" && q.said
        ? `${Math.abs(q.said.out_of_plumb_mm).toFixed(1)} mm${f.t.tolerance ? (q.said.within === true ? " · within" : q.said.within === false ? " · outside" : "") : ""}`
        : q.verdict === "not_yet" ? "not yet" : q.verdict === "does_not_fit" ? "doesn't fit" : "not asked";
      if (f.label) g += label({ ...f.label, px: [f.label.px[0], f.label.px[1] - fs * 0.5] }, `${f.t.name.replace(/ \(.*\)$/, "")} · ${said}`, `face-t ${cls}`, fs);
    }
  }
  if (L.stations) {
    for (const s of o.stations) {
      const [f1, f2, f3, head, top, base] = s.tripod;
      if (!top) continue;
      for (const f of [f1, f2, f3]) if (f && head) g += strokes([f, head], w, "ghost");
      g += strokes([head, top], w, "ghost head");
      g += `<circle class="ghost-dot${top.seen === "hidden" ? " hid" : ""}" cx="${top.px[0]}" cy="${top.px[1]}" r="${fs * 0.55}"/>`;
      g += tag({ ...top, px: [top.px[0], top.px[1] + fs * 0.32] }, String(s.station), "ghost-n", fs * 0.75);
    }
  }
  if (L.spots) {
    for (const s of o.spots) {
      const d = decided[`${st}|${s.rank}`];
      const state = d ? (d.use ? " use" : " no") : "";
      const full = s.rank === next || (more && !(d && !d.use));
      const [, , , head0, top0] = s.tripod;
      if (!full) {
        // Another spot: just its number, to tap.
        if (top0) {
          const by = top0.px[1] - fs * 1.3;
          g += `<g class="spot3d other${state}"><g class="badge small" data-act="spot-focus" data-rank="${s.rank}"><circle cx="${top0.px[0]}" cy="${by}" r="${fs * 0.55}"/><text x="${top0.px[0]}" y="${by + fs * 0.27}" font-size="${fs * 0.75}">${s.rank}</text></g></g>`;
        }
        continue;
      }
      const dim = more && s.rank !== next ? " dim" : "";
      g += `<g class="spot3d${state}${dim}">`;
      g += strokes(s.path, w, "walk");
      if (more) s.ticks.forEach((p) => { if (p) g += `<circle class="tick${p.seen === "hidden" ? " hid" : ""}" cx="${p.px[0]}" cy="${p.px[1]}" r="${fs * 0.16}"/>`; });
      g += strokes(s.foot, w, "foot");
      const runsFoot = runs(s.foot, w);
      if (runsFoot.length === 1) g += `<path class="foot-fill" d="${runsFoot[0].d} Z"/>`;
      if (s.lm?.base && s.lm.top) {
        g += strokes([s.lm.base, s.lm.top], w, "pole");
        if (s.rank === next) g += label({ ...s.lm.top, px: [s.lm.top.px[0], s.lm.top.px[1] - fs * 0.3] }, s.lm.shape === "post" ? "post" : "landmark", "pole-t", fs * 0.75);
      }
      const [f1, f2, f3, head, top] = s.tripod;
      for (const f of [f1, f2, f3]) if (f && head) g += strokes([f, head], w, "leg");
      if (head && top) g += strokes([head, top], w, "scanner");
      if (top) {
        const by = top.px[1] - fs * 1.3;
        g += `<g class="badge" data-act="spot-focus" data-rank="${s.rank}"><circle cx="${top.px[0]}" cy="${by}" r="${fs * 0.75}"/><text x="${top.px[0]}" y="${by + fs * 0.35}" font-size="${fs}">${s.rank}</text></g>`;
        g += label({ ...top, px: [top.px[0], by - fs * 1.05] }, `${Math.round(s.dist)} m`, "badge-t", fs * 0.75);
      }
      g += `</g>`;
    }
  }
  // Distance labels last, and only where they don't crowd something else.
  for (const [p, text] of ringLabels) {
    const bw = text.length * fs * 0.45, bh = fs * 0.9;
    if (!placed.some((b) => Math.abs(b.x - p.px[0]) < (b.w + bw) / 2 && Math.abs(b.y - p.px[1]) < (b.h + bh) / 2)) {
      placed.push({ x: p.px[0], y: p.px[1], w: bw, h: bh });
      g += tag(p, text, "ring-t", fs * 0.8);
    }
  }
  return `<svg class="overlay" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none" aria-hidden="true">${g}</svg>`;
}

// A small reminder that the agent in the chat can say more than the page does.
const CHAT_ICON = `<svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><path d="M2.5 3h11a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1H7l-3 2.5V11H2.5a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1z" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/></svg>`;

// The spot to show: the one tapped, else the one chosen, else the best not ruled out.
function nextSpot(cands, decided, st) {
  if (S.spotFocus != null && cands.some((c) => c.rank === S.spotFocus)) return S.spotFocus;
  const by = (f) => cands.find((c) => f(decided[`${st}|${c.rank}`]));
  return (by((d) => d?.use) ?? by((d) => !d) ?? cands[0]).rank;
}
function spotShort(c) {
  const lm = c.landmark;
  const by = lm ? (lm.shape === "post" ? `, by a post ${lm.height_m.toFixed(1)} m tall` : `, by something ${lm.height_m.toFixed(0)} m tall`) : "";
  return `${Math.round(c.distance_m)} m out, ${Math.round(c.walk_m)} m to walk${by}`;
}

function spotsCard(pid) {
  const dome = latestDome(pid);
  if (!dome) return "";
  const plan = loadPlan(dome.scan_id);
  if (plan === "loading") return `<div class="card"><div class="placeholder small">Working out where to set up next…</div></div>`;
  if (plan?.error) return `<div class="card"><div class="problem">${esc(plan.error)}</div></div>`;
  const cands = plan.candidates ?? [];
  if (!cands.length) return `<div class="card"><p class="meta">${esc(plan.reason || "No more spots to suggest from this station.")}</p></div>`;
  const decided = spotDecisions(pid);
  const st = dome.station;
  const img = preview(dome.scan_id);
  const [pw, ph] = plan.preview_size ?? [0, 0];
  const ov = overlayFor(pid, dome, plan);
  const next = nextSpot(cands, decided, st);
  // Until the 3D overlay is in (or if it fails), plain markers at the planner's pixels.
  const marks = typeof ov === "object" && !ov.error ? overlaySvg(ov, decided, st, next) : pw && ph ? cands.filter((c) => S.showMore || c.rank === next).map((c) => {
    const d = decided[`${st}|${c.rank}`];
    const cls = d ? (d.use ? "spot use" : "spot no") : "spot";
    return c.preview_px ? `<span class="${cls}" style="left:${(100 * c.preview_px[0] / pw).toFixed(2)}%;top:${(100 * c.preview_px[1] / ph).toFixed(2)}%">${c.rank}</span>` : "";
  }).join("") : "";
  const moreBtn = typeof ov === "object" && !ov.error
    ? `<button class="layer ${S.showMore ? "on" : ""}" data-act="more">${S.showMore ? "Show less" : "Show more"}</button>` : "";
  const pic = typeof img === "string" && img !== "loading"
    ? `<div class="pic-wrap"><div class="spots-pic" data-keep-scroll="spots:${esc(dome.scan_id)}"><div class="spots-inner"><img src="${esc(img)}" alt="Station ${esc(st)}'s view, with the next spot drawn as a tripod to scale and the walk to it">${marks}</div></div>
       <div class="ask-chat" title="The agent in the chat can explain any of this, or find another spot">${CHAT_ICON}Ask in the chat for more, like “why spot ${next}?”</div></div>
       <div class="line pic-foot"><span class="hint">From station ${esc(st)}, to scale${S.showMore ? ". Dashed: hidden from here" : ""}.</span>${moreBtn}</div>`
    : img?.error ? "" : `<div class="placeholder small">Loading station ${esc(st)}'s view…</div>`;
  const spotRow = (c, main) => {
    const d = decided[`${st}|${c.rank}`];
    const state = d ? `<div class="meta decided">${d.use ? "Chosen" : "Ruled out"} · ${esc(ago(d.at))}${d.use ? "" : `: ${esc(d.text.replace(SPOT_LOG, "").replace(/^:\s*/, ""))}`}</div>` : "";
    const btns = canAct() && !(d && !d.use) ? `<div class="btns-row">
        <button class="btn small ${d?.use ? "good" : main ? "primary" : ""}" data-act="spot-use" data-pid="${esc(pid)}" data-station="${esc(st)}" data-rank="${c.rank}" ${S.busy || d?.use ? "disabled" : ""}>${d?.use ? "Chosen" : "Set up here"}</button>
        <button class="btn small" data-act="spot-no" data-pid="${esc(pid)}" data-station="${esc(st)}" data-rank="${c.rank}" ${S.busy ? "disabled" : ""}>Can't stand here</button></div>` : "";
    return `<div class="${main ? "next-spot" : "other-spot"}"><span class="dot ${d ? (d.use ? "done" : "failed") : "now"}">${c.rank}</span><div class="grow">
      <strong>${main ? "Next: " : ""}${esc(spotShort(c))}</strong>${state}${btns}</div></div>`;
  };
  const main = cands.find((c) => c.rank === next);
  const others = cands.filter((c) => c.rank !== next);
  const otherList = others.length ? `<button class="fold" data-act="other-spots">${S.otherSpots ? "▾" : "▸"} Other spots (${others.length})</button>
    ${S.otherSpots ? others.map((c) => spotRow(c, false)).join("") : ""}` : "";
  return `<div class="card">${pic}${spotRow(main, true)}${otherList}</div>`;
}

// --------------------------------------------------------------- trust ---
// Stations not yet trusted, in words, with the one question a person can
// answer: did the tripod move?
const TRUST = {
  single: "Tied to the rest by one overlap only. Fine for now; the next station should also see what this one saw.",
  doubtful: "Doesn't agree with its neighbours.",
  grouped: "Placed with its own group, not yet joined to the rest. The next station should see walls from both groups.",
  ambiguous: "Could fit in more than one place. The next station should overlap it more.",
  unplaced: "Not placed yet: too little overlap with the others.",
};
const BUMP_LOG = /^Station (\d+): the person says the tripod (was bumped|didn't move)/;
function trustNotes(pid) {
  const r = S.regs[pid];
  // Only what the person can answer: a doubtful station (did the tripod move?).
  // The rest is the agent's to sort out with the next station.
  const odd = (r?.stations ?? []).filter((s) => s.verdict === "doubtful");
  if (!odd.length) return "";
  const said = {};
  for (const e of S.log) {
    if (e.project_id !== pid) continue;
    const m = BUMP_LOG.exec(e.text || "");
    if (m && !(m[1] in said)) said[m[1]] = m[2];
  }
  return `<ul class="ledger trust">${odd.map((s) => {
    const ask = s.verdict === "doubtful" && canAct()
      ? said[s.station] ? `<div class="meta decided">You said the tripod ${esc(said[s.station])}.</div>`
        : `<div class="meta">Did the tripod move or get bumped during this scan?</div><div class="btns-row">
          <button class="btn small" data-act="bumped" data-pid="${esc(pid)}" data-station="${esc(s.station)}" data-said="was bumped">It was bumped</button>
          <button class="btn small" data-act="bumped" data-pid="${esc(pid)}" data-station="${esc(s.station)}" data-said="didn't move">It didn't move</button></div>`
      : "";
    return `<li><span class="chip ${s.verdict === "single" ? "info" : "warn"}">${esc(s.verdict)}</span><div class="grow"><strong>Station ${esc(s.station)}</strong>
      <div class="meta">${esc(TRUST[s.verdict])}</div>${s.reasons?.length ? `<div class="meta small">${esc(s.reasons[0])}</div>` : ""}${ask}</div></li>`;
  }).join("")}</ul>`;
}

function workerText() {
  if (!S.workers.length) return "on the Pi";
  const w = S.workers[0];
  return `${esc(w.worker)}${w.job_id ? " · working" : " · ready"}${S.workers.length > 1 ? ` +${S.workers.length - 1}` : ""}`;
}

// The project as a line map (walls, stations), redrawn as stations land and register.
function minimapCard(pid) {
  if (!(pid in S.regs)) {
    S.regs[pid] = null;
    api(`/projects/${encodeURIComponent(pid)}/registration`).then((v) => { S.regs[pid] = v; }).catch(() => {}).finally(() => render());
  }
  const r = S.regs[pid];
  const registering = S.jobs.filter((j) => j.kind === "register" && (j.state === "running" || j.state === "queued"));
  const domes = S.scans.filter((s) => s.project_id === pid && s.kind === "full_dome").length;
  const m = S.missions.find((x) => x.project_id === pid);
  const key = `${pid}|${r?.updated_at ?? ""}|${registering.length}|${domes}|${m ? m.targets.map((t) => t.status).join() : ""}`;
  let img = S.mapImages.get(key);
  if (!img) {
    S.mapImages.set(key, "loading");
    api(`/projects/${encodeURIComponent(pid)}/minimap?format=svg`, { raw: true })
      .then((x) => x.blob())
      .then((b) => { const u = URL.createObjectURL(b); S.mapImages.set(key, u); S.mapLast[pid] = u; })
      .catch((e) => S.mapImages.set(key, { error: e.message }))
      .finally(() => render());
    img = "loading";
  }
  const shown = typeof img === "string" && img !== "loading" ? img : S.mapLast[pid];
  const pic = shown ? `<a href="${esc(shown)}" target="_blank" rel="noopener" title="Open full size"><img class="minimap" src="${esc(shown)}" alt="Line map of the walls found and the stations so far"></a>`
    : img?.error ? `<div class="problem">${esc(img.error)}</div>` : `<div class="placeholder small">Drawing the map…</div>`;
  const phase = registering[0]?.phase;
  return `<div class="card">${pic}<div class="meta">${registering.length ? `Registering${phase ? `: ${esc(phase)}` : "…"}` : esc(r?.summary ?? "The map fills in as stations are scanned.")}</div>${trustNotes(pid)}</div>`;
}

// The agent's proposal for where to set up next: the picture and its directions.
function nextStationCard(p) {
  const img = nsImage(p);
  const pic = img === "loading" ? `<div class="placeholder small">Loading the picture…</div>`
    : img?.error ? `<div class="problem">${esc(img.error)}</div>`
    : `<a href="${esc(img)}" target="_blank" rel="noopener" title="Open full size"><img class="nextstation" src="${esc(img)}" alt="The station's view, marked with where to set up next"></a>`;
  return `<div class="card attn"><div class="line"><strong>Set up the next station</strong><span class="meta">from station ${esc(p.station ?? "")} · ${esc(p.proposed_by)} · ${esc(ago(p.at))}</span></div>
    <p class="ns-note">${esc(p.note)}</p>${pic}</div>`;
}

function evidence(id) {
  const p = S.evidence.get(id);
  if (p) return p;
  S.evidence.set(id, "loading");
  api(`/measurements/${encodeURIComponent(id)}/evidence`, { raw: true })
    .then((r) => r.blob())
    .then((b) => S.evidence.set(id, URL.createObjectURL(b)))
    .catch((e) => S.evidence.set(id, { error: e.message }))
    .finally(() => render());
  return "loading";
}

// A measurement's values, checks and evidence image.
function measurementCard(m) {
  if (!m || m.error) return m?.error ? `<div class="problem">${esc(m.error)}</div>` : "";
  const ev = evidence(m.measurement_id);
  const img = ev === "loading" ? `<div class="placeholder small">Loading the evidence image…</div>` : ev?.error ? `<div class="problem">${esc(ev.error)}</div>` : `<img class="evidence" src="${esc(ev)}" alt="Wall face (grey), opening (green), measuring row and column (red)">`;
  return `<div class="measure"><div class="big">${esc(m.opening_width_mm)} × ${esc(m.opening_height_mm)} mm</div>
    <div class="meta">width × height, ± ${esc(m.uncertainty_mm)} mm · ${esc(m.details?.distance_m)} m from the scanner</div>
    ${m.site_check?.rescans ? `<div class="note">The bridge rescanned ${m.site_check.rescans === 1 ? "once" : `${m.site_check.rescans} times`} on its own: ${esc(m.site_check.attempts.filter((a) => a.short_sides.length).map((a) => `${a.opening_width_mm} × ${a.opening_height_mm} mm with too little wall at the ${a.short_sides.join(", ")}`).join("; "))}.</div>` : ""}
    ${img}<p class="hint">Grey: the wall face. Green: the opening. Red: where it was measured. The green should fill the window up to the wall.</p>
    <ul class="steps">${(m.checks || []).map((c) => `<li><span class="dot ${c.ok ? "done" : "failed"}">${c.ok ? "✓" : "×"}</span><div class="grow"><strong>${esc(MCHECKS[c.name] ?? c.name)}</strong><div class="meta">${esc(c.detail ?? "")}</div></div></li>`).join("")}</ul></div>`;
}
const MCHECKS = { wall_fit: "Wall face fit", opening_bounded: "Wall found around the opening", margins: "Enough wall on every side", unoccluded: "Nothing in front of the wall", consistent: "Rows agree" };
const FIELD = (n) => ({ opening_width_mm: "Opening width (mm)", opening_height_mm: "Opening height (mm)" })[n] ?? n;

async function loadExport(id) {
  try { S.exports[id] = await api(`/exports/${encodeURIComponent(id)}`); } catch (e) { S.exports[id] = { error: e.message }; }
  render();
}

async function download(path, name) {
  const r = await api(path, { raw: true });
  const cd = r.headers.get("content-disposition") || "";
  const m = cd.match(/filename="([^"]+)"/);
  const url = URL.createObjectURL(await r.blob());
  const a = document.createElement("a");
  a.href = url;
  a.download = m ? m[1] : name;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60000);
}

// ----------------------------------------------------------------- events ---
function onEvent(chunk) {
  let kind = "message";
  let data = "";
  for (const line of chunk.split("\n")) {
    if (line.startsWith("event:")) kind = line.slice(6).trim();
    else if (line.startsWith("data:")) data += line.slice(5).trim();
  }
  if (!data) return;
  let v;
  try { v = JSON.parse(data); } catch { return; }
  if (kind === "job") {
    upsert(S.jobs, v, "job_id");
    S.jobs.sort(byNewest);
    if (v.kind === "export" && v.state === "succeeded" && v.result?.export_id && !S.exports[v.result.export_id]) loadExport(v.result.export_id);
    if (v.kind === "measure" && v.state === "succeeded" && v.result?.measurement_id && !S.measurements[v.result.measurement_id]) loadMeasurement(v.result.measurement_id);
    if (v.kind === "scan" && v.state === "succeeded") {
      load("projects"); load("scans"); load("status");
      // The viewer follows the project it is showing: open each new scan as it lands.
      const showing = viewer?.scanId && S.scanById[viewer.scanId];
      if (S.route.view && viewer.follow && v.result?.scan_id && showing && v.request?.project_id === showing.project_id && v.result.scan_id !== viewer.scanId) {
        location.hash = `#/view/${encodeURIComponent(v.result.scan_id)}`;
      }
    }
  } else if (kind === "mission") {
    S.missions = [v, ...S.missions.filter((m) => m.project_id !== v.project_id)];
  } else if (kind === "registration") {
    S.regs[v.project_id] = v;
  } else if (kind === "next_station") {
    S.nextStations = [v, ...S.nextStations.filter((p) => p.scan_id !== v.scan_id)];
  } else if (kind === "run") {
    upsertRun(v);
    S.runs.sort(byNewest);
  } else if (kind === "log") {
    if (!S.log.some((e) => e.entry_id === v.entry_id)) S.log.unshift(v);
    S.log.length = Math.min(S.log.length, 500);
  }
  render();
}

async function stream() {
  let delay = 1000;
  while (S.token) {
    try {
      const r = await fetch("/v1/events", { headers: { Authorization: `Bearer ${S.token}` } });
      if (r.status === 401) { signOut("That token was not accepted."); return; }
      if (!r.ok || !r.body) throw new Error(`events: HTTP ${r.status}`);
      S.live = true;
      delay = 1000;
      refreshAll();
      const reader = r.body.pipeThrough(new TextDecoderStream()).getReader();
      let buf = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += value.replace(/\r\n/g, "\n");
        let i;
        while ((i = buf.indexOf("\n\n")) >= 0) {
          onEvent(buf.slice(0, i));
          buf = buf.slice(i + 2);
        }
      }
    } catch { /* reconnect below */ }
    S.live = false;
    render();
    await sleep(delay);
    delay = Math.min(delay * 2, 15000);
  }
}

function refreshAll() {
  ["status", "jobs", "runs", "recipes", "projects", "modes", "log", "scans", "grants", "next-stations", "missions", "workers"].forEach(load);
}

// ----------------------------------------------------------------- routes ---
function parseRoute() {
  const [tab, id] = location.hash.replace(/^#\/?/, "").split("/");
  if (tab === "run" && id) return { tab: "runs", run: decodeURIComponent(id) };
  if (tab === "scan" && id) return { tab: "scans", scan: decodeURIComponent(id) };
  if (tab === "view" && id) return { tab: "scans", scan: decodeURIComponent(id), view: true };
  return { tab: ["now", "runs", "scans", "log"].includes(tab) ? tab : "now" };
}

// ---------------------------------------------------------------- sign in ---
async function signIn(token) {
  S.token = token.trim();
  S.signinError = "";
  try {
    S.me = await api("/whoami");
    store.set("x9.token", S.token);
    render(true);
    refreshAll();
    stream();
    return true;
  } catch (e) {
    S.token = null;
    S.signinError = e.status === 401 ? "That token was not accepted." : `Could not reach the bridge: ${e.message}`;
    render(true);
    return false;
  }
}

function signOut(why = "") {
  S.token = null;
  S.me = null;
  S.signinError = why;
  store.set("x9.token", null);
  render(true);
}

// ---------------------------------------------------------------- render ---
const ICONS = {
  now: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></svg>',
  runs: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 6h11M9 12h11M9 18h11"/><path d="M4 6l1 1 2-2M4 12l1 1 2-2M4 18l1 1 2-2"/></svg>',
  scans: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="8" y="3" width="8" height="11" rx="2"/><circle cx="12" cy="8" r="2"/><path d="M12 14v3M7 21l5-4 5 4"/></svg>',
  log: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 3h9l4 4v14H6z"/><path d="M9 11h7M9 15h7M9 7h4"/></svg>',
};

const stepDef = (r, sid) => { const rec = S.recipe[r.recipe_id]; return typeof rec === "object" && rec.steps ? rec.steps.find((s) => (s.id ?? s.step_id) === sid) : null; };
// A person step with review = "final" is the agent's to answer; a person sees it at the sign-off.
const agentsStep = (r) => stepDef(r, r.current_step_id)?.review === "final";
const waitsForPerson = (r) => r.state === "waiting_for_person" && r.waiting_for === "input" && !agentsStep(r);

const runChip = (r) => (r.state === "waiting_for_person" && r.waiting_for === "input" && agentsStep(r) ? `<span class="chip info">agent's turn</span>` : stateChip(r.state));

function needsYou() {
  const approvals = isOp() ? S.jobs.filter((j) => j.state === "awaiting_approval").length : 0;
  const inputs = S.runs.filter((r) => waitsForPerson(r) && mayAdvance(r)).length;
  return approvals + inputs;
}
const mayAdvance = (r) => S.me && (isOp() || r.started_by === S.me.name);
const mayCancelJob = (j) => S.me && (isOp() || j.requested_by === S.me.name);

function viewSignin() {
  return `<div class="signin">
    <img class="logo" src="icon.svg" alt="">
    <h1>X9 Bridge</h1>
    <p class="meta">Paste the token you were given for this bridge. It is kept in this browser.</p>
    ${S.signinError ? `<div class="problem">${esc(S.signinError)}</div>` : ""}
    <form data-form="signin">
      <label class="field"><span>Token</span><input name="token" type="password" autocomplete="off" autocapitalize="off" spellcheck="false" required></label>
      <button class="btn primary wide" ${S.busy ? "disabled" : ""}>Sign in</button>
    </form>
  </div>`;
}

function viewTop() {
  const r = S.route;
  const title = r.run ? "Run" : r.scan ? "Scan" : { now: "Now", runs: "Recipes and runs", scans: "Scans", log: "Field log" }[r.tab];
  const back = r.run || r.scan ? `<button class="back" data-act="back">‹ Back</button>` : "";
  return `<header class="top">${back}<span class="brand">X9 Bridge</span><h1 class="${back ? "" : "tab-title"}">${esc(title)}</h1>${viewNav()}
    <span class="live ${S.live ? "on" : ""}">${S.live ? "Live" : "Offline"}</span>
    <button class="who" data-act="me">${esc(S.me?.name ?? "")}</button></header>`;
}

function viewNav() {
  const n = needsYou();
  const tab = (id, label) => `<a href="#/${id}" class="${S.route.tab === id ? "on" : ""}">${ICONS[id]}${label}${id === "now" && n ? `<span class="badge">${n}</span>` : ""}</a>`;
  return `<nav class="nav">${tab("now", "Now")}${tab("runs", "Runs")}${tab("scans", "Scans")}${tab("log", "Log")}</nav>`;
}

const RUN_HOURS = 2;

function jobCard(j, { actions = true } = {}) {
  const phase = j.phase ? PHASES[j.phase] ?? j.phase : j.state === "queued" ? "Queued" : "";
  const pct = typeof j.progress === "number" ? Math.round(j.progress * 100) : null;
  const running = j.state === "running" || j.state === "queued";
  const scanLink = j.result?.scan_id ? `<a href="#/scan/${encodeURIComponent(j.result.scan_id)}">Open scan</a>` : "";
  const partial = j.partial_scan_id ? `<div class="note">The scanner kept the partial sweep. <a href="#/scan/${encodeURIComponent(j.partial_scan_id)}">Keep or delete it</a></div>` : "";
  return `<div class="card ${j.state === "awaiting_approval" ? "attn" : ""}">
    <div class="line top-align"><strong class="grow">${esc(describeJob(j))}</strong>${stateChip(j.state)}</div>
    <div class="meta">${esc(jobDetail(j))}${jobDetail(j) ? " · " : ""}by ${esc(j.requested_by)} · ${esc(clock(j.created_at))}</div>
    ${running ? `<div class="bar ${pct == null ? "busy" : ""}"><i style="width:${pct ?? 0}%"></i></div>
      <div class="line meta"><span>${esc(phase)}${pct != null ? ` · ${pct}%` : ""}</span><span>${esc(elapsed(j.started_at))}</span></div>` : ""}
    ${j.problem ? `<div class="problem">${esc(problemText(j.problem))}</div>` : ""}
    ${partial}
    ${scanLink && !running ? `<div class="meta" style="margin-top:6px">${scanLink}</div>` : ""}
    ${actions && j.state === "awaiting_approval" && isOp() && j.run_id ? `<p class="hint">Part of a recipe run. Approving the run lets its later scans go ahead too (for ${RUN_HOURS} hours).</p>` : ""}
    ${actions && j.state === "awaiting_approval" && isOp() ? `<div class="btns">
      ${j.run_id ? `<button class="btn good" data-act="approve-run" data-id="${esc(j.job_id)}" ${S.busy ? "disabled" : ""}>Approve the run</button>` : ""}
      <button class="btn ${j.run_id ? "" : "good"}" data-act="approve" data-id="${esc(j.job_id)}" ${S.busy ? "disabled" : ""}>${j.run_id ? "Only this scan" : "Approve"}</button>
      <button class="btn danger" data-act="cancel-job" data-id="${esc(j.job_id)}" ${S.busy ? "disabled" : ""}>Deny</button></div>` : ""}
    ${actions && j.state === "awaiting_approval" && !isOp() ? `<p class="hint">Waiting for an operator to approve.</p>` : ""}
    ${actions && running && mayCancelJob(j) ? `<div class="btns"><button class="btn danger" data-act="cancel-job" data-id="${esc(j.job_id)}" ${S.busy ? "disabled" : ""}>Stop</button></div>` : ""}
  </div>`;
}

function viewNow() {
  const st = S.status;
  let scanner;
  if (!st) scanner = `<div class="card status"><p class="meta">Reading the scanner…</p></div>`;
  else if (st.error) scanner = `<div class="card"><div class="problem">${esc(st.error)}</div></div>`;
  else {
    const s = st.scanner || {};
    if (!s.connected) {
      scanner = `<div class="card attn"><h3>Scanner not connected</h3>
        <p class="meta">The bridge can't reach the X9. Check that it is on and linked to the Pi by USB or its Wi-Fi.</p></div>`;
    } else {
      const batt = s.battery_pct ?? null;
      const battCls = batt == null ? "" : batt < 15 ? "bad" : batt < 30 ? "warn" : "ok";
      const lev = s.leveling?.state ?? "–";
      scanner = `<div class="card status">
        <div class="line"><div class="battery"><span class="pct ${battCls}">${batt ?? "–"}%</span><span class="meta">${s.on_ac_power ? "on power" : "battery"}</span></div>
        ${s.busy || S.jobs.some((j) => j.kind === "scan" && j.state === "running") ? `<span class="chip info">busy</span>` : `<span class="chip good">ready</span>`}</div>
        <div class="bar"><i style="width:${batt ?? 0}%"></i></div>
        <div class="facts">
          <div class="fact"><b>Level</b><span class="${lev === "ok" ? "ok" : "warn"}">${esc(lev)}</span></div>
          <div class="fact"><b>Moved since last scan</b><span class="${s.moved_since_last_scan ? "warn" : "ok"}">${s.moved_since_last_scan ? "yes" : "no"}</span></div>
          <div class="fact"><b>Storage free</b><span>${gb(s.storage_free_bytes)}</span></div>
          <div class="fact"><b>Bridge</b><span>${esc(st.bridge?.version ?? "")}</span></div>
          <div class="fact"><b>Heavy work</b><span>${workerText()}</span></div>
        </div></div>`;
    }
  }

  const approvals = S.jobs.filter((j) => j.state === "awaiting_approval");
  const waiting = S.runs.filter((r) => { if (!S.recipe[r.recipe_id]) recipe(r.recipe_id); return waitsForPerson(r); });
  const active = S.jobs.filter((j) => j.state === "running" || j.state === "queued");
  const recent = S.jobs.filter((j) => ["succeeded", "failed", "cancelled"].includes(j.state)).slice(0, 5);

  const needs = [
    ...approvals.map((j) => jobCard(j)),
    ...waiting.map((r) => {
      const rec = S.recipe[r.recipe_id];
      const step = typeof rec === "object" && rec.steps ? rec.steps.find((s) => (s.id ?? s.step_id) === r.current_step_id) : null;
      if (!S.recipe[r.recipe_id]) recipe(r.recipe_id);
      return `<a class="card link attn" href="#/run/${encodeURIComponent(r.run_id)}">
        <div class="line"><strong class="grow ellip">${esc(recipeTitle(r.recipe_id))}</strong>${stateChip(r.state)}</div>
        <div class="meta">${esc((() => { const p = r.steps.find((x) => x.step_id === r.current_step_id)?.proposal; return p ? `${p.by} proposed an answer${p.note ? `: ${p.note.replace(/[.\s]+$/, "")}` : ""}. Check it.` : step?.instructions ?? `Step “${r.current_step_id}” is waiting for input`; })())}</div></a>`;
    }),
  ];

  const nextStation = (() => {
    const p = S.nextStations.find((x) => !S.project || x.project_id === S.project) ?? S.nextStations[0];
    const spots = S.project ? spotsCard(S.project) : "";
    return p || spots ? `<h2>Next station</h2><section class="next-station">${p ? nextStationCard(p) : ""}${spots}</section>` : "";
  })();
  return `<div class="cols now"><div class="col-a">
    <section>${scanner}</section>
    <h2>Now</h2><section>${active.length ? active.map((j) => jobCard(j)).join("") : `<p class="empty">Nothing running.</p>`}
      ${canAct() ? `<button class="btn primary wide" data-act="sheet-scan">New full-dome scan</button>` : ""}</section>
    </div>${nextStation ? `<div class="span">${nextStation}</div>` : ""}<div class="col-b">
    ${S.project ? `<h2 class="first">Map</h2><section>${minimapCard(S.project)}</section>` : ""}
    ${(() => { const m = S.missions.find((x) => x.project_id === S.project) ?? S.missions[0]; return m ? `<h2 class="${S.project ? "" : "first"}">Mission</h2><section>${missionCard(m)}</section>` : ""; })()}
    <h2 class="${S.nextStations.length || S.missions.length || S.project ? "" : "first"}">Needs you</h2><section>${needs.length ? needs.join("") : `<p class="empty">Nothing needs you.</p>`}</section>
    ${recent.length ? `<h2>Recent</h2><section>${recent.map((j) => jobCard(j, { actions: false })).join("")}</section>` : ""}
    </div></div>`;
}

const CHECKS = { point_count: "Point count", levelled: "Levelled", coordinates: "Coordinates", intensity: "Intensity" };

function interventionText(iv) {
  const parts = [[iv.approvals, "approval"], [iv.confirmed, "confirmation"], [iv.corrected, "correction"], [iv.answered, "answer"]]
    .filter(([n]) => n > 0).map(([n, w]) => `${n} ${w}${n === 1 ? "" : "s"}`);
  return `${iv.total} person intervention${iv.total === 1 ? "" : "s"}${parts.length ? ` (${parts.join(", ")})` : ""}`;
}

const recipeTitle = (id) => S.recipes.find((r) => r.recipe_id === id)?.title ?? id;

function viewRuns() {
  const recipes = S.recipes.length
    ? S.recipes.map((r) => `<button class="card link" style="width:100%;text-align:left" data-act="sheet-run" data-id="${esc(r.recipe_id)}" ${canAct() ? "" : "disabled"}>
        <div class="line"><strong>${esc(r.title)}</strong><span class="meta">v${esc(r.version)}</span></div>
        <div class="meta">${esc(r.summary ?? "")}</div></button>`).join("")
    : `<p class="empty">No recipes installed on this bridge.</p>`;
  const runs = S.runs.length
    ? S.runs.map((r) => `<a class="card link ${r.state === "waiting_for_person" ? "attn" : ""}" href="#/run/${encodeURIComponent(r.run_id)}">
        <div class="line"><strong class="grow ellip">${esc(recipeTitle(r.recipe_id))}</strong>${runChip(r)}</div>
        <div class="meta">by ${esc(r.started_by)} · ${esc(ago(r.created_at))}${r.current_step_id && !["succeeded"].includes(r.state) ? ` · step “${esc(r.current_step_id)}”` : ""}</div></a>`).join("")
    : `<p class="empty">No runs yet.</p>`;
  return `<h2>Start a recipe</h2><section>${recipes}</section><h2>Runs</h2><section>${runs}</section>`;
}

function stepDot(state, i) {
  const map = { done: ["done", "✓"], running: ["now", "…"], waiting: ["wait", "!"], failed: ["failed", "×"], skipped: ["", "–"] };
  const [cls, ch] = map[state] ?? ["", String(i + 1)];
  return `<span class="dot ${cls}">${ch}</span>`;
}

function stepOutput(rs, def) {
  const o = rs.output;
  if (!o) return "";
  if (o.measurement_id) {
    if (!S.measurements[o.measurement_id]) S.measurements[o.measurement_id] = o;
    const sc = o.site_check;
    const re = sc && sc.rescans > 0 ? ` · rescanned ${sc.rescans === 1 ? "once" : `${sc.rescans} times`} (${esc(sc.attempts.filter((a) => a.short_sides.length).map((a) => `${a.short_sides.join(", ")} short`).join("; "))})` : "";
    return `${esc(o.opening_width_mm)} × ${esc(o.opening_height_mm)} mm (± ${esc(o.uncertainty_mm)} mm)${re}`;
  }
  if (o.export_id) {
    const ex = S.exports[o.export_id];
    if (!ex) loadExport(o.export_id);
    return ex && !ex.error ? `${num(ex.point_count)} points · ${esc(ex.format?.toUpperCase())} · <a href="#" data-act="dl-export" data-id="${esc(o.export_id)}">Download</a>` : "Export ready";
  }
  if (o.scan_id) return `<a href="#/scan/${encodeURIComponent(o.scan_id)}">Open scan</a>`;
  if (o.window) return `Window ${deg(o.window.delta_ha)} × ${deg(o.window.delta_va)}`;
  if (def?.kind === "check") return o.ok ? `Checks passed: ${esc((o.checked || []).join(", "))}` : "";
  if (def?.kind === "person") {
    const parts = Object.keys(o).filter((k) => k !== "box").map((k) => `${k}: ${typeof o[k] === "object" ? JSON.stringify(o[k]) : o[k]}`);
    if (o.box) parts.unshift(o.box.x1 >= o.box.x0 ? `box ${o.box.x1 - o.box.x0} × ${o.box.y1 - o.box.y0} px` : `box across the panorama's seam, ${o.box.y1 - o.box.y0} px high`);
    return esc(parts.join(" · ") || "Done");
  }
  return "";
}

function viewRun(id) {
  const r = S.runs.find((x) => x.run_id === id);
  if (!r) return `<p class="empty">This run isn't on the bridge (runs are kept until it restarts).</p>`;
  const rec = S.recipe[r.recipe_id];
  if (!rec) recipe(r.recipe_id);
  const defs = typeof rec === "object" && rec.steps ? rec.steps : [];
  const def = (sid) => defs.find((s) => (s.id ?? s.step_id) === sid);
  const final = ["succeeded", "failed", "cancelled"].includes(r.state);

  let panel = "";
  const cur = r.steps.find((s) => s.step_id === r.current_step_id);
  if (r.state === "waiting_for_person" && r.waiting_for === "input" && cur) {
    const d = def(cur.step_id) || {};
    const needs = d.needs || [];
    const key = `run:${r.run_id}:${cur.step_id}`;
    let previewScan = null;
    if (d.preview_of) previewScan = r.steps.find((s) => s.step_id === d.preview_of)?.output?.scan_id ?? null;
    const prop = cur.proposal;
    // AI-first: the agent proposes, the person validates. Load the proposed box
    // unless the person has drawn their own; a newer proposal replaces it.
    if (prop?.input?.box && !S.userDrew[key] && S.boxFrom[key] !== prop.at) {
      S.boxes[key] = prop.input.box;
      S.boxFrom[key] = prop.at;
      if (S.drawMode[key] === undefined) S.drawMode[key] = false;
    }
    const val = (n) => (prop?.input?.[n] == null ? "" : typeof prop.input[n] === "object" ? JSON.stringify(prop.input[n]) : String(prop.input[n]));
    const src = d.propose_from ? r.steps.find((x) => x.step_id === d.propose_from)?.output : null;
    const fields = needs.filter((n) => n !== "box").map((n) => `<label class="field"><span>${esc(FIELD(n))}</span>${String(val(n)).length > 40 ? `<textarea name="${esc(n)}">${esc(val(n))}</textarea>` : `<input type="text" name="${esc(n)}" value="${esc(val(n))}" autocomplete="off">`}</label>`).join("");
    const changed = prop && (S.userDrew[key] || false);
    // At the sign-off, show what the agent decided by itself earlier in the run.
    const decided = d.review === "final" ? [] : r.steps.filter((s) => s.state === "done" && def(s.step_id)?.review === "final");
    const reviewed = decided.map((s) => {
      const sd = def(s.step_id);
      const scan = sd.preview_of ? r.steps.find((x) => x.step_id === sd.preview_of)?.output?.scan_id : null;
      const k = `review:${r.run_id}:${s.step_id}`;
      if (s.output?.box) { S.boxes[k] = s.output.box; S.drawMode[k] = false; }
      return `<div class="review"><h4>${esc(sd.title ?? s.step_id)}: decided by the agent</h4>
        ${s.output?.box && scan ? picker(k, scan, { readonly: true }) : `<div class="meta">${stepOutput(s, sd)}</div>`}</div>`;
    }).join("");
    if (d.review === "final") {
      panel = `<div class="card"><h3>${esc(d.title ?? cur.step_id)}</h3>
        <p>The agent decides this step and goes on; you review it with the result at the end.</p>
        ${prop ? `<div class="note">${esc(prop.by)} proposed: “${esc(prop.note ?? "an answer")}”.</div>` : ""}
        <p class="hint">${esc(d.instructions ?? "")}</p>
        ${mayAdvance(r) ? `<details><summary class="hint">Answer it yourself instead</summary>
          ${needs.includes("box") ? (previewScan ? picker(key, previewScan) : "") : ""}
          <form data-form="advance" data-run="${esc(r.run_id)}" data-step="${esc(cur.step_id)}" data-key="${esc(key)}">${fields}
          <button class="btn primary wide" ${S.busy || (needs.includes("box") && !S.boxes[key]) ? "disabled" : ""}>Continue with my answer</button></form></details>` : ""}</div>`;
    } else
    panel = `<div class="card attn"><h3>${esc(prop ? `Check ${prop.by}'s answer` : d.title ?? cur.step_id)}</h3>
      ${prop ? `<div class="note">${esc(prop.by)} proposed this ${esc(ago(prop.at))}${prop.note ? `: “${esc(prop.note)}”` : "."} Confirm it, or adjust it first.</div>
        <p class="hint">The step asks: ${esc(d.instructions ?? "")}</p>`
        : `<p>${esc(d.instructions ?? "This step needs your input.")}</p><p class="hint">Waiting for the agent to propose an answer. You can also give it yourself.</p>`}
      ${needs.includes("box") ? (previewScan ? picker(key, previewScan) : `<div class="problem">No preview to draw on.</div>`) : ""}
      ${src?.measurement_id ? measurementCard(src) : ""}
      ${reviewed ? `<h4>Also check what the agent decided</h4>${reviewed}` : ""}
      ${mayAdvance(r) ? `<form data-form="advance" data-run="${esc(r.run_id)}" data-step="${esc(cur.step_id)}" data-key="${esc(key)}">${fields}
        <button class="btn ${prop && !changed ? "good" : "primary"} wide" ${S.busy || (needs.includes("box") && !S.boxes[key]) ? "disabled" : ""}>${prop ? (changed ? "Continue with my box" : "Looks right") : "Continue"}</button></form>`
        : `<p class="hint">Only ${esc(r.started_by)} or an operator can continue this run.</p>`}</div>`;
  } else if (r.state === "waiting_for_person" && r.waiting_for === "approval" && cur?.job_id) {
    const j = S.jobs.find((x) => x.job_id === cur.job_id);
    panel = j ? jobCard(j) : "";
  } else if (cur?.job_id && !final) {
    const j = S.jobs.find((x) => x.job_id === cur.job_id);
    panel = j ? jobCard(j) : "";
  }

  const steps = r.steps.map((s, i) => {
    const d = def(s.step_id);
    return `<li>${stepDot(s.state, i)}<div class="grow"><strong>${esc(d?.title ?? s.step_id)}</strong>
      <div class="meta">${esc(d?.kind ?? "")}${s.state !== "pending" ? ` · ${esc(s.state)}` : ""}</div>
      <div class="meta">${stepOutput(s, d)}</div></div></li>`;
  }).join("");

  return `<section><div class="card">
      <div class="line"><h3 class="grow">${esc(recipeTitle(r.recipe_id))}</h3>${runChip(r)}</div>
      <div class="meta">Started by ${esc(r.started_by)} · ${esc(ago(r.created_at))}</div>
      ${r.interventions ? `<div class="meta">${esc(interventionText(r.interventions))}</div>` : ""}
      ${(r.authorized_by || []).length ? `<div class="meta">Scans under ${esc(r.authorized_by.map((g) => `a grant from ${g.granted_by}`).join(", "))}</div>` : ""}
      ${(r.interventions?.corrections || []).map((c) => `<div class="note">Corrected “${esc(c.field)}” in step “${esc(c.step_id)}”${c.delta != null ? `: ${c.delta > 0 ? "+" : ""}${esc(c.delta)}` : ""} (proposed ${esc(typeof c.proposed === "object" ? JSON.stringify(c.proposed) : c.proposed)}, given ${esc(typeof c.given === "object" ? JSON.stringify(c.given) : c.given)})</div>`).join("")}
      ${r.problem ? `<div class="problem">${esc(problemText(r.problem))}</div>` : ""}
    </div></section>
    ${panel ? `<section>${panel}</section>` : ""}
    <h2>Steps</h2><section class="card"><ul class="steps">${steps}</ul></section>
    ${!final && mayAdvance(r) ? `<button class="btn danger wide" data-act="cancel-run" data-id="${esc(r.run_id)}" ${S.busy ? "disabled" : ""}>Cancel run</button>` : ""}`;
}

function picker(key, scanId, { readonly = false } = {}) {
  const p = preview(scanId);
  const draw = readonly ? false : S.drawMode[key] ?? true;
  const b = S.boxes[key];
  const dims = S.dims[scanId];
  let img;
  if (p === "loading") img = `<div class="placeholder">Loading preview…</div>`;
  else if (p?.error) img = `<div class="problem">${esc(p.error)}</div>`;
  else {
    const box = b && dims ? `<div class="boxsel" style="left:${(b.x0 / (2 * dims[0])) * 100}%;top:${(b.y0 / dims[1]) * 100}%;width:${(span(b, dims[0]) / (2 * dims[0])) * 100}%;height:${((b.y1 - b.y0) / dims[1]) * 100}%"></div>` : "";
    img = `<div class="pano" data-keep-scroll="${esc(key)}"><div class="pano-in ${draw ? "draw" : ""}" data-picker="${esc(key)}" data-scan="${esc(scanId)}"><img src="${esc(p)}" alt="Scan preview" data-dims="${esc(scanId)}"><img src="${esc(p)}" alt="" aria-hidden="true" class="seam">${box}</div></div>`;
  }
  if (readonly) return `${img}<p class="hint">${b && dims ? `The agent's box, ${span(b, dims[0])} × ${b.y1 - b.y0} px. Scroll to see all of it.` : ""}</p>`;
  return `<div class="pano-tools"><span class="seg"><button type="button" class="${draw ? "on" : ""}" data-act="draw" data-key="${esc(key)}" data-on="1">Draw box</button><button type="button" class="${draw ? "" : "on"}" data-act="draw" data-key="${esc(key)}" data-on="0">Pan</button></span>
    ${b ? `<button type="button" class="btn small" data-act="clear-box" data-key="${esc(key)}">Clear</button>` : ""}</div>
    ${img}
    <p class="hint">${b && dims ? `Box ${span(b, dims[0])} × ${b.y1 - b.y0} px${b.x1 < b.x0 ? ", across the panorama's seam" : ""}.` : draw ? "Drag on the picture to draw a box around what to scan. The panorama repeats after its end, so a box can cross the seam. Switch to Pan to scroll." : "Swipe to scroll the panorama, then switch to Draw box."}</p>`;
}

function viewScans() {
  const opts = S.projects.map((p) => `<option value="${esc(p.project_id)}" ${p.project_id === S.project ? "selected" : ""}>${esc(p.name)}</option>`).join("");
  const head = `<div class="card">
    <label class="field" style="margin-top:0"><span>Project</span>
      <select data-change="project"><option value="">Choose a project…</option>${opts}</select></label>
    ${canAct() ? `<div class="btns"><button class="btn" data-act="sheet-project">New project</button>
      <button class="btn primary" data-act="sheet-scan" ${S.project ? "" : "disabled"}>New scan</button></div>` : ""}
    ${S.project ? grantCard() : ""}
    ${S.project && S.scans.length ? `<a class="btn wide" href="#/view/${encodeURIComponent([...S.scans].sort(byNewest)[0].scan_id)}">View the latest scan, and follow new ones</a>` : ""}</div>`;
  if (!S.project) return head;
  const stations = new Map();
  for (const s of S.scans) {
    const k = s.station ?? 0;
    if (!stations.has(k)) stations.set(k, { dome: null, areas: [] });
    if (s.kind === "area") stations.get(k).areas.push(s); else stations.get(k).dome = s;
  }
  const keys = [...stations.keys()].sort((a, b) => b - a);
  const row = (s, label) => `<a class="card link" href="#/scan/${encodeURIComponent(s.scan_id)}">
      <div class="line"><strong>${esc(label)}</strong><span class="meta">${esc(s.mode ?? "")}</span></div>
      <div class="meta">${num(s.point_count)} points${s.created_at ? ` · ${esc(ago(s.created_at))}` : ""}</div></a>`;
  const list = keys.length ? keys.map((k) => {
    const st = stations.get(k);
    return `<div class="station">${st.dome ? row(st.dome, `Station ${k} · full dome`) : `<div class="meta">Station ${k}</div>`}
      ${st.areas.sort((a, b) => String(a.created_at).localeCompare(String(b.created_at))).map((a, i) => `<div class="subscan">${row(a, `Area scan ${i + 1}`)}</div>`).join("")}</div>`;
  }).join("") : `<p class="empty">No scans in this project yet.</p>`;
  return head + `<h2>Scans</h2><section>${list}</section>`;
}

// Standing permission for agents to scan in the chosen project: given once at
// setup, so the agent's runs need no approval per scan or per run.
const GRANT_HOURS = 4, GRANT_SCANS = 12;
function grantCard() {
  const now = Date.now();
  const gs = S.grants.filter((g) => g.project_id === S.project && !g.run_id && Date.parse(g.expires_at) > now);
  const list = gs.map((g) => `<div class="line"><span class="meta">Agents may scan here until ${esc(clock(g.expires_at))} (${g.max_scans == null ? "any number" : `${g.max_scans - g.scans_used} of ${g.max_scans} left`}), from ${esc(g.granted_by)}</span>
    ${isOp() ? `<button class="btn small" data-act="revoke-grant" data-id="${esc(g.grant_id)}" ${S.busy ? "disabled" : ""}>Revoke</button>` : ""}</div>`).join("");
  if (!gs.length && !isOp()) return `<p class="meta grantbox">An operator can let agents scan here.</p>`;
  return `<div class="grantbox">${list || `<p class="meta">Agents need an approval for each run's scans in this project.</p>`}
    ${isOp() && !gs.length ? `<button class="btn wide" data-act="grant-project" ${S.busy ? "disabled" : ""}>Let agents scan here for ${GRANT_HOURS} hours (up to ${GRANT_SCANS} scans)</button>` : ""}</div>`;
}

function viewScan(id) {
  const s = S.scans.find((x) => x.scan_id === id) ?? S.scanById[id];
  if (!s) {
    if (!viewScan.tried?.has(id)) {
      viewScan.tried = viewScan.tried ?? new Set();
      viewScan.tried.add(id);
      api(`/scans/${encodeURIComponent(id)}`)
        .then((v) => { S.scanById[v.scan_id] = v; render(); })
        .catch((e) => { viewScan.tried.delete(id); toast(e.message, true); });
    }
    return `<p class="empty">Loading scan…</p>`;
  }
  const key = `aim:${id}`;
  const aim = S.aim[id];
  const partialOf = S.jobs.find((j) => j.partial_scan_id === id);
  const exportJobId = S.watchExport[id];
  const exportJob = exportJobId ? S.jobs.find((j) => j.job_id === exportJobId) : null;
  const exportId = exportJob?.result?.export_id;
  const ex = exportId ? S.exports[exportId] : null;
  const modeOpts = S.modes.map((m) => `<option value="${esc(m.mode)}" ${m.mode === (store.get("x9.mode") || "STD_L0") ? "selected" : ""}>${esc(m.mode)}</option>`).join("");

  const title = s.kind === "area" ? `Station ${s.station} · area scan` : `Station ${s.station} · full dome`;
  return `<section><div class="card">
      <h3>${esc(title)}</h3>
      <div class="meta">${esc(s.mode ?? "")} · ${num(s.point_count)} points${s.created_at ? ` · ${esc(clock(s.created_at))}` : ""}${s.window ? ` · window ${deg(s.window.delta_ha)} × ${deg(s.window.delta_va)}` : ""}</div>
      ${partialOf ? `<div class="note">This is the partial sweep of a cancelled scan.</div>` : ""}
      <a class="btn primary wide" style="margin-top:12px" href="#/view/${encodeURIComponent(id)}">View the points</a>
    </div></section>
    ${(() => { const p = S.nextStations.find((x) => x.scan_id === id); return p ? `<h2>Next station from here</h2><section>${nextStationCard(p)}</section>` : ""; })()}
    <h2>Aim an area scan</h2>
    <section class="card">
      ${picker(key, id)}
      ${canAct() ? `<div class="btns"><button class="btn" data-act="aim" data-id="${esc(id)}" ${S.boxes[key] && !S.busy ? "" : "disabled"}>Work out the window</button></div>` : ""}
      ${aim ? `<div class="facts"><div class="fact"><b>Across</b><span>${deg(aim.window.delta_ha)}</span></div><div class="fact"><b>Up and down</b><span>${deg(aim.window.delta_va)}</span></div></div>
        <label class="field"><span>Scan mode</span><select name="mode" data-change="mode">${modeOpts}</select></label>
        <button class="btn primary wide" data-act="scan-area" data-id="${esc(id)}" ${S.busy ? "disabled" : ""}>Scan this area</button>` : ""}
    </section>
    <div class="cols even"><div>
    <h2>Measure</h2>
    <section class="card">
      ${(() => {
        const jid = S.watchMeasure[id];
        const mj = jid ? S.jobs.find((j) => j.job_id === jid) : null;
        const mid = mj?.result?.measurement_id;
        if (mid && !S.measurements[mid]) loadMeasurement(mid);
        return (mj && mj.state !== "succeeded" ? jobCard(mj) : "") + (mid ? measurementCard(S.measurements[mid]) : `<p class="hint">Measures a window's finished opening from a close-up with wall on every side.</p>`);
      })()}
      ${canAct() ? `<div class="btns"><button class="btn" data-act="measure" data-id="${esc(id)}" ${S.busy ? "disabled" : ""}>Measure the opening</button></div>` : ""}
    </section>
    </div><div>
    <h2>Points</h2>
    <section class="card">
      ${exportJob && exportJob.state !== "succeeded" ? jobCard(exportJob) : ""}
      ${ex && !ex.error ? `<div class="line"><strong>${esc(ex.format?.toUpperCase())} · ${num(ex.point_count)} points</strong></div>
        <ul class="steps">${(ex.checks || []).map((c) => `<li><span class="dot ${c.ok ? "done" : "failed"}">${c.ok ? "✓" : "×"}</span><div class="grow"><strong>${esc(CHECKS[c.name] ?? c.name)}</strong><div class="meta">${esc(c.detail ?? "")}</div></div></li>`).join("")}</ul>
        <button class="btn primary wide" data-act="dl-export" data-id="${esc(exportId)}">Download ${esc(ex.format?.toUpperCase())}</button>` : ""}
      ${canAct() ? `<div class="btns"><button class="btn" data-act="export" data-id="${esc(id)}" data-format="ply" ${S.busy ? "disabled" : ""}>Export PLY</button>
        <button class="btn" data-act="export" data-id="${esc(id)}" data-format="las" ${S.busy ? "disabled" : ""}>Export LAS</button>
        <button class="btn" data-act="export" data-id="${esc(id)}" data-format="copc" ${S.busy ? "disabled" : ""}>Export COPC</button></div>` : ""}
      <div class="btns"><button class="btn" data-act="dl-tzf" data-id="${esc(id)}">Download raw .tzf</button></div>
    </section>
    </div></div>
    ${isOp() ? `<h2>Delete</h2><section><button class="btn danger wide" data-act="delete-scan" data-id="${esc(id)}" ${S.busy ? "disabled" : ""}>Delete this scan from the scanner</button></section>` : ""}`;
}

function viewLog() {
  const form = canAct() ? `<form class="card" data-form="note">
      <label class="field" style="margin-top:0"><span>Add a note</span><textarea name="text" maxlength="2000" placeholder="What you see, what you changed, anything worth remembering"></textarea></label>
      <button class="btn primary wide" ${S.busy ? "disabled" : ""}>Add to the log</button></form>` : "";
  const entries = S.log.length ? S.log.map((e) => `<div class="entry"><div class="meta"><span class="chip">${esc(e.kind)}</span><span>${esc(clock(e.at))}</span><span class="ellip">${esc(e.author)}</span></div><div>${esc(e.text)}</div></div>`).join("") : `<p class="empty">Nothing logged yet.</p>`;
  return `${form}<h2>Field log</h2><section class="card">${entries}</section>`;
}

function viewSheet() {
  const sh = S.sheet;
  if (!sh) return "";
  let body = "";
  if (sh.type === "scan") {
    const proj = S.projects.map((p) => `<option value="${esc(p.project_id)}" ${p.project_id === S.project ? "selected" : ""}>${esc(p.name)}</option>`).join("");
    const modes = S.modes.map((m) => `<option value="${esc(m.mode)}" ${m.mode === (store.get("x9.mode") || "STD_L0") ? "selected" : ""}>${esc(m.mode)} · about ${Math.round((m.full_dome_seconds ?? 0) / 60)} min</option>`).join("");
    body = `<h3>New full-dome scan</h3>
      <form data-form="scan">
        <label class="field"><span>Project</span><select name="project_id" required><option value="">Choose…</option>${proj}</select></label>
        <label class="field"><span>Mode</span><select name="mode" required>${modes}</select></label>
        <label class="check"><input type="checkbox" name="color"> Colour pictures (adds a few minutes)</label>
        <label class="field"><span>Note (optional)</span><input type="text" name="note" maxlength="200"></label>
        ${isOp() ? "" : `<p class="hint">Your scans wait for an operator's approval.</p>`}
        <div class="btns"><button type="button" class="btn" data-act="close">Cancel</button><button class="btn primary" ${S.busy ? "disabled" : ""}>Start scan</button></div>
      </form>`;
  } else if (sh.type === "run") {
    const rec = S.recipe[sh.id];
    if (!rec) recipe(sh.id);
    const proj = S.projects.map((p) => `<option value="${esc(p.project_id)}" ${p.project_id === S.project ? "selected" : ""}>${esc(p.name)}</option>`).join("");
    const inputs = typeof rec === "object" && rec.inputs ? rec.inputs.map((i) => i.choices
      ? `<label class="field"><span>${esc(i.prompt ?? i.name)}</span><select name="in:${esc(i.name)}" required><option value="">Choose…</option>${i.choices.map((c) => `<option>${esc(c)}</option>`).join("")}</select></label>`
      : `<label class="field"><span>${esc(i.prompt ?? i.name)}</span><input type="text" name="in:${esc(i.name)}" ${i.type === "number" ? 'inputmode="decimal"' : ""} required></label>`).join("") : "";
    body = `<h3>${esc(typeof rec === "object" ? rec.title ?? sh.id : sh.id)}</h3>
      <p class="meta">${esc(typeof rec === "object" ? rec.summary ?? "" : "Loading…")}</p>
      <form data-form="run" data-id="${esc(sh.id)}">
        <label class="field"><span>Project</span><select name="project_id" required><option value="">Choose…</option>${proj}</select></label>
        ${inputs}
        <div class="btns"><button type="button" class="btn" data-act="close">Cancel</button><button class="btn primary" ${S.busy || typeof rec !== "object" ? "disabled" : ""}>Start</button></div>
      </form>`;
  } else if (sh.type === "project") {
    body = `<h3>New project</h3>
      <form data-form="project">
        <label class="field"><span>Name</span><input type="text" name="name" maxlength="64" required placeholder="e.g. 2026-10-21 kitchen window"></label>
        <div class="btns"><button type="button" class="btn" data-act="close">Cancel</button><button class="btn primary" ${S.busy ? "disabled" : ""}>Create</button></div>
      </form>`;
  } else if (sh.type === "spot") {
    const why = ["Blocked: a vehicle, gate, fence or building", "Not safe: traffic, a drop-off, machinery", "Ground too soft, steep or uneven", "Not allowed there", "Other"];
    body = `<h3>Can't stand at spot ${esc(sh.rank)}</h3>
      <p class="meta">The agent reads this and finds another spot.</p>
      <form data-form="spot" data-id="${esc(sh.station)}|${esc(sh.rank)}">
        <label class="field"><span>Why</span><select name="why" required><option value="">Choose…</option>${why.map((w) => `<option>${esc(w)}</option>`).join("")}</select></label>
        <label class="field"><span>Anything else (optional)</span><input type="text" name="detail" maxlength="200" placeholder="e.g. delivery truck parked there until noon"></label>
        <div class="btns"><button type="button" class="btn" data-act="close">Cancel</button><button class="btn primary" ${S.busy ? "disabled" : ""}>Rule it out</button></div>
      </form>`;
  } else if (sh.type === "me") {
    body = `<h3>${esc(S.me?.name ?? "")}</h3>
      <p class="meta">Signed in as ${esc(S.me?.role ?? "")}. ${isOp() ? "You can approve scans and delete them." : S.me?.role === "agent" ? "Your scans wait for an operator's approval." : "You can look but not change anything."}</p>
      <div class="btns"><button class="btn" data-act="close">Close</button><button class="btn danger" data-act="signout">Sign out</button></div>`;
  }
  return `<div class="scrim" data-act="scrim"><div class="sheet" role="dialog" aria-modal="true">${body}</div></div>`;
}

function viewMain() {
  const r = S.route;
  if (r.run) return viewRun(r.run);
  if (r.scan) return viewScan(r.scan);
  return { now: viewNow, runs: viewRuns, scans: viewScans, log: viewLog }[r.tab]();
}

// Re-rendering replaces the page, so hold it while someone is typing or
// drawing, and keep panorama scroll positions.
let pending = false;
let drawing = null;
function render(force = false) {
  const root = document.getElementById("app");
  const active = document.activeElement;
  const typing = active && root.contains(active) && /^(INPUT|TEXTAREA|SELECT)$/.test(active.tagName) && active.type !== "checkbox";
  if (!force && (typing || drawing)) { pending = true; return; }
  if (!force) {
    if (render.queued) return;
    render.queued = true;
    requestAnimationFrame(() => { render.queued = false; render(true); });
    return;
  }
  pending = false;
  // Form fields keep what was typed or picked, and focus, across redraws.
  const fieldKey = (el) => { const f = el.closest("form[data-form]"); return f && el.name ? [f.dataset.form, f.dataset.id, f.dataset.run, f.dataset.step, el.name].join("|") : null; };
  const fields = {};
  // Only what a person changed is kept; a field still showing what the page
  // put there takes the new page's value (e.g. an agent's proposal arriving).
  root.querySelectorAll("form[data-form] [name]").forEach((el) => {
    const k = fieldKey(el);
    if (!k) return;
    const changed = el.type === "checkbox" ? el.checked !== el.defaultChecked : el.value !== el.defaultValue;
    if (changed || el.tagName === "SELECT") fields[k] = el.type === "checkbox" ? el.checked : el.value;
  });
  const focusKey = document.activeElement && root.contains(document.activeElement) ? fieldKey(document.activeElement) : null;
  const scroll = {};
  root.querySelectorAll("[data-keep-scroll]").forEach((el) => { scroll[el.dataset.keepScroll] = el.scrollLeft; });
  if (!S.token || !S.me) {
    if (!root.querySelector("[data-form=signin]") || S.signinError !== render.lastErr || S.busy !== render.lastBusy) root.innerHTML = viewSignin();
    render.lastErr = S.signinError;
    render.lastBusy = S.busy;
    return;
  }
  root.innerHTML = viewTop() + `<main>${viewMain()}</main>` + viewSheet() +
    (S.toast ? `<div class="toast ${S.toast.bad ? "bad" : ""}" role="status">${esc(S.toast.text)}</div>` : "");
  root.querySelectorAll("[data-keep-scroll]").forEach((el) => {
    const k = el.dataset.keepScroll;
    const pick = el.querySelector("[data-picker]");
    const b = S.boxes[k];
    const dims = pick && S.dims[pick.dataset.scan];
    // Bring a box into view once, as soon as the picture's size is known
    // (e.g. the agent's proposal at the far end); after that keep the
    // person's own scrolling.
    if (b && dims && S.boxShown[k] !== S.boxFrom[k] + "|" + b.x0) {
      S.boxShown[k] = S.boxFrom[k] + "|" + b.x0;
      if (!S.userDrew[k]) {
        const centre = (b.x0 + span(b, dims[0]) / 2) / (2 * dims[0]);
        el.scrollLeft = Math.max(0, centre * pick.scrollWidth - el.clientWidth / 2);
        return;
      }
    }
    if (scroll[k] != null) el.scrollLeft = scroll[k];
  });
  // A spots picture opens on spot 1; after that it keeps the person's scrolling.
  root.querySelectorAll(".spots-pic").forEach((el) => {
    const k = el.dataset.keepScroll;
    if (S.spotScrolled[k]) return;
    const img = el.querySelector("img");
    const go = () => {
      const sp = el.querySelector(".spot");
      const badge = el.querySelector(".badge circle");
      const svg = el.querySelector("svg.overlay");
      const x = sp ? sp.offsetLeft : badge && svg ? Number(badge.getAttribute("cx")) / svg.viewBox.baseVal.width * svg.clientWidth : null;
      if (x == null) return;
      S.spotScrolled[k] = true;
      el.scrollLeft = Math.max(0, x - el.clientWidth / 2);
    };
    if (img.complete && img.naturalWidth) go(); else img.addEventListener("load", go, { once: true });
  });
  root.querySelectorAll("form[data-form] [name]").forEach((el) => {
    const k = fieldKey(el);
    if (k == null || !(k in fields)) return;
    if (el.type === "checkbox") el.checked = fields[k]; else el.value = fields[k];
    if (k === focusKey) el.focus({ preventScroll: true });
  });
  syncViewer();
  root.querySelectorAll("img[data-dims]").forEach((img) => {
    const set = () => {
      const id = img.dataset.dims;
      const d = S.dims[id];
      if (!d || d[0] !== img.naturalWidth) { S.dims[id] = [img.naturalWidth, img.naturalHeight]; render(); }
    };
    if (img.complete && img.naturalWidth) set(); else img.addEventListener("load", set, { once: true });
  });
}
document.addEventListener("focusout", () => setTimeout(() => { if (pending) render(); }, 0));
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if (S.sheet) { S.sheet = null; render(true); }
  else if (S.route.view) location.hash = `#/scan/${encodeURIComponent(S.route.scan)}`;
});

// ----------------------------------------------------------------- viewer ---
let viewer = null;
function syncViewer() {
  const r = S.route;
  if (!r.view || !S.me) { viewer?.close(); return; }
  if (!viewer) {
    viewer = createViewer(document.getElementById("viewer"), {
      fetchRaw: (path) => api(path, { raw: true }),
      fetchJson: (path) => api(path),
      onClose: () => { location.hash = `#/scan/${encodeURIComponent(S.route.scan)}`; },
    });
  }
  const scan = S.scans.find((x) => x.scan_id === r.scan) ?? S.scanById[r.scan];
  viewer.open(r.scan, scan);
  // A scan in progress in the same project, for the live line.
  const pid = scan?.project_id;
  const j = pid && S.jobs.find((x) => x.kind === "scan" && x.request?.project_id === pid && ["queued", "awaiting_approval", "running"].includes(x.state));
  viewer.setLive(j ? `${describeJob(j)}: ${j.state === "running" ? (PHASES[j.phase] ?? "running") : j.state.replace("_", " ")}${viewer.follow ? " · opens here when it's done" : ""}` : "");
}

// ---------------------------------------------------------------- drawing ---
function pointIn(el, e, dims) {
  // The picker shows the panorama twice: x runs over 0 … 2 × width.
  const r = el.getBoundingClientRect();
  const x = Math.round(((e.clientX - r.left) / r.width) * 2 * dims[0]);
  const y = Math.round(((e.clientY - r.top) / r.height) * dims[1]);
  return [Math.max(0, Math.min(2 * dims[0], x)), Math.max(0, Math.min(dims[1], y))];
}
document.addEventListener("pointerdown", (e) => {
  const el = e.target.closest("[data-picker]");
  if (!el || !el.classList.contains("draw")) return;
  const dims = S.dims[el.dataset.scan];
  if (!dims) return;
  e.preventDefault();
  el.setPointerCapture(e.pointerId);
  const start = pointIn(el, e, dims);
  let box = el.querySelector(".boxsel");
  if (!box) { box = document.createElement("div"); box.className = "boxsel"; el.append(box); }
  drawing = { el, key: el.dataset.picker, dims, start, end: start, box };
});
document.addEventListener("pointermove", (e) => {
  if (!drawing) return;
  drawing.end = pointIn(drawing.el, e, drawing.dims);
  const [w, h] = drawing.dims;
  const [a, b] = [drawing.start, drawing.end];
  Object.assign(drawing.box.style, {
    left: `${(Math.min(a[0], b[0]) / (2 * w)) * 100}%`, top: `${(Math.min(a[1], b[1]) / h) * 100}%`,
    width: `${(Math.min(Math.abs(a[0] - b[0]), w - 1) / (2 * w)) * 100}%`, height: `${(Math.abs(a[1] - b[1]) / h) * 100}%`,
  });
});
function endDraw() {
  if (!drawing) return;
  const { key, start: a, end: b, dims } = drawing;
  drawing = null;
  if (Math.abs(a[0] - b[0]) >= 4 && Math.abs(a[1] - b[1]) >= 4) {
    // Back to preview pixels: x0 in [0, width); a box past the end wraps (x1 < x0).
    const W = dims[0];
    let lo = Math.min(a[0], b[0]);
    let hi = Math.min(Math.max(a[0], b[0]), lo + W - 1);
    if (lo >= W) { lo -= W; hi -= W; }
    const x1 = hi > W ? hi - W : hi;
    S.boxes[key] = { x0: lo, y0: Math.min(a[1], b[1]), x1, y1: Math.max(a[1], b[1]) };
    S.userDrew[key] = true;
    const scanId = key.startsWith("aim:") ? key.slice(4) : null;
    if (scanId) delete S.aim[scanId];
  }
  render(true);
}
document.addEventListener("pointerup", endDraw);
document.addEventListener("pointercancel", endDraw);

// ---------------------------------------------------------------- actions ---
const actions = {
  back: () => history.length > 1 ? history.back() : (location.hash = `#/${S.route.tab}`),
  me: () => { S.sheet = { type: "me" }; render(true); },
  close: () => { S.sheet = null; render(true); },
  scrim: (el, e) => { if (e.target === el) { S.sheet = null; render(true); } },
  signout: () => { S.sheet = null; signOut(); },
  "sheet-scan": () => { S.sheet = { type: "scan" }; load("projects"); render(true); },
  "sheet-run": (el) => { S.sheet = { type: "run", id: el.dataset.id }; load("projects"); render(true); },
  "sheet-project": () => { S.sheet = { type: "project" }; render(true); },
  "mission-sign-off": (el) => {
    if (!confirm("Sign off this job? Do it only once you've checked the ledger: nothing more will be added to it.")) return;
    return act(() => api(`/projects/${encodeURIComponent(el.dataset.id)}/mission/sign-off`, { method: "POST", body: { confirmed_via: "web UI" } })
      .then((m) => { S.missions = [m, ...S.missions.filter((x) => x.project_id !== m.project_id)]; }), "Job signed off");
  },
  ask: (el) => {
    const pid = el.dataset.pid, tid = el.dataset.id, key = `${pid}|${tid}`;
    S.asking[key] = true;
    render(true);
    api(`/projects/${encodeURIComponent(pid)}/mission/targets/${encodeURIComponent(tid)}/ask`, { method: "POST" })
      .then((r) => toast(r.verdict === "answer" ? "Answered" : r.verdict === "does_not_fit" ? "Doesn't fit: see why" : "Not yet: see why"))
      .catch((e) => { if (e.status !== 401) toast(e.message, true); })
      .finally(() => { delete S.asking[key]; load("missions"); });
  },
  "spot-use": (el) => {
    const plan = S.plans[latestDome(el.dataset.pid)?.scan_id];
    const c = plan?.candidates?.find((x) => String(x.rank) === el.dataset.rank);
    const text = `Next station from station ${el.dataset.station}: setting up at spot ${el.dataset.rank}${c ? ` (${spotWords(c)})` : ""}.`;
    return act(() => logNote(text, el.dataset.pid), `Spot ${el.dataset.rank} chosen`);
  },
  more: () => { S.showMore = !S.showMore; store.set("x9.more", S.showMore ? "1" : null); render(true); },
  "other-spots": () => { S.otherSpots = !S.otherSpots; render(true); },
  "q-open": (el) => { const k = el.dataset.key; S.qOpen[k] = !S.qOpen[k]; render(true); },
  "spot-focus": (el) => { const r = Number(el.dataset.rank); S.spotFocus = S.spotFocus === r ? null : r; render(true); },
  "spot-no": (el) => { S.sheet = { type: "spot", pid: el.dataset.pid, station: el.dataset.station, rank: el.dataset.rank }; render(true); },
  bumped: (el) => act(() => logNote(`Station ${el.dataset.station}: the person says the tripod ${el.dataset.said} during the scan.`, el.dataset.pid), "Noted for the agent"),
  "approve-run": (el) => act(() => api(`/jobs/${encodeURIComponent(el.dataset.id)}/approve`, { method: "POST", body: { run: true } }).then((j) => { upsert(S.jobs, j, "job_id"); load("grants"); }), "Run approved"),
  "grant-project": () => act(async () => {
    const expires = new Date(Date.now() + GRANT_HOURS * 3600e3).toISOString().replace(/\.\d+Z$/, "Z");
    await api("/grants", { method: "POST", body: { project_id: S.project, expires_at: expires, max_scans: GRANT_SCANS } });
    await load("grants");
  }, "Agents may scan here"),
  "revoke-grant": (el) => act(async () => { await api(`/grants/${encodeURIComponent(el.dataset.id)}`, { method: "DELETE" }); await load("grants"); }, "Permission revoked"),
  approve: (el) => act(() => api(`/jobs/${encodeURIComponent(el.dataset.id)}/approve`, { method: "POST" }).then((j) => upsert(S.jobs, j, "job_id")), "Approved"),
  "cancel-job": (el) => {
    const j = S.jobs.find((x) => x.job_id === el.dataset.id);
    const running = j?.state === "running";
    if (running && !confirm("Stop this scan? The scanner keeps the part it has swept.")) return;
    return act(() => api(`/jobs/${encodeURIComponent(el.dataset.id)}/cancel`, { method: "POST" }).then((j2) => upsert(S.jobs, j2, "job_id")), running ? "Stopping" : "Denied");
  },
  "cancel-run": (el) => {
    if (!confirm("Cancel this run? A scan in progress is stopped too.")) return;
    return act(() => api(`/runs/${encodeURIComponent(el.dataset.id)}/cancel`, { method: "POST" }).then((r) => upsertRun(r)), "Cancelling");
  },
  draw: (el) => { S.drawMode[el.dataset.key] = el.dataset.on === "1"; render(true); },
  "clear-box": (el) => { delete S.boxes[el.dataset.key]; S.userDrew[el.dataset.key] = true; render(true); },
  aim: (el) => {
    const id = el.dataset.id;
    const box = S.boxes[`aim:${id}`];
    return act(async () => { S.aim[id] = await api("/aim", { method: "POST", body: { reference_scan_id: id, box } }); });
  },
  "scan-area": (el) => {
    const id = el.dataset.id;
    const s = S.scans.find((x) => x.scan_id === id) ?? S.scanById[id];
    const mode = document.querySelector("select[data-change=mode]")?.value || store.get("x9.mode") || "STD_L0";
    return act(async () => {
      const j = await api("/scans", { method: "POST", body: { project_id: s.project_id, kind: "area", mode, window: S.aim[id].window, note: `area from station ${s.station}` } });
      upsert(S.jobs, j, "job_id");
      location.hash = "#/now";
    }, isOp() ? "Area scan queued" : "Area scan sent for approval");
  },
  export: (el) => act(async () => {
    const j = await api(`/scans/${encodeURIComponent(el.dataset.id)}/exports`, { method: "POST", body: { format: el.dataset.format, intensity: "auto" } });
    upsert(S.jobs, j, "job_id");
    S.watchExport[el.dataset.id] = j.job_id;
  }),
  measure: (el) => act(async () => {
    const j = await api(`/scans/${encodeURIComponent(el.dataset.id)}/measurements`, { method: "POST", body: { kind: "opening" } });
    upsert(S.jobs, j, "job_id");
    S.watchMeasure[el.dataset.id] = j.job_id;
  }),
  "dl-export": (el, e) => { e.preventDefault(); return act(() => download(`/exports/${encodeURIComponent(el.dataset.id)}/file`, `${el.dataset.id}`)); },
  "dl-tzf": (el) => act(() => download(`/scans/${encodeURIComponent(el.dataset.id)}/tzf`, `${el.dataset.id}.tzf`)),
  "delete-scan": (el) => {
    const id = el.dataset.id;
    const s = S.scans.find((x) => x.scan_id === id) ?? S.scanById[id];
    if (!confirm(`Delete station ${s?.station ?? ""} ${s?.kind === "area" ? "area scan" : "full dome"} from the scanner? This can't be undone.`)) return;
    return act(async () => {
      await api(`/scans/${encodeURIComponent(id)}`, { method: "DELETE", headers: { "X-Confirm": id } });
      S.scans = S.scans.filter((x) => x.scan_id !== id);
      location.hash = "#/scans";
    }, "Scan deleted");
  },
};

document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-act]");
  if (!el || el.disabled) return;
  const fn = actions[el.dataset.act];
  if (fn) fn(el, e);
});

document.addEventListener("change", (e) => {
  const el = e.target;
  if (el.dataset.change === "project") {
    S.project = el.value || null;
    store.set("x9.project", S.project);
    S.scans = [];
    load("scans");
  } else if (el.dataset.change === "mode") {
    store.set("x9.mode", el.value);
  }
});

const forms = {
  signin: async (f) => { S.busy = true; render(true); await signIn(f.token.value); S.busy = false; render(true); },
  scan: (f) => {
    store.set("x9.mode", f.mode.value);
    return act(async () => {
      const body = { project_id: f.project_id.value, kind: "full_dome", mode: f.mode.value, color: f.color.checked };
      if (f.note.value.trim()) body.note = f.note.value.trim();
      const j = await api("/scans", { method: "POST", body });
      upsert(S.jobs, j, "job_id");
      S.sheet = null;
      location.hash = "#/now";
    }, isOp() ? "Scan queued" : "Scan sent for approval");
  },
  run: (f) => act(async () => {
    const inputs = {};
    for (const el of f.elements) {
      if (!el.name?.startsWith("in:")) continue;
      const v = el.value.trim();
      inputs[el.name.slice(3)] = el.inputMode === "decimal" && v !== "" && !isNaN(Number(v)) ? Number(v) : v;
    }
    const r = await api("/runs", { method: "POST", body: { recipe_id: f.dataset.id, project_id: f.project_id.value, inputs } });
    upsertRun(r);
    S.sheet = null;
    location.hash = `#/run/${encodeURIComponent(r.run_id)}`;
  }, "Run started"),
  advance: (f) => act(async () => {
    const input = {};
    for (const el of f.elements) {
      if (!el.name) continue;
      const v = el.value.trim();
      let parsed = v;
      if (v !== "" && !isNaN(Number(v))) parsed = Number(v);
      else if (/^[[{]/.test(v)) { try { parsed = JSON.parse(v); } catch { /* keep the text */ } }
      input[el.name] = parsed;
    }
    const box = S.boxes[f.dataset.key];
    if (box) input.box = box;
    const r = await api(`/runs/${encodeURIComponent(f.dataset.run)}/advance`, { method: "POST", body: { step_id: f.dataset.step, input } });
    upsertRun(r);
  }, "Sent"),
  project: (f) => act(async () => {
    const p = await api("/projects", { method: "POST", body: { name: f.name.value.trim() } });
    await load("projects");
    S.project = p.project_id;
    store.set("x9.project", S.project);
    S.sheet = null;
    load("scans");
  }, "Project created"),
  spot: (f) => {
    const sh = S.sheet;
    const why = [f.why.value, f.detail.value.trim()].filter(Boolean).join(": ");
    return act(async () => {
      await logNote(`Next station from station ${sh.station}: can't stand at spot ${sh.rank}: ${why}.`, sh.pid);
      S.sheet = null;
    }, `Spot ${sh.rank} ruled out`);
  },
  note: (f) => act(async () => {
    const body = { text: f.text.value.trim() };
    if (S.project) body.project_id = S.project;
    const e = await api("/log", { method: "POST", body });
    if (e && !S.log.some((x) => x.entry_id === e.entry_id)) S.log.unshift(e);
    f.text.value = "";
  }, "Noted"),
};

document.addEventListener("submit", (e) => {
  const f = e.target.closest("[data-form]");
  if (!f) return;
  e.preventDefault();
  if (S.busy) return;
  forms[f.dataset.form]?.(f);
});

window.addEventListener("hashchange", () => {
  S.route = parseRoute();
  S.sheet = null;
  if (S.route.tab === "scans" && !S.route.scan) { load("projects"); load("scans"); }
  window.scrollTo(0, 0);
  render(true);
});
document.addEventListener("visibilitychange", () => { if (!document.hidden && S.me) load("status"); });

// ------------------------------------------------------------------- boot ---
async function boot() {
  // A token can arrive in the link (?token=…), e.g. from a QR code; it is then
  // kept in this browser and removed from the address bar.
  const q = new URLSearchParams(location.search);
  const t = q.get("token");
  if (t) {
    history.replaceState(null, "", location.pathname + location.hash);
    S.token = t;
  }
  S.route = parseRoute();
  if (S.token) {
    if (!(await signIn(S.token))) return;
  } else {
    render(true);
  }
  setInterval(() => { if (S.me && !document.hidden) { load("status"); load("workers"); } }, 5000);
  setInterval(() => { if (S.me && S.jobs.some((j) => j.state === "running")) render(); }, 1000);
}
boot();
