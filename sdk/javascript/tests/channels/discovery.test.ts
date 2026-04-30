import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { discoverChannels } from "../../src/channels/discovery";

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

  test("channel filename becomes the channel name", async () => {
    await writeChannel(
      "status.ts",
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel({ auth: { verify: async () => true } });`,
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
       export default defineChannel();`,
    );
    const found = await discoverChannels(join(dir, "channels"));
    expect(found[0]!.name).toBe("mission-log");
    expect(found[0]!.definition.channel).toBe("mission-log");
  });

  test("skips _ and . prefixed files", async () => {
    await writeChannel(
      "_skipped.ts",
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel();`,
    );
    await writeChannel(
      ".hidden.ts",
      `import { defineChannel } from "${sdkImportPath()}";
       export default defineChannel();`,
    );
    const found = await discoverChannels(join(dir, "channels"));
    expect(found).toEqual([]);
  });

  test("throws when default export is not a channel definition", async () => {
    await writeChannel("bad.ts", `export default { foo: "bar" };`);
    await expect(discoverChannels(join(dir, "channels"))).rejects.toThrow(
      /must default-export a defineChannel/,
    );
  });

  test("nested directories are rejected", async () => {
    await mkdir(join(dir, "channels", "chat"));
    await writeFile(join(dir, "channels", "chat", "room.ts"), "export default {};", "utf8");
    await expect(discoverChannels(join(dir, "channels"))).rejects.toThrow(
      /nested channel directory/,
    );
  });
});
