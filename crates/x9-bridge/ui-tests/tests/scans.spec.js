import { test, expect, waitJob, domeScan, dragOn } from "../support/fixtures.js";

test.describe("scans and grants", () => {
  test("the operator lets agents scan a project, then revokes it", async ({ openAs, api, project }) => {
    const pid = project.project_id;
    const op = await openAs("operator", { project: pid, hash: "#/scans" });
    await expect(op.getByLabel("Project")).toHaveValue(pid);
    await expect(op.getByText("Agents need an approval for each run's scans in this project.")).toBeVisible();
    await op.getByRole("button", { name: "Let agents scan here for 4 hours (up to 12 scans)" }).click();
    await expect(op.getByRole("status")).toHaveText("Agents may scan here");
    await expect(op.getByText(/Agents may scan here until .* \(12 of 12 left\), from operator-1/)).toBeVisible();

    // Under the grant, the agent's scan goes ahead without asking.
    const job = await api.agent.post("/scans", { project_id: pid, kind: "full_dome", mode: "STD_L0" });
    expect(job.state).toBe("queued");
    await waitJob(api, job.job_id, ["succeeded"]);
    await op.reload();
    await expect(op.getByText(/\(11 of 12 left\)/)).toBeVisible();
    await expect(op.getByRole("link", { name: /Station 1 · full dome/ })).toBeVisible();

    await op.getByRole("button", { name: "Revoke" }).click();
    await expect(op.getByRole("status")).toHaveText("Permission revoked");
    await expect(op.getByText("Agents need an approval for each run's scans in this project.")).toBeVisible();
    const agent = await openAs("agent", { project: pid, hash: "#/scans" });
    await expect(agent.getByText("An operator can let agents scan here.")).toBeVisible();
  });

  test("an agent makes a new project and switches between projects", async ({ openAs, project }) => {
    const agent = await openAs("agent", { hash: "#/scans" });
    await agent.getByRole("button", { name: "New project" }).click();
    const name = `ui-new-${Date.now()}`;
    await agent.getByRole("dialog").getByLabel("Name").fill(name);
    await agent.getByRole("dialog").getByRole("button", { name: "Create" }).click();
    await expect(agent.getByRole("status")).toHaveText("Project created");
    await expect(agent.getByLabel("Project").locator("option:checked")).toHaveText(name);
    await expect(agent.getByText("No scans in this project yet.")).toBeVisible();
    await expect(agent.getByRole("button", { name: "New scan" })).toBeEnabled();
    await agent.getByLabel("Project").selectOption(project.project_id);
    await expect(agent.getByText("No scans in this project yet.")).toBeVisible();
    await agent.getByLabel("Project").selectOption("");
    await expect(agent.getByRole("button", { name: "New scan" })).toBeDisabled();
    await expect(agent.getByRole("heading", { name: "Scans", exact: true, level: 2 })).toHaveCount(0);
  });

  test("a scan's page: aim, export, measure and download need the scan file; the operator deletes it", async ({ openAs, api, project }) => {
    const scan = await domeScan(api, project);
    const op = await openAs("operator", { project: project.project_id, hash: "#/scans" });
    await op.getByRole("link", { name: /Station 1 · full dome/ }).click();
    await expect(op.getByRole("heading", { name: "Station 1 · full dome" })).toBeVisible();
    await expect(op.locator(".card").first()).toContainText("STD_L0");

    // Aim: draw a box on the preview (Draw is the default), clear it, draw again.
    const pic = op.getByRole("img", { name: "Scan preview" });
    await expect(pic).toBeVisible();
    await expect(op.getByRole("button", { name: "Work out the window" })).toBeDisabled();
    const drag = (x0, y0, x1, y1) => dragOn(op, pic, [x0, y0], [x1, y1]);
    await drag(0.5, 0.3, 0.7, 0.7);
    await expect(op.getByText(/^Box \d+ × \d+ px\.$/)).toBeVisible();
    await op.getByRole("button", { name: "Clear" }).click();
    await expect(op.getByText(/^Drag on the picture to draw a box/)).toBeVisible();
    await op.getByRole("button", { name: "Pan" }).click();
    await expect(op.getByText("Swipe to scroll the panorama, then switch to Draw box.")).toBeVisible();
    await op.getByRole("button", { name: "Draw box" }).click();
    await drag(0.55, 0.3, 0.75, 0.6);
    await op.getByRole("button", { name: "Work out the window" }).click();
    await expect(op.locator(".toast.bad")).toBeVisible();

    // Export and measure: each starts a job, shown on the page; without a scan
    // file it fails at once.
    const started = (what) => op.waitForResponse((r) => r.request().method() === "POST" && new URL(r.url()).pathname.endsWith(what)).then((r) => r.json());
    let reply = started("/exports");
    await op.getByRole("button", { name: "Export PLY" }).click();
    const exportJob = await reply;
    await expect(op.locator(".card", { hasText: /^\s*Export/ }).locator(".chip")).toBeVisible();
    reply = started("/measurements");
    await op.getByRole("button", { name: "Measure the opening" }).click();
    const measureJob = await reply;
    await expect(op.locator(".card", { hasText: "Opening measurement" }).locator(".chip")).toBeVisible();
    for (const j of [exportJob, measureJob]) expect((await waitJob(api, j.job_id, ["failed", "succeeded"])).state).toBe("failed");
    // Download: a toast, no file.
    await op.getByRole("button", { name: "Download raw .tzf" }).click();
    await expect(op.locator(".toast.bad")).toBeVisible();

    op.once("dialog", (d) => { expect(d.message()).toContain("Delete station 1 full dome from the scanner?"); d.accept(); });
    await op.getByRole("button", { name: "Delete this scan from the scanner" }).click();
    await expect(op.getByRole("status")).toHaveText("Scan deleted");
    await expect(op).toHaveURL(/#\/scans$/);
    await expect.poll(async () => (await api.viewer.get(`/scans?project_id=${project.project_id}`)).length).toBe(0);
    await expect(op.getByText("No scans in this project yet.")).toBeVisible();
    // A link to a scan that is gone says so (after a reload: the page keeps
    // every scan it has seen, so a link opened before reloading still shows it).
    await op.goto(`/ui/#/scan/${encodeURIComponent(scan.scan_id)}`);
    await op.reload();
    await expect(op.getByText("Loading scan…")).toBeVisible();
    await expect(op.locator(".toast.bad")).toBeVisible();
  });

  test("a viewer's scan page has no actions", async ({ openAs, api, project }) => {
    const scan = await domeScan(api, project);
    const viewer = await openAs("viewer", { project: project.project_id, hash: `#/scan/${encodeURIComponent(scan.scan_id)}` });
    await expect(viewer.getByRole("heading", { name: "Station 1 · full dome" })).toBeVisible();
    for (const name of ["Work out the window", "Measure the opening", "Export PLY", "Delete this scan from the scanner"]) {
      await expect(viewer.getByRole("button", { name })).toHaveCount(0);
    }
    await expect(viewer.getByRole("button", { name: "Download raw .tzf" })).toBeVisible();
  });
});

test.describe("point viewer", () => {
  test("opens, shows why it can't draw without a scan file, and its controls work", async ({ openAs, api, project }) => {
    const scan = await domeScan(api, project);
    const op = await openAs("operator", { project: project.project_id, hash: "#/scans" });
    await op.getByRole("link", { name: "View the latest scan, and follow new ones" }).click();
    await expect(op).toHaveURL(new RegExp(`#/view/${scan.scan_id}`));
    const vw = op.locator("#viewer");
    await expect(vw).toBeVisible();
    await expect(vw.locator("[data-vw-title]")).toHaveText("Station 1 · full dome");
    await expect(vw.locator("[data-vw-status]")).toHaveText(/Could not load the points|can't draw the points/);

    const seg = vw.getByRole("group", { name: "Colour" });
    for (const m of ["Height", "Coverage", "Intensity"]) {
      await seg.getByRole("button", { name: m }).click();
      await expect(seg.getByRole("button", { name: m })).toHaveClass(/\bon\b/);
    }
    await vw.getByRole("button", { name: "Plan" }).click();
    await expect(vw.getByRole("button", { name: "Plan" })).toHaveClass(/\bon\b/);
    await vw.getByRole("button", { name: "No ceiling" }).click();
    await expect(vw.getByRole("button", { name: "No ceiling" })).not.toHaveClass(/\bon\b/);
    await vw.getByRole("button", { name: "From scanner" }).click();
    await vw.getByRole("button", { name: "Overview" }).click();
    await vw.getByRole("slider").fill("4");
    // Drag, shift-drag and wheel on the canvas.
    const c = await vw.locator("canvas").boundingBox();
    await op.mouse.move(c.x + c.width / 2, c.y + c.height / 2);
    await op.mouse.down();
    await op.mouse.move(c.x + c.width / 2 + 40, c.y + c.height / 2 + 20, { steps: 3 });
    await op.mouse.up();
    await op.keyboard.down("Shift");
    await op.mouse.down();
    await op.mouse.move(c.x + c.width / 2, c.y + c.height / 2, { steps: 3 });
    await op.mouse.up();
    await op.keyboard.up("Shift");
    await op.mouse.wheel(0, 200);
    // Preferences are kept in this browser.
    expect(await op.evaluate(() => [localStorage.getItem("x9.view.size"), localStorage.getItem("x9.view.plan")])).toEqual(["4", "true"]);

    await vw.getByRole("button", { name: "‹ Back" }).click();
    await expect(op).toHaveURL(new RegExp(`#/scan/${scan.scan_id}`));
    await expect(vw).toBeHidden();
    await op.getByRole("link", { name: "View the points" }).click();
    await expect(vw).toBeVisible();
    await op.keyboard.press("Escape");
    await expect(vw).toBeHidden();
  });

  test("follows the next scan in its project", async ({ openAs, api, project }) => {
    const first = await domeScan(api, project);
    const op = await openAs("operator", { project: project.project_id, hash: `#/view/${encodeURIComponent(first.scan_id)}` });
    const vw = op.locator("#viewer");
    await expect(vw.getByRole("checkbox", { name: "Follow new scans" })).toBeChecked();
    const job = await api.agent.post("/scans", { project_id: project.project_id, kind: "full_dome", mode: "STD_L0" });
    await expect(vw.locator("[data-vw-live]")).toContainText("Full-dome scan · STD_L0: awaiting approval · opens here when it's done");
    await api.operator.post(`/jobs/${job.job_id}/approve`);
    const done = await waitJob(api, job.job_id, ["succeeded"]);
    await expect(op).toHaveURL(new RegExp(`#/view/${done.result.scan_id}`));
    await expect(vw.locator("[data-vw-title]")).toHaveText("Station 2 · full dome");
    // Unticked, it stays put.
    await vw.getByRole("checkbox", { name: "Follow new scans" }).uncheck();
    expect(await op.evaluate(() => localStorage.getItem("x9.view.follow"))).toBe("false");
  });
});

// The page upserts the job from the POST reply. A job that fails at once
// (no scan file) sends its "failed" event before that reply arrives, and the
// older "running" copy from the reply then replaces it, with no later event to
// correct it: the page shows the export running forever. Runs guard against
// this (upsertRun); jobs don't.
test("an export that fails at once shows as failed", async ({ openAs, api, project }) => {
  const scan = await domeScan(api, project);
  const op = await openAs("operator", { project: project.project_id, hash: `#/scan/${encodeURIComponent(scan.scan_id)}` });
  await op.getByRole("button", { name: "Export PLY" }).click();
  await expect(op.locator(".card", { hasText: /^\s*Export/ }).locator(".chip")).toHaveText("failed");
  await expect(op.locator(".card", { hasText: /^\s*Export/ }).locator(".problem")).toContainText("The scanner refused the request");
});
