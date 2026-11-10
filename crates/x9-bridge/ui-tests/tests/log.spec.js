import { test, expect } from "../support/fixtures.js";

// The page redraws as its first loads land. Typing into a box that is being
// replaced loses the text, so type until the live box holds it.
async function typeNote(page, text) {
  const box = page.getByLabel("Add a note");
  await expect(async () => {
    await box.fill(text);
    await expect(box).toHaveValue(text, { timeout: 500 });
  }).toPass({ timeout: 10_000 });
}

test.describe("field log", () => {
  test("the operator adds a note, and an agent's note arrives live", async ({ openAs, api, project }) => {
    const op = await openAs("operator", { project: project.project_id, hash: "#/log" });
    await expect(op.getByRole("heading", { name: "Field log" }).first()).toBeVisible();
    // Unique per project, so a repeated run never finds an earlier note.
    const note = `Ladder left by the north wall (${project.project_id})`;
    const agentNote = `Agent: station 2 should see the gable (${project.project_id})`;
    await typeNote(op, note);
    await op.getByRole("button", { name: "Add to the log" }).click();
    await expect(op.getByRole("status")).toHaveText("Noted");
    const entry = op.locator(".entry", { hasText: note });
    await expect(entry).toContainText("operator-1");
    const notes = await api.viewer.get(`/log?project_id=${project.project_id}`);
    expect(notes.some((e) => e.text === note)).toBe(true);

    await api.agent.post("/log", { text: agentNote, project_id: project.project_id });
    await expect(op.locator(".entry").first()).toContainText(agentNote);
    await expect(op.locator(".entry").first()).toContainText("agent-1");
  });

  test("a viewer reads the log but can't write to it", async ({ openAs }) => {
    const viewer = await openAs("viewer", { hash: "#/log" });
    await expect(viewer.getByRole("heading", { name: "Field log" }).first()).toBeVisible();
    await expect(viewer.getByLabel("Add a note")).toHaveCount(0);
    await expect(viewer.locator(".entry").first()).toBeVisible();
  });
});

// act() redraws the page before the note is sent, so clearing only the form
// that was submitted left the new box holding the note (easy to add twice).
test("the note box is empty after adding a note", async ({ openAs, project }) => {
  const op = await openAs("operator", { project: project.project_id, hash: "#/log" });
  await typeNote(op, `Gate left open (${project.project_id})`);
  await op.getByRole("button", { name: "Add to the log" }).click();
  await expect(op.getByRole("status")).toHaveText("Noted");
  await expect(op.getByLabel("Add a note")).toHaveValue("");
});
