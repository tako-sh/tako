import { test, expect } from "bun:test";

test("tako.sh/server exports the runtime object", async () => {
  const mod = await import("../src/server");
  expect(typeof mod.tako).toBe("object");
  expect(mod.tako).toHaveProperty("secrets");
});
