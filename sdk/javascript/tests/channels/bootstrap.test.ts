import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
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
    const { registry, channelCount } = await bootstrapChannels({ appDir, appRoot: "." });
    expect(channelCount).toBe(0);
    expect(registry.resolve("x")).toBeNull();
  });

  test("registers discovered channels", async () => {
    await mkdir(join(appDir, "channels"));
    await writeFile(
      join(appDir, "channels", "status.ts"),
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel("status", {
  auth: { verify: async () => true } });`,
      "utf8",
    );
    const { registry, channelCount } = await bootstrapChannels({ appDir, appRoot: "." });
    expect(channelCount).toBe(1);
    expect(registry.resolve("status")).not.toBeNull();
  });

  test("registers channels by declared name even when file basename differs", async () => {
    await mkdir(join(appDir, "channels"));
    await writeFile(
      join(appDir, "channels", "status.ts"),
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel("health");`,
      "utf8",
    );
    const { registry, channelCount } = await bootstrapChannels({ appDir, appRoot: "." });
    expect(channelCount).toBe(1);
    expect(registry.resolve("health")).not.toBeNull();
    expect(registry.resolve("status")).toBeNull();
  });

  test("returns a fresh registry each call", async () => {
    await mkdir(join(appDir, "channels"));
    await writeFile(
      join(appDir, "channels", "status.ts"),
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel("status", {
  auth: { verify: async () => true } });`,
      "utf8",
    );
    const first = await bootstrapChannels({ appDir, appRoot: "." });
    const second = await bootstrapChannels({ appDir, appRoot: "." });
    expect(first.registry).not.toBe(second.registry);
    expect(first.registry.all.length).toBe(1);
    expect(second.registry.all.length).toBe(1);
  });

  test("workflow source imports use the declared channel name", async () => {
    await mkdir(join(appDir, "channels"));
    await mkdir(join(appDir, "workflows"));
    await writeFile(
      join(appDir, "channels", "mission-log.ts"),
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel("mission-log", {
  paramsSchema: (t) => t.Object({ base: t.String() }) });`,
      "utf8",
    );
    await writeFile(
      join(appDir, "workflows", "uses-channel.ts"),
      `import missionLog from "../channels/mission-log.ts";
       export default function channelName() {
         return missionLog({ base: "shackleton" }).name;
       }`,
      "utf8",
    );

    const workflowModule = (await import(
      pathToFileURL(join(appDir, "workflows", "uses-channel.ts")).href
    )) as { default: () => string };
    expect(workflowModule.default()).toBe("mission-log?base=shackleton");
  });

  test("discovers channels under the configured app root", async () => {
    await mkdir(join(appDir, "src", "channels"), { recursive: true });
    await writeFile(
      join(appDir, "src", "channels", "status.ts"),
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel("status");`,
      "utf8",
    );

    const { registry, channelCount } = await bootstrapChannels({ appDir, appRoot: "src" });

    expect(channelCount).toBe(1);
    expect(registry.resolve("status")).not.toBeNull();
  });
});
