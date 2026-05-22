import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { discoverChannels } from "../../src/channels/discovery";
import { expectAsyncToThrow } from "../assertions";

let dir = "";

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "tako-channels-disc-"));
  await mkdir(join(dir, "channels"));
});

afterEach(async () => {
  await rm(dir, { recursive: true, force: true });
});

function sdkImportPath(): string {
  return join(import.meta.dir, "..", "..", "src", "channels", "define.ts");
}

async function writeChannel(name: string, body: string): Promise<void> {
  await writeFile(join(dir, "channels", name), body, "utf8");
}

describe("discoverChannels", () => {
  test("returns empty when channels/ does not exist", async () => {
    const found = await discoverChannels(join(dir, "nowhere"));
    expect(found).toEqual([]);
  });

  test("declared name becomes the channel name", async () => {
    await writeChannel(
      "status.ts",
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel("status", {
  auth: { verify: async () => true } });`,
    );
    const found = await discoverChannels(join(dir, "channels"));
    expect(found).toHaveLength(1);
    expect(found[0]!.name).toBe("status");
    expect(found[0]!.definition.channel).toBe("status");
  });

  test("kebab-case filenames are preserved", async () => {
    await writeChannel(
      "mission-log.ts",
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel("mission-log");`,
    );
    const found = await discoverChannels(join(dir, "channels"));
    expect(found[0]!.name).toBe("mission-log");
    expect(found[0]!.definition.channel).toBe("mission-log");
  });

  test("declared name may differ from the file basename", async () => {
    await writeChannel(
      "mission-log.ts",
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel("expedition-feed");`,
    );
    const found = await discoverChannels(join(dir, "channels"));
    expect(found[0]!.name).toBe("expedition-feed");
    expect(found[0]!.definition.channel).toBe("expedition-feed");
  });

  test("skips _ and . prefixed files", async () => {
    await writeChannel(
      "_skipped.ts",
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel("_skipped");`,
    );
    await writeChannel(
      ".hidden.ts",
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel(".hidden");`,
    );
    const found = await discoverChannels(join(dir, "channels"));
    expect(found).toEqual([]);
  });

  test("throws when default export is not a channel definition", async () => {
    await writeChannel("bad.ts", `export default { foo: "bar" };`);
    await expectAsyncToThrow(
      () => discoverChannels(join(dir, "channels")),
      /must default-export a defineChannel/,
    );
  });

  test("throws when two files declare the same channel name", async () => {
    await writeChannel(
      "mission-log.ts",
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel("updates");`,
    );
    await writeChannel(
      "updates.ts",
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel("updates");`,
    );

    await expectAsyncToThrow(
      () => discoverChannels(join(dir, "channels")),
      /duplicate channel 'updates'/,
    );
  });

  test("nested directories are rejected", async () => {
    await mkdir(join(dir, "channels", "chat"));
    await writeFile(join(dir, "channels", "chat", "room.ts"), "export default {};", "utf8");
    await expectAsyncToThrow(
      () => discoverChannels(join(dir, "channels")),
      /nested channel directory/,
    );
  });
});
