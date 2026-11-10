import { test, expect } from "../support/fixtures.js";
import { TOKENS, nameOf } from "../support/tokens.js";

test.describe("sign in", () => {
  test("a bad token on the sign-in form is refused", async ({ page }) => {
    await page.goto("/ui/");
    await expect(page.getByRole("heading", { name: "X9 Bridge" })).toBeVisible();
    await page.getByLabel("Token").fill("not-a-real-token");
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.getByText("That token was not accepted.")).toBeVisible();
    await expect(page.getByLabel("Token")).toBeVisible();
  });

  test("a bad token in the link is refused and taken out of the address bar", async ({ page }) => {
    await page.goto("/ui/?token=wrong-token#/log");
    await expect(page.getByText("That token was not accepted.")).toBeVisible();
    expect(new URL(page.url()).search).toBe("");
    // Then the right one on the form.
    await page.getByLabel("Token").fill(TOKENS.viewer);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.getByRole("heading", { name: "Field log" }).first()).toBeVisible();
  });

  for (const [who, says] of [
    ["viewer", "You can look but not change anything."],
    ["agent", "Your scans wait for an operator's approval."],
    ["operator", "You can approve scans and delete them."],
  ]) {
    test(`${who} signs in from the link, sees their role, and signs out`, async ({ openAs }) => {
      const page = await openAs(who);
      expect(new URL(page.url()).search).toBe("");
      await expect(page.getByRole("heading", { name: "Now", exact: true })).toBeVisible();
      // Only people who may act get the one big action.
      const scan = page.getByRole("button", { name: "New full-dome scan" });
      if (who === "viewer") await expect(scan).toHaveCount(0); else await expect(scan).toBeVisible();
      await page.getByRole("button", { name: nameOf(who) }).click();
      const sheet = page.getByRole("dialog");
      await expect(sheet).toContainText(`Signed in as ${who}.`);
      await expect(sheet).toContainText(says);
      await sheet.getByRole("button", { name: "Close" }).click();
      await expect(sheet).toHaveCount(0);
      // The token is kept in this browser: a reload stays signed in.
      await page.reload();
      await expect(page.getByRole("button", { name: nameOf(who) })).toBeVisible();
      await page.getByRole("button", { name: nameOf(who) }).click();
      await page.getByRole("dialog").getByRole("button", { name: "Sign out" }).click();
      await expect(page.getByLabel("Token")).toBeVisible();
      await page.reload();
      await expect(page.getByLabel("Token")).toBeVisible();
    });
  }

  test("Escape and a tap outside close a sheet", async ({ openAs }) => {
    const page = await openAs("operator");
    await page.getByRole("button", { name: "operator-1" }).click();
    await expect(page.getByRole("dialog")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("dialog")).toHaveCount(0);
    await page.getByRole("button", { name: "operator-1" }).click();
    await page.mouse.click(5, 400);
    await expect(page.getByRole("dialog")).toHaveCount(0);
  });
});

// boot() returns before it starts the 5 s status poll and the 1 s redraw when
// the link's token is refused, so signing in afterwards on the form leaves
// the scanner status and running jobs' elapsed time frozen until a reload.
test("after a refused link token, signing in on the form still polls the scanner", async ({ page }) => {
  let polls = 0;
  page.on("request", (r) => { if (new URL(r.url()).pathname === "/v1/workers") polls++; });
  await page.goto("/ui/?token=wrong-token");
  await expect(page.getByText("That token was not accepted.")).toBeVisible();
  await page.getByLabel("Token").fill(TOKENS.viewer);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByText("Live", { exact: true })).toBeVisible();
  const after = polls;
  // The poll runs every 5 s (workers are fetched only at sign-in and by it).
  await expect.poll(() => polls, { timeout: 8_000 }).toBeGreaterThan(after);
});
