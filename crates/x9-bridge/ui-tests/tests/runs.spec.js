import { test, expect, waitRun, dragOn } from "../support/fixtures.js";

const atStep = (step) => (r) => r.state === "waiting_for_person" && r.current_step_id === step;

test.describe("recipes and runs", () => {
  test("the operator starts a recipe from the Runs tab and answers its steps", async ({ openAs, api, project }) => {
    const op = await openAs("operator", { project: project.project_id, hash: "#/runs" });
    await expect(op.getByRole("heading", { name: "Start a recipe" })).toBeVisible();
    await expect(op.getByRole("button", { name: /Measure a window/ })).toContainText("v0.4.0");
    await op.getByRole("button", { name: /Walk-round check/ }).click();

    const sheet = op.getByRole("dialog");
    await expect(sheet).toContainText("Name the side, count its windows");
    await sheet.getByLabel("Project").selectOption(project.project_id);
    await sheet.getByLabel("Which side of the building?").selectOption("back");
    await sheet.getByLabel("How many floors?").fill("2");
    await sheet.getByRole("button", { name: "Start" }).click();
    await expect(op.getByRole("status")).toHaveText("Run started");
    await expect(op).toHaveURL(/#\/run\//);
    const runId = decodeURIComponent(op.url().split("#/run/")[1]);
    expect((await api.viewer.get(`/runs/${runId}`)).started_by).toBe("operator-1");
    await waitRun(api, runId, atStep("label"));

    // The first step is the agent's to decide; a person can still answer it.
    await expect(op.getByRole("heading", { name: "Name the side" }).first()).toBeVisible();
    await expect(op.locator(".chip", { hasText: "agent's turn" }).first()).toBeVisible();
    await expect(op.getByText("The agent decides this step and goes on")).toBeVisible();
    await op.getByText("Answer it yourself instead").click();
    await op.getByLabel("label").fill("the street");
    await op.getByRole("button", { name: "Continue with my answer" }).click();
    await expect(op.getByRole("status")).toHaveText("Sent");

    // The next step waits for a person, and shows what was decided before it.
    await expect(op.getByRole("heading", { name: "Count the windows" }).first()).toBeVisible();
    await expect(op.getByText("Waiting for the agent to propose an answer.")).toBeVisible();
    await expect(op.getByText("Name the side: decided by the agent")).toBeVisible();
    await expect(op.locator(".review")).toContainText("label: the street");
    await op.getByLabel("windows").fill("4");
    await op.getByRole("button", { name: "Continue", exact: true }).click();
    await expect(op.locator(".card").first().locator(".chip")).toHaveText("succeeded");
    await expect(op.locator("ul.steps li")).toHaveCount(3);
    await expect(op.locator("ul.steps .dot.done")).toHaveCount(3);
    await expect(op.locator(".card").first()).toContainText(/person intervention/);
    expect((await api.viewer.get(`/log?project_id=${project.project_id}`)).some((e) => e.text === "Walk-round back: 4 windows")).toBe(true);

    // Listed under Runs, and Back returns there.
    await op.getByRole("button", { name: "‹ Back" }).click();
    await expect(op.getByRole("heading", { name: "Runs", exact: true })).toBeVisible();
    await expect(op.getByRole("link", { name: /Walk-round check/ }).first()).toContainText("by operator-1");
  });

  test("an agent proposes an answer; the operator checks it from Needs you", async ({ openAs, api, project }) => {
    const run = await api.agent.post("/runs", { recipe_id: "walk-round", project_id: project.project_id, inputs: { side: "front", floors: 1 } });
    await waitRun(api, run.run_id, atStep("label"));
    await api.agent.post(`/runs/${run.run_id}/advance`, { step_id: "label", input: { label: "the yard" } });
    await waitRun(api, run.run_id, atStep("count"));
    await api.agent.post(`/runs/${run.run_id}/propose`, { step_id: "count", input: { windows: 6 }, note: "six on the drone photo." });

    const op = await openAs("operator", { project: project.project_id });
    const need = op.getByRole("link", { name: /Walk-round check/ }).filter({ hasText: "agent-1 proposed an answer: six on the drone photo. Check it." });
    await expect(need).toBeVisible();
    await expect(op.getByRole("link", { name: /^Now\s*\d+/ })).toBeVisible();
    await need.click();
    await expect(op.getByRole("heading", { name: "Check agent-1's answer" })).toBeVisible();
    await expect(op.getByText("Confirm it, or adjust it first.")).toBeVisible();
    await expect(op.getByLabel("windows")).toHaveValue("6");
    await op.getByRole("button", { name: "Looks right" }).click();
    await expect(op.getByRole("status")).toHaveText("Sent");
    await waitRun(api, run.run_id, (r) => r.state === "succeeded");
    await expect(op.locator(".card").first().locator(".chip")).toHaveText("succeeded");
  });

  test("a run's scan is approved for the whole run; a box drawn by hand goes to the aim step", async ({ openAs, api, project }) => {
    const run = await api.agent.post("/runs", { recipe_id: "window-measurement", project_id: project.project_id, inputs: { purpose: "blinds" } });
    await waitRun(api, run.run_id, (r) => r.waiting_for === "approval");
    const op = await openAs("operator", { project: project.project_id, hash: `#/run/${encodeURIComponent(run.run_id)}` });
    await expect(op.getByRole("heading", { name: "Measure a window" })).toBeVisible();
    await expect(op.getByText("Part of a recipe run.")).toBeVisible();
    await expect(op.getByRole("button", { name: "Only this scan" })).toBeVisible();
    await op.getByRole("button", { name: "Approve the run" }).click();
    await expect(op.getByRole("status")).toHaveText("Run approved");

    // The overview scan runs, then the box step is the agent's turn.
    await expect(op.locator(".chip", { hasText: "agent's turn" }).first()).toBeVisible({ timeout: 25_000 });
    await expect(op.getByRole("link", { name: "Open scan" })).toBeVisible();
    await op.getByText("Answer it yourself instead").click();
    const pic = op.getByRole("img", { name: "Scan preview" });
    await expect(pic).toBeVisible();
    await expect(op.getByRole("button", { name: "Continue with my answer" })).toBeDisabled();
    await dragOn(op, pic, [0.5, 0.3], [0.65, 0.7]);
    // The fold stays open through the redraw that drawing causes.
    const fold = op.locator("details", { hasText: "Answer it yourself instead" });
    await expect(fold).toHaveJSProperty("open", true);
    await expect(op.getByText(/^Box \d+ × \d+ px\.$/)).toBeVisible();
    await op.getByRole("button", { name: "Continue with my answer" }).click();
    await expect(op.getByRole("status")).toHaveText("Sent");
    // With no scan file to read, the aim step fails and the run says why.
    await expect(op.locator(".card").first().locator(".chip")).toHaveText("failed", { timeout: 20_000 });
    await expect(op.locator(".card").first().locator(".problem")).toBeVisible();
    await expect(op.getByRole("button", { name: "Cancel run" })).toHaveCount(0);
  });

  test("the operator cancels an agent's run", async ({ openAs, api, project }) => {
    const run = await api.agent.post("/runs", { recipe_id: "walk-round", project_id: project.project_id, inputs: { side: "front", floors: 3 } });
    await waitRun(api, run.run_id, atStep("label"));
    const op = await openAs("operator", { project: project.project_id, hash: `#/run/${encodeURIComponent(run.run_id)}` });
    op.once("dialog", (d) => d.accept());
    await op.getByRole("button", { name: "Cancel run" }).click();
    await expect(op.getByRole("status")).toHaveText("Cancelling");
    await waitRun(api, run.run_id, (r) => r.state === "cancelled");
    await expect(op.locator(".card").first().locator(".chip")).toHaveText("cancelled");
  });

  test("an agent starts a run from the page; a viewer can only look", async ({ openAs, project }) => {
    const agent = await openAs("agent", { project: project.project_id, hash: "#/runs" });
    await agent.getByRole("button", { name: /^Note/ }).click();
    const sheet = agent.getByRole("dialog");
    await sheet.getByLabel("Project").selectOption(project.project_id);
    await sheet.getByLabel("What to note").fill("Gate code is on the van");
    await sheet.getByRole("button", { name: "Start" }).click();
    await expect(agent.getByRole("status")).toHaveText("Run started");
    await expect(agent.locator(".card").first().locator(".chip")).toHaveText("succeeded");

    const viewer = await openAs("viewer", { hash: "#/runs" });
    await expect(viewer.getByRole("button", { name: /Walk-round check/ })).toBeDisabled();
    await viewer.getByRole("link", { name: /^Note/ }).first().click();
    await expect(viewer.getByRole("button", { name: "Cancel run" })).toHaveCount(0);
    await viewer.goto("/ui/#/run/no-such-run");
    await expect(viewer.getByText("This run isn't on the bridge")).toBeVisible();
  });
});

// Every redraw (the box drawn, a toast, an event) rebuilds the page, and the
// <details> fold is not kept open: a person answering the agent's step sees
// the picture and the Continue button vanish as soon as they draw.
test("\"Answer it yourself instead\" stays open while the person draws a box", async ({ openAs, api, project }) => {
  const run = await api.agent.post("/runs", { recipe_id: "window-measurement", project_id: project.project_id, inputs: { purpose: "trim" } });
  await waitRun(api, run.run_id, (r) => r.waiting_for === "approval");
  const op = await openAs("operator", { project: project.project_id, hash: `#/run/${encodeURIComponent(run.run_id)}` });
  await op.getByRole("button", { name: "Approve the run" }).click();
  await expect(op.locator(".chip", { hasText: "agent's turn" }).first()).toBeVisible({ timeout: 25_000 });
  await op.getByText("Answer it yourself instead").click();
  await dragOn(op, op.getByRole("img", { name: "Scan preview" }), [0.5, 0.3], [0.6, 0.6]);
  await expect(op.getByRole("button", { name: "Continue with my answer" })).toBeVisible();
});

// upsertRun keeps the newer of two copies by progress, but "running" and
// "waiting_for_person" with no step done weigh the same, so the reply to
// POST /runs (state running) replaces the "waiting" event that beat it: the
// page shows the run running, with no step to answer, until a reload. Made
// certain here by holding the reply until the run waits.
test("a run started from the page shows its first step even when the reply is slow", async ({ openAs, api, project }) => {
  const op = await openAs("operator", { project: project.project_id, hash: "#/runs" });
  await op.route("**/v1/runs", async (route) => {
    if (route.request().method() !== "POST") return route.fallback();
    const r = await route.fetch();
    const run = await r.json();
    await waitRun(api, run.run_id, atStep("label"));
    await route.fulfill({ response: r, json: run });
  });
  await op.getByRole("button", { name: /Walk-round check/ }).click();
  const sheet = op.getByRole("dialog");
  await sheet.getByLabel("Project").selectOption(project.project_id);
  await sheet.getByLabel("Which side of the building?").selectOption("front");
  await sheet.getByLabel("How many floors?").fill("1");
  await sheet.getByRole("button", { name: "Start" }).click();
  await expect(op.getByRole("status")).toHaveText("Run started");
  await expect(op.getByText("Answer it yourself instead")).toBeVisible();
});
