import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";

describe("InferChannel", () => {
  test("infers params, messages, and transport from channel exports", async () => {
    const packageRoot = join(import.meta.dir, "..", "..");
    const root = await mkdtemp(join(packageRoot, ".tmp-infer-channel-"));
    const source = join(root, "case.ts");

    await writeFile(
      source,
      `
        import { defineChannel, type InferChannel } from "../src/index.ts";

        type Equal<A, B> =
          (<T>() => T extends A ? 1 : 2) extends
          (<T>() => T extends B ? 1 : 2) ? true : false;
        type Expect<T extends true> = T;

        const missionLog = defineChannel("mission-log", {
          paramsSchema: (t) => t.Object({ base: t.String() }),
        }).$messageTypes<{ event: { text: string } }>();

        type MissionLog = InferChannel<typeof missionLog>;
        type MissionLogCheck = Expect<Equal<MissionLog, {
          params: { base: string };
          messages: { event: { text: string } };
          transport: "sse";
        }>>;

        await missionLog({ base: "shackleton" }).publish({
          type: "event",
          data: { text: "ok" },
        });
        // @ts-expect-error SSE channels do not expose WebSocket connect().
        missionLog({ base: "shackleton" }).connect();

        const chat = defineChannel("chat", {
          paramsSchema: (t) => t.Object({ roomId: t.String() }),
          handler: {
            msg: (data: { text: string }) => data,
          },
        }).$messageTypes<{ msg: { text: string } }>();

        type Chat = InferChannel<typeof chat>;
        type ChatCheck = Expect<Equal<Chat, {
          params: { roomId: string };
          messages: { msg: { text: string } };
          transport: "ws";
        }>>;

        chat({ roomId: "r1" }).connect();
      `,
      "utf8",
    );

    try {
      const tsc = join(
        packageRoot,
        "node_modules",
        ".bin",
        process.platform === "win32" ? "tsc.cmd" : "tsc",
      );
      const proc = Bun.spawn(
        [
          tsc,
          "--noEmit",
          "--ignoreConfig",
          "--strict",
          "--target",
          "ESNext",
          "--module",
          "ESNext",
          "--moduleResolution",
          "bundler",
          "--lib",
          "ESNext,DOM",
          "--types",
          "bun",
          "--allowImportingTsExtensions",
          source,
        ],
        { cwd: packageRoot, stdout: "pipe", stderr: "pipe" },
      );
      const [stdout, stderr, exitCode] = await Promise.all([
        new Response(proc.stdout).text(),
        new Response(proc.stderr).text(),
        proc.exited,
      ]);

      expect(`${stdout}${stderr}`).toBe("");
      expect(exitCode).toBe(0);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  }, 20_000);
});
