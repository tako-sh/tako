import { test, expect } from "bun:test";
import { unlinkSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

test("tako.sh/runtime exports createLogger and loadSecrets", async () => {
  const mod = await import("../src/runtime");
  expect(typeof mod.createLogger).toBe("function");
  expect(typeof mod.loadSecrets).toBe("function");
  expect(typeof mod.Logger).toBe("function");
});

test("tako.sh exports the app runtime object", async () => {
  const mod = await import("../src/index");
  expect(typeof mod.tako).toBe("object");
  expect(typeof mod.tako.logger.info).toBe("function");
  expect(mod.tako.secrets.toString()).toBe("[REDACTED]");
  expect(typeof mod.tako.storages).toBe("object");
  expect(typeof mod.tako.cache.get).toBe("function");
  expect(typeof mod.tako.cache.put).toBe("function");
  expect(typeof mod.tako.cache.delete).toBe("function");
});

test("tako.sh named runtime exports match the app runtime object", async () => {
  const mod = await import("../src/index");
  expect(mod.tako.env).toBe(mod.env);
  expect(mod.tako.port).toBe(mod.port);
  expect(mod.tako.host).toBe(mod.host);
  expect(mod.tako.build).toBe(mod.build);
  expect(mod.tako.dataDir).toBe(mod.dataDir);
  expect(mod.tako.logger).toBe(mod.logger);
  expect(mod.tako.secrets).toBe(mod.secrets);
  expect(mod.tako.storages).toBe(mod.storages);
});

test("tako.sh/runtime bundles cleanly for the browser (no node:* specifiers)", async () => {
  const result = await Bun.build({
    entrypoints: [resolve(import.meta.dir, "../src/runtime.ts")],
    target: "browser",
  });
  if (!result.success) {
    const messages = result.logs.map((log) => log.message).join("\n");
    throw new Error(`runtime.ts failed to bundle for browser:\n${messages}`);
  }
  expect(result.success).toBe(true);
});

test("tako.sh runtime import tree-shakes cleanly for the browser", async () => {
  const entrypoint = resolve(import.meta.dir, ".tmp-runtime-entry.ts");
  writeFileSync(entrypoint, 'import { tako } from "../src/index";\nconsole.log(tako.env);\n');
  try {
    const result = await Bun.build({
      entrypoints: [entrypoint],
      target: "browser",
    });
    if (!result.success) {
      const messages = result.logs.map((log) => log.message).join("\n");
      throw new Error(`index.ts failed to tree-shake for browser:\n${messages}`);
    }
    expect(result.success).toBe(true);
  } finally {
    unlinkSync(entrypoint);
  }
});
