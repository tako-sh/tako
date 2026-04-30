import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { bootstrapChannels } from "../../src/channels/bootstrap";

let appDir = "";

beforeEach(async () => {
  appDir = await mkdtemp(join(tmpdir(), "tako-ch-boot-"));
});

afterEach(async () => {
  await rm(appDir, { recursive: true, force: true });
});

function sdkImportPath(): string {
  return join(import.meta.dir, "..", "..", "src", "channels", "define.ts");
}

describe("bootstrapChannels", () => {
  test("no-op when channels/ does not exist", async () => {
    const { registry, channelCount } = await bootstrapChannels({ appDir });
    expect(channelCount).toBe(0);
    expect(registry.resolve("x")).toBeNull();
  });

  test("registers discovered channels", async () => {
    await mkdir(join(appDir, "channels"));
    await writeFile(
      join(appDir, "channels", "status.ts"),
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel({ auth: { verify: async () => true } });`,
      "utf8",
    );
    const { registry, channelCount } = await bootstrapChannels({ appDir });
    expect(channelCount).toBe(1);
    expect(registry.resolve("status")).not.toBeNull();
  });

  test("returns a fresh registry each call", async () => {
    await mkdir(join(appDir, "channels"));
    await writeFile(
      join(appDir, "channels", "status.ts"),
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel({ auth: { verify: async () => true } });`,
      "utf8",
    );
    const first = await bootstrapChannels({ appDir });
    const second = await bootstrapChannels({ appDir });
    expect(first.registry).not.toBe(second.registry);
    expect(first.registry.all.length).toBe(1);
    expect(second.registry.all.length).toBe(1);
  });
});
