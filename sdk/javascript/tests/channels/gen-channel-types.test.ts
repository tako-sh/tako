import { mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { describe, expect, test } from "bun:test";
import { generateChannelTypes } from "../../bin/gen-channel-types";

describe("gen-channel-types", () => {
  test("emits TakoChannels for discovered channels", async () => {
    const root = await mktemp();
    const channels = join(root, "channels");
    await mkdir(channels);
    const sdk = pathToFileURL(join(process.cwd(), "src/index.ts")).href;

    await writeFile(
      join(channels, "chat.ts"),
      `
        import { defineChannel } from ${JSON.stringify(sdk)};
        export default defineChannel("chat", {
          paramsSchema: (t) => t.Object({
            roomId: t.String(),
            limit: t.Optional(t.Integer()),
          }),
          handler: {
            "chat.send": (data) => data,
          },
        }).$messageTypes<{ "chat.send": { text: string } }>();
      `,
    );
    await writeFile(
      join(channels, "status.ts"),
      `
        import { defineChannel } from ${JSON.stringify(sdk)};
        export default defineChannel("status");
      `,
    );

    const out = await generateChannelTypes(channels);

    expect(out).toContain(
      "Project-specific channel metadata discovered from `<app_root>/channels/`.",
    );
    expect(out).toContain("export interface TakoChannels");
    expect(out).toContain('Channel `"chat"` route params, message metadata, and transport.');
    expect(out).toContain(
      `"chat": import("tako.sh").InferChannel<typeof import("./channels/chat").default>;`,
    );
    expect(out).toContain('Channel `"status"` route params, message metadata, and transport.');
    expect(out).toContain(
      `"status": import("tako.sh").InferChannel<typeof import("./channels/status").default>;`,
    );
    expect(out).not.toContain("messages: Record<string, unknown>");
  });

  test("uses declared names as keys and file stems as imports", async () => {
    const root = await mktemp();
    const channels = join(root, "channels");
    await mkdir(channels);
    const sdk = pathToFileURL(join(process.cwd(), "src/index.ts")).href;

    await writeFile(
      join(channels, "mission-log.ts"),
      `
        import { defineChannel } from ${JSON.stringify(sdk)};
        export default defineChannel("expedition-feed");
      `,
    );

    const out = await generateChannelTypes(channels);

    expect(out).toContain(
      `"expedition-feed": import("tako.sh").InferChannel<typeof import("./channels/mission-log").default>;`,
    );
  });
});

async function mktemp(): Promise<string> {
  const dir = await import("node:fs/promises").then(({ mkdtemp }) =>
    mkdtemp(join(tmpdir(), "tako-channel-types-")),
  );
  return dir;
}
