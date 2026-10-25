import { test } from "bun:test";
import { copyFile, mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { generateStoreTypes } from "../bin/gen-store-types";

async function run(command: string[], cwd: string): Promise<void> {
  const child = Bun.spawn(command, { cwd, stdout: "pipe", stderr: "pipe" });
  const [stdout, stderr, code] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  if (code !== 0) throw new Error(`${command.join(" ")} failed (${code}):\n${stdout}${stderr}`);
}

test("published React declarations consume generated Store params and result types", async () => {
  const sdkRoot = resolve(import.meta.dir, "..");
  const root = await mkdtemp(join(tmpdir(), "tako-store-package-types-"));
  const packageDir = join(root, "package");
  const projectDir = join(root, "project");
  const stores = join(projectDir, "app/stores");
  try {
    await mkdir(packageDir);
    await mkdir(stores, { recursive: true });
    await mkdir(join(projectDir, "node_modules"));
    await copyFile(join(sdkRoot, "package.json"), join(packageDir, "package.json"));
    await symlink(join(sdkRoot, "node_modules"), join(packageDir, "node_modules"), "dir");
    await symlink(packageDir, join(projectDir, "node_modules/tako.sh"), "dir");

    // Build fresh published entrypoints in isolation. Source-only typechecks can
    // miss the declaration bundler duplicating the augmentable TakoStores type.
    await run(
      [
        process.execPath,
        "--bun",
        join(sdkRoot, "node_modules/.bin/tsdown"),
        "--out-dir",
        join(packageDir, "dist"),
      ],
      sdkRoot,
    );
    await writeFile(
      join(stores, "chat-messages.ts"),
      `import { defineStore } from "tako.sh";
export default defineStore("messages", {
  paramsSchema: (t) => t.Object({ chat: t.Object({ id: t.String() }) }),
  load: ({ params }) => ({ chatId: params.chat.id, messages: [{ text: "hello" }] }),
});`,
    );
    await writeFile(
      join(stores, "status.ts"),
      `import { defineStore } from "tako.sh";
export default defineStore("status", { load: () => ({ online: true }) });`,
    );
    const declaration = join(projectDir, "app/tako.d.ts");
    await writeFile(
      declaration,
      `import "tako.sh";\ndeclare module "tako.sh" {\n${await generateStoreTypes(stores)}\n}\n`,
    );
    const consumer = join(projectDir, "app/consumer.ts");
    await writeFile(
      consumer,
      `import { useStore } from "tako.sh/react";
const result = useStore("messages", { params: { chat: { id: "chat-a" } } });
const chatId: string | undefined = result.data?.chatId;
const message: string | undefined = result.data?.messages[0]?.text;
const online: boolean | undefined = useStore("status").data?.online;
// @ts-expect-error A registered parameterized Store requires its params.
useStore("messages");
// @ts-expect-error Nested params preserve their inferred types.
useStore("messages", { params: { chat: { id: 123 } } });
// @ts-expect-error Returned data preserves the loader's result type.
const wrong: number | undefined = result.data?.chatId;
`,
    );
    await run(
      [
        process.execPath,
        join(sdkRoot, "node_modules/typescript/bin/tsc"),
        "--ignoreConfig",
        "--noEmit",
        "--strict",
        "--skipLibCheck",
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
        consumer,
        declaration,
      ],
      sdkRoot,
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}, 60_000);
