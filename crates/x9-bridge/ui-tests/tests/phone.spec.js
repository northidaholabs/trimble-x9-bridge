import { test, expect, domeScan } from "../support/fixtures.js";

const PHONE = { width: 390, height: 844 };
const overflow = (page) => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

test("on a phone (390 × 844) no view scrolls sideways", async ({ openAs, api, project }) => {
  const scan = await domeScan(api, project);
  await api.agent.put(`/projects/${project.project_id}/mission`, {
    goal: "Phone check: the long goal of a mission with a fairly long sentence that has to wrap on a small screen",
    kind: "exterior", done_when: "Every facade seen", targets: [{ target_id: "n", name: "North facade with a long name", kind: "facade" }],
  });
  await api.agent.post("/scans", { project_id: project.project_id, kind: "full_dome", mode: "STD_L0", note: "a long note for the phone check, long enough to wrap" });
  const page = await openAs("operator", { project: project.project_id, viewport: PHONE });

  const views = [
    ["#/now", page.getByRole("heading", { name: "Needs you" })],
    ["#/runs", page.getByRole("heading", { name: "Start a recipe" })],
    ["#/scans", page.getByRole("link", { name: /Station 1 · full dome/ })],
    ["#/log", page.getByLabel("Add a note")],
    [`#/scan/${encodeURIComponent(scan.scan_id)}`, page.getByRole("img", { name: "Scan preview" })],
  ];
  for (const [hash, ready] of views) {
    await page.evaluate((h) => { location.hash = h; }, hash);
    await expect(ready).toBeVisible();
    expect(await overflow(page), `${hash} scrolls sideways`).toBeLessThanOrEqual(0);
  }
  // The tabs stay on screen, and a sheet fits too.
  await page.evaluate(() => { location.hash = "#/now"; });
  for (const tab of ["Now", "Runs", "Scans", "Log"]) await expect(page.getByRole("link", { name: tab })).toBeInViewport();
  await page.getByRole("button", { name: "New full-dome scan" }).click();
  await expect(page.getByRole("dialog").getByRole("button", { name: "Start scan" })).toBeInViewport();
  expect(await overflow(page)).toBeLessThanOrEqual(0);
  await page.keyboard.press("Escape");
  // The viewer, too.
  await page.evaluate((id) => { location.hash = `#/view/${encodeURIComponent(id)}`; }, scan.scan_id);
  await expect(page.locator("#viewer")).toBeVisible();
  await expect(page.locator("#viewer").getByRole("button", { name: "Overview" })).toBeInViewport();
  expect(await overflow(page)).toBeLessThanOrEqual(0);

  // Tidy up the waiting scan.
  const waiting = (await api.viewer.get("/jobs")).filter((j) => j.state === "awaiting_approval" && j.request?.project_id === project.project_id);
  for (const j of waiting) await api.operator.post(`/jobs/${j.job_id}/cancel`);
});
