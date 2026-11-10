// Fixtures: the bridge's API as each role (for what an agent does from its
// chat), and pages signed in as a role, each recording JS coverage.

import { test as base, expect } from "@playwright/test";
import { TOKENS } from "./tokens.js";
import { save } from "./coverage.js";

export { expect };

function client(baseURL, who) {
  const call = async (method, path, body, headers = {}) => {
    const r = await fetch(`${baseURL}/v1${path}`, {
      method,
      headers: { Authorization: `Bearer ${TOKENS[who]}`, ...(body === undefined ? {} : { "Content-Type": "application/json" }), ...headers },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await r.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { json = text; }
    return { status: r.status, body: json };
  };
  // Calls that must work: anything but 2xx fails the test with the problem.
  const ok = (method) => async (path, body, headers) => {
    const r = await call(method, path, body, headers);
    if (r.status < 200 || r.status > 299) throw new Error(`${who} ${method} ${path}: ${r.status} ${JSON.stringify(r.body)}`);
    return r.body;
  };
  return { call, get: ok("GET"), post: ok("POST"), put: ok("PUT"), del: ok("DELETE") };
}

let projectSeq = 0;

// V8 keeps a module's counts only while its page lives: a reload or a new
// document drops them. So coverage is taken (and started afresh) before each
// goto or reload, and once more when the test ends.
async function recordCoverage(page, label) {
  await page.coverage.startJSCoverage({ resetOnNavigation: false });
  const take = async () => save(await page.coverage.stopJSCoverage(), label);
  for (const name of ["goto", "reload"]) {
    const orig = page[name].bind(page);
    page[name] = async (...args) => {
      if (page.url() !== "about:blank") {
        await take();
        await page.coverage.startJSCoverage({ resetOnNavigation: false });
      }
      return orig(...args);
    };
  }
  return take;
}

export const test = base.extend({
  // The bridge global-setup started.
  baseURL: async ({}, use) => { await use(process.env.X9_UI_BASE); },

  // The default page also records coverage.
  page: async ({ page }, use, testInfo) => {
    const done = await recordCoverage(page, testInfo.titlePath.join(" "));
    await use(page);
    await done();
  },

  /** api.agent.post("/scans", {...}) and so on: the API as each role. */
  api: async ({ baseURL }, use) => {
    await use(Object.fromEntries(Object.keys(TOKENS).map((who) => [who, client(baseURL, who)])));
  },

  /** A fresh project for this test (the bridge is shared by every test). */
  project: async ({ api }, use, testInfo) => {
    const name = `ui-${testInfo.title.toLowerCase().replace(/[^a-z0-9]+/g, "-").slice(0, 32)}-${process.pid}-${++projectSeq}`;
    await use(await api.agent.post("/projects", { name }));
  },

  /**
   * A page in its own browser context, signed in through `?token=` as a role.
   * `project` preselects the project (as the person's last choice would),
   * `hash` the route, `viewport` the screen.
   */
  openAs: async ({ browser, baseURL }, use, testInfo) => {
    const opened = [];
    await use(async (who, { hash = "", project, viewport } = {}) => {
      const context = await browser.newContext({ baseURL, ...(viewport ? { viewport } : {}) });
      if (project) await context.addInitScript((id) => { try { localStorage.setItem("x9.project", id); } catch { /* none */ } }, project);
      const page = await context.newPage();
      opened.push({ context, done: await recordCoverage(page, testInfo.titlePath.join(" ")) });
      await page.goto(`/ui/?token=${encodeURIComponent(TOKENS[who] ?? who)}${hash}`);
      if (TOKENS[who]) await expect(page.getByText("Live", { exact: true })).toBeVisible();
      return page;
    });
    for (const { context, done } of opened) {
      await done();
      await context.close();
    }
  },
});

/** Wait (through the API) until a job reaches one of `states`; returns it. */
export async function waitJob(api, jobId, states, timeout = 30_000) {
  let job;
  await expect.poll(async () => (job = await api.viewer.get(`/jobs/${jobId}`)).state, { timeout, intervals: [100, 200] })
    .toMatch(new RegExp(`^(${states.join("|")})$`));
  return job;
}

/** Wait until a run satisfies `pred`; returns it. */
export async function waitRun(api, runId, pred, timeout = 30_000) {
  let run;
  await expect.poll(async () => pred(run = await api.viewer.get(`/runs/${runId}`)), { timeout, intervals: [100, 200] }).toBe(true);
  return run;
}

/** A finished full-dome scan in `project`, made by the operator through the API. */
export async function domeScan(api, project) {
  const job = await api.operator.post("/scans", { project_id: project.project_id, kind: "full_dome", mode: "STD_L0" });
  const done = await waitJob(api, job.job_id, ["succeeded", "failed", "cancelled"]);
  expect(done.state, JSON.stringify(done.problem)).toBe("succeeded");
  return api.viewer.get(`/scans/${done.result.scan_id}`);
}

/**
 * Drag across an element from (x0, y0) to (x1, y1), as fractions of its size.
 * The page redraws on every event, so the element is looked up until it
 * holds still long enough to measure; once the pointer is down the page
 * holds its redraws until it is up.
 */
export async function dragOn(page, locator, [x0, y0], [x1, y1]) {
  let b;
  await expect(async () => { b = await locator.boundingBox(); expect(b).not.toBeNull(); }).toPass({ timeout: 5_000 });
  await page.mouse.move(b.x + b.width * x0, b.y + b.height * y0);
  await page.mouse.down();
  await page.mouse.move(b.x + b.width * x1, b.y + b.height * y1, { steps: 5 });
  await page.mouse.up();
}
