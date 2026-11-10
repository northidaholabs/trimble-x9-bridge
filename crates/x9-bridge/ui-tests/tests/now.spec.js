import { test, expect, waitJob } from "../support/fixtures.js";

test.describe("Now", () => {
  test("shows the scanner's live status from the mock", async ({ openAs, api }) => {
    const status = await api.viewer.get("/status");
    const page = await openAs("viewer");
    const card = page.locator(".card.status");
    await expect(card).toContainText(`${status.scanner.battery_pct}%`);
    await expect(card).toContainText("battery");
    await expect(card.locator(".fact", { hasText: "Level" })).toContainText(status.scanner.leveling.state);
    await expect(card.locator(".fact", { hasText: "Moved since last scan" })).toContainText(/yes|no/);
    await expect(card.locator(".fact", { hasText: "Storage free" })).toContainText("GB");
    await expect(card.locator(".fact", { hasText: "Bridge" })).toContainText(status.bridge.version);
    await expect(card.locator(".fact", { hasText: "Heavy work" })).toContainText("on the Pi");
    await expect(page.getByRole("heading", { name: "Needs you" })).toBeVisible();
    // Calm by default: the four tabs, nothing else to choose from.
    for (const tab of ["Now", "Runs", "Scans", "Log"]) await expect(page.getByRole("link", { name: tab })).toBeVisible();
  });

  test("an agent's scan waits for approval; the operator approves it and watches it run to done", async ({ openAs, api, project }) => {
    const op = await openAs("operator", { project: project.project_id });
    const agentPage = await openAs("agent", { project: project.project_id });

    // The agent asks for a scan from its chat (the API).
    const job = await api.agent.post("/scans", { project_id: project.project_id, kind: "full_dome", mode: "STD_L0", note: "station 1 by the door" });
    expect(job.state).toBe("awaiting_approval");

    // The operator sees it under Needs you, with a count on the Now tab.
    const card = op.locator(".card", { hasText: "Full-dome scan" }).filter({ hasText: "station 1 by the door" });
    await expect(card).toContainText("needs approval");
    await expect(card).toContainText(project.name);
    await expect(card).toContainText("by agent-1");
    await expect(op.getByRole("link", { name: /Now\s*1/ })).toBeVisible();
    // The agent sees it waiting, with no buttons.
    const agentCard = agentPage.locator(".card", { hasText: "station 1 by the door" });
    await expect(agentCard).toContainText("Waiting for an operator to approve.");
    await expect(agentCard.getByRole("button")).toHaveCount(0);

    await card.getByRole("button", { name: "Approve" }).click();
    await expect(op.getByRole("status")).toHaveText("Approved");
    // It runs under Now, with its phase, then lands under Recent.
    const running = op.locator("section", { has: op.locator(".bar") }).locator(".card", { hasText: "station 1 by the door" });
    await expect(running.locator(".chip")).toHaveText(/queued|running/);
    await expect(running).toContainText(/Queued|Levelling|Scanning|Waiting for the scanner|Downloading/);
    await expect(op.getByRole("heading", { name: "Recent" })).toBeVisible({ timeout: 20_000 });
    const done = op.locator(".card", { hasText: "station 1 by the door" }).filter({ hasText: "succeeded" });
    await expect(done).toBeVisible({ timeout: 20_000 });
    await expect(op.getByText("Nothing needs you.")).toBeVisible();
    // The agent's page follows over the event stream too.
    await expect(agentPage.locator(".card", { hasText: "station 1 by the door" }).filter({ hasText: "succeeded" })).toBeVisible();

    await done.getByRole("link", { name: "Open scan" }).click();
    await expect(op.getByRole("heading", { name: "Station 1 · full dome" })).toBeVisible();
    await op.getByRole("button", { name: "‹ Back" }).click();
    await expect(op.getByRole("heading", { name: "Now", exact: true })).toBeVisible();
  });

  test("the operator denies an agent's scan", async ({ openAs, api, project }) => {
    const op = await openAs("operator", { project: project.project_id });
    const job = await api.agent.post("/scans", { project_id: project.project_id, kind: "full_dome", mode: "STD_L0", note: "deny me" });
    const card = op.locator(".card", { hasText: "deny me" });
    await card.getByRole("button", { name: "Deny" }).click();
    await expect(op.getByRole("status")).toHaveText("Denied");
    expect((await api.viewer.get(`/jobs/${job.job_id}`)).state).toBe("cancelled");
    await expect(op.locator(".card", { hasText: "deny me" })).toContainText("cancelled");
  });

  test("a scan started from the sheet runs, and Stop cancels it", async ({ openAs, api, project }) => {
    const op = await openAs("operator", { project: project.project_id });
    await op.getByRole("button", { name: "New full-dome scan" }).click();
    const sheet = op.getByRole("dialog");
    await expect(sheet.getByRole("heading", { name: "New full-dome scan" })).toBeVisible();
    await expect(sheet.getByLabel("Project")).toHaveValue(project.project_id);
    await sheet.getByLabel("Mode").selectOption("STD_L0");
    await sheet.getByLabel("Note (optional)").fill("stop me");
    await sheet.getByRole("button", { name: "Start scan" }).click();
    await expect(op.getByRole("status")).toHaveText("Scan queued");
    await expect(sheet).toHaveCount(0);

    const card = op.locator(".card", { hasText: "stop me" });
    await expect(card.locator(".chip")).toHaveText("running", { timeout: 15_000 });
    op.once("dialog", (d) => d.accept());
    await card.getByRole("button", { name: "Stop" }).click();
    await expect(op.getByRole("status")).toHaveText("Stopping");
    await expect(op.locator(".card", { hasText: "stop me" }).locator(".chip")).toHaveText("cancelled", { timeout: 15_000 });
    const jobs = await api.viewer.get("/jobs");
    expect(jobs.find((j) => j.request?.note === "stop me").state).toBe("cancelled");
  });

  test("an agent's own scan from the sheet goes for approval", async ({ openAs, api, project }) => {
    const agent = await openAs("agent", { project: project.project_id });
    await agent.getByRole("button", { name: "New full-dome scan" }).click();
    const sheet = agent.getByRole("dialog");
    await expect(sheet).toContainText("Your scans wait for an operator's approval.");
    await sheet.getByLabel("Colour pictures").check();
    await sheet.getByLabel("Note (optional)").fill("agent from the sheet");
    await sheet.getByRole("button", { name: "Start scan" }).click();
    await expect(agent.getByRole("status")).toHaveText("Scan sent for approval");
    const card = agent.locator(".card", { hasText: "agent from the sheet" });
    await expect(card).toContainText("colour");
    await expect(card).toContainText("needs approval");
    // Tidy up: the operator denies it through the API.
    const job = (await api.viewer.get("/jobs")).find((j) => j.request?.note === "agent from the sheet");
    await api.operator.post(`/jobs/${job.job_id}/cancel`);
    await waitJob(api, job.job_id, ["cancelled"]);
    await expect(card).toContainText("cancelled");
  });
});
