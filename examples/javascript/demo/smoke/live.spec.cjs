const { expect, test } = require("@playwright/test");

test("landing page and optimized artwork are available", async ({ page, request }) => {
  const response = await page.goto("/");
  expect(response?.status(), "landing HTTP status").toBe(200);
  await expect(page.getByRole("heading", { name: "Planetary Supply Desk" })).toBeVisible();
  await expect(page.getByLabel("Enter a base name")).toBeVisible();

  const image = await request.get("/_tako/image?src=%2Fimages%2Feuropa-dock.jpg&w=640");
  expect(image.status(), "optimized image HTTP status").toBe(200);
  expect(image.headers()["content-type"]).toMatch(/^image\//);
  expect((await image.body()).length).toBeGreaterThan(100);
});

test("wildcard routing, live delivery, and persisted shipments work", async ({ page }) => {
  // One isolated base and one shipment per run; the demo's retention cron removes them.
  const base = `ci-${crypto.randomUUID()}`;
  const response = await page.goto(`https://${base}.demo.tako.sh/`);
  expect(response?.status(), "wildcard HTTP status").toBe(200);
  await expect(page.getByText("live", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "O2 Canisters", exact: true }).click();
  const shipments = page.locator("section").filter({
    has: page.getByRole("heading", { name: "Shipments", exact: true }),
  });
  await expect(shipments.getByRole("heading", { name: "O2 Canisters", exact: true })).toBeVisible();
  // No reload or polling: this must arrive through the live channel.
  await expect(
    shipments.locator('[data-slot="badge"]').filter({ hasText: /^Delivered$/ }),
  ).toBeVisible({
    timeout: 90_000,
  });
  await expect(page.getByText("Request could not be enqueued:", { exact: false })).toHaveCount(0);
  await page.reload();
  await expect(shipments.getByRole("heading", { name: "O2 Canisters", exact: true })).toBeVisible();
  await expect(
    shipments.locator('[data-slot="badge"]').filter({ hasText: /^Delivered$/ }),
  ).toBeVisible();
});
