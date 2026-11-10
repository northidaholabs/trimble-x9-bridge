import { test, expect, domeScan } from "../support/fixtures.js";

const GOAL = "Scan the outside of the workshop";

test.describe("mission", () => {
  test("an agent's mission shows with its ledger; ready, then the operator signs it off", async ({ openAs, api, project }) => {
    const pid = project.project_id;
    const scan = await domeScan(api, project);
    // The bridge registers the new station by itself and then judges every
    // question. Let that finish first, so the question below starts unasked.
    await expect.poll(async () => {
      const reg = await api.viewer.get(`/projects/${pid}/registration`);
      return !reg.registering_job_id && !String(reg.summary ?? "").startsWith("Not registered yet");
    }, { timeout: 30_000 }).toBe(true);
    await api.agent.put(`/projects/${pid}/mission`, {
      goal: GOAL, kind: "exterior", done_when: "Every facade seen from two stations",
      targets: [
        { target_id: "north", name: "North facade", kind: "facade" },
        { target_id: "east", name: "East wall (by the gate)", kind: "wall", face: { from: [0, 4], to: [6, 4] },
          question: "plumb", tolerance: { words: "1/4 in in 10 ft", mm_per_m: 2 } },
      ],
    });
    const op = await openAs("operator", { project: pid });
    const agentPage = await openAs("agent", { project: pid });

    const card = op.locator(".card.mission");
    await expect(op.getByRole("heading", { name: "Mission" })).toBeVisible();
    await expect(card).toContainText(GOAL);
    await expect(card).toContainText("in progress");
    await expect(card).toContainText("Done when: Every facade seen from two stations");
    await expect(card).toContainText("2 open");
    await expect(card.locator("li", { hasText: "North facade" })).toContainText("open");
    // The map of the project, drawn by the bridge.
    await expect(op.getByRole("heading", { name: "Map" })).toBeVisible();
    await expect(op.getByRole("img", { name: "Line map of the walls found and the stations so far" })).toBeVisible();

    // A question: one line, its detail one tap away.
    const q = card.locator("li.question");
    await expect(q).toContainText("not asked");
    const line = q.getByRole("button", { name: /East wall · not asked yet/ });
    await expect(line).toHaveAttribute("aria-expanded", "false");
    await line.click();
    await expect(q.getByRole("button", { name: /East wall/ })).toHaveAttribute("aria-expanded", "true");
    await expect(q).toContainText("Not asked yet.");
    // Asking needs placed stations; without them the bridge says why, in a toast.
    await q.getByRole("button", { name: "Ask now" }).click();
    await expect(op.getByRole("status")).toBeVisible({ timeout: 20_000 });
    await expect(card.locator("li.question").getByRole("button", { name: /Ask (now|again)/ })).toBeEnabled({ timeout: 20_000 });
    await card.locator("li.question").getByRole("button", { name: /East wall/ }).click();
    await expect(card.locator("li.question").getByRole("button", { name: /East wall/ })).toHaveAttribute("aria-expanded", "false");

    // The agent settles the ledger and says it's ready to leave.
    await api.agent.post(`/projects/${pid}/mission/targets`, { targets: [
      { target_id: "north", status: "covered", stations: [scan.scan_id] },
      { target_id: "east", status: "declined", note: "behind a locked fence" },
    ] });
    await expect(card).toContainText("1 covered · 1 declined");
    await expect(card.locator("li", { hasText: "North facade" })).toContainText("covered");
    await expect(card.locator("li", { hasText: "North facade" })).toContainText("1 station");
    await expect(card.locator("li.question")).toContainText("East wall · behind a locked fence");
    await api.agent.post(`/projects/${pid}/mission/ready`, { summary: "North from station 1; east declined (locked fence)." });
    await expect(card).toContainText("ready to leave");
    await expect(card).toContainText("North from station 1; east declined (locked fence).");
    await expect(agentPage.locator(".card.mission")).toContainText("Waiting for an operator to sign it off.");
    await expect(agentPage.locator(".card.mission").getByRole("button", { name: "Sign off the job" })).toHaveCount(0);

    // A dismissed confirm changes nothing.
    op.once("dialog", (d) => d.dismiss());
    await card.getByRole("button", { name: "Sign off the job" }).click();
    await expect(card).toContainText("ready to leave");
    op.once("dialog", (d) => d.accept());
    await card.getByRole("button", { name: "Sign off the job" }).click();
    await expect(op.getByRole("status")).toHaveText("Job signed off");
    await expect(card).toContainText("signed off");
    await expect(card).toContainText("Signed off by operator-1 · just now · web UI");
    await expect(agentPage.locator(".card.mission")).toContainText("Signed off by operator-1");
    expect((await api.viewer.get(`/projects/${pid}/mission`)).state).toBe("signed_off");
  });

  test("the agent's next-station proposal shows on Now, and on its scan", async ({ openAs, api, project }) => {
    const scan = await domeScan(api, project);
    await api.agent.put(`/scans/${scan.scan_id}/next-station`, {
      marker: { x: 300, y: 200, label: "by the gate post" },
      note: "Carry the tripod past the workshop door, to the gate post.",
      labels: [{ x: 260, y: 190, text: "Gate post", kind: "landmark" }],
    });
    const op = await openAs("operator", { project: project.project_id });
    await expect(op.getByRole("heading", { name: "Next station" })).toBeVisible();
    const card = op.locator(".card", { hasText: "Set up the next station" });
    await expect(card).toContainText("Carry the tripod past the workshop door, to the gate post.");
    await expect(card).toContainText("from station 1 · agent-1 · just now");
    await expect(card.getByRole("img", { name: "The station's view, marked with where to set up next" })).toBeVisible();
    await op.goto(`/ui/#/scan/${encodeURIComponent(scan.scan_id)}`);
    await expect(op.getByRole("heading", { name: "Next station from here" })).toBeVisible();
  });
});
