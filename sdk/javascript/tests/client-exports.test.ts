import { test, expect } from "bun:test";
import { resolve } from "node:path";

test("tako.sh/client exports Channel", async () => {
  const mod = await import("../src/client");
  expect(mod.Channel).toBeDefined();
  expect(typeof mod.Channel).toBe("function");
  expect(mod.configureChannels).toBeDefined();
  expect(typeof mod.configureChannels).toBe("function");
});

test("tako.sh/client bundles cleanly for the browser (no node:* specifiers)", async () => {
  const result = await Bun.build({
    entrypoints: [resolve(import.meta.dir, "../src/client.ts")],
    target: "browser",
  });
  if (!result.success) {
    const messages = result.logs.map((log) => log.message).join("\n");
    throw new Error(`client.ts failed to bundle for browser:\n${messages}`);
  }
  expect(result.success).toBe(true);
});
