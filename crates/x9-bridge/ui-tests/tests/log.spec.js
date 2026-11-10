import { test, expect } from "../support/fixtures.js";

test.describe("field log", () => {
  test("the operator adds a note, and an agent's note arrives live", async ({ openAs, api, project }) => {
    const op = await openAs("operator", { project: project.project_id, hash: "#/log" });
    await expect(op.getByRole("heading", { name: "Field log" }).first()).toBeVisible();
    const box = op.getByLabel("Add a note");
    await box.fill("Ladder left by the north wall");
    await op.getByRole("button", { name: "Add to the log" }).click();
    await expect(op.getByRole("status")).toHaveText("Noted");
    const entry = op.locator(".entry", { hasText: "Ladder left by the north wall" });
    await expect(entry).toContainText("operator-1");
    const notes = await api.viewer.get(`/log?project_id=${project.project_id}`);
    expect(notes.some((e) => e.text === "Ladder left by the north wall")).toBe(true);

    await api.agent.post("/log", { text: "Agent: station 2 should see the gable", project_id: project.project_id });
    await expect(op.locator(".entry").first()).toContainText("Agent: station 2 should see the gable");
    await expect(op.locator(".entry").first()).toContainText("agent-1");
  });

  test("a viewer reads the log but can't write to it", async ({ openAs }) => {
    const viewer = await openAs("viewer", { hash: "#/log" });
    await expect(viewer.getByRole("heading", { name: "Field log" }).first()).toBeVisible();
    await expect(viewer.getByLabel("Add a note")).toHaveCount(0);
    await expect(viewer.locator(".entry").first()).toBeVisible();
  });
});

// forms.note clears `f.text`, but act() has already redrawn the page, so it
// clears the old, detached form: the new box keeps the note (easy to add twice).
test("the note box is empty after adding a note", async ({ openAs, project }) => {
  const op = await openAs("operator", { project: project.project_id, hash: "#/log" });
  await op.getByLabel("Add a note").fill("Gate left open");
  await op.getByRole("button", { name: "Add to the log" }).click();
  await expect(op.getByRole("status")).toHaveText("Noted");
  await expect(op.getByLabel("Add a note")).toHaveValue("");
});
