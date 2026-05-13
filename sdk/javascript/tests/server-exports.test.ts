import { test, expect } from "bun:test";

test("tako.sh/server exports image URL signing", async () => {
  const mod = await import("../src/server");
  expect(typeof mod.createImageUrl).toBe("function");
});
