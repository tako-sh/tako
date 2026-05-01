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
        export default defineChannel({
  name: "chat",
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
        export default defineChannel({ name: "status" });
      `,
    );

    const out = await generateChannelTypes(channels);

    expect(out).toContain("export interface TakoChannels");
    expect(out).toContain(`"chat": { params: { roomId: string; limit?: number; }`);
    expect(out).toContain(`transport: "ws"`);
    expect(out).toContain(`"status": { params: Record<string, never>`);
    expect(out).toContain(`transport: "sse"`);
  });
});

async function mktemp(): Promise<string> {
  const dir = await import("node:fs/promises").then(({ mkdtemp }) =>
    mkdtemp(join(tmpdir(), "tako-channel-types-")),
  );
  return dir;
}
