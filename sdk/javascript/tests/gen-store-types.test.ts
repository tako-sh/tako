import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { generateStoreTypes } from "../bin/gen-store-types";

const temporaryDirectories: string[] = [];

afterEach(async () => {
  await Promise.all(temporaryDirectories.splice(0).map((path) => rm(path, { recursive: true })));
});

async function fixture(): Promise<{ root: string; stores: string }> {
  const root = await mkdtemp(join(tmpdir(), "tako-store-types-"));
  temporaryDirectories.push(root);
  const stores = join(root, "app/stores");
  await mkdir(stores, { recursive: true });
  return { root, stores };
}

describe("gen-store-types", () => {
  test("uses declared store names and file stems without running loaders", async () => {
    const { stores } = await fixture();
    const sdk = pathToFileURL(join(import.meta.dir, "../src/index.ts")).href;
    await writeFile(
      join(stores, "chat-messages.ts"),
      `import { defineStore } from ${JSON.stringify(sdk)};
export default defineStore("messages", {
  paramsSchema: (t) => t.Object({ chatId: t.String() }),
  load: async () => { throw new Error("Codegen must never call the loader"); },
});`,
    );
    const output = await generateStoreTypes(stores);
    expect(output).toContain("export interface TakoStores");
    expect(output).toContain(
      '"messages": import("tako.sh").InferStore<typeof import("./stores/chat-messages").default>;',
    );
    expect(output).not.toContain("tako.store:");
  });

  test("resolves imports relative to an existing declaration outside app_root", async () => {
    const { root, stores } = await fixture();
    const sdk = pathToFileURL(join(import.meta.dir, "../src/index.ts")).href;
    await writeFile(
      join(stores, "profile.ts"),
      `import { defineStore } from ${JSON.stringify(sdk)};
export default defineStore("profile", { load: async () => ({ name: "Ada" }) });`,
    );
    const output = await generateStoreTypes(stores, root);
    expect(output).toContain('typeof import("./app/stores/profile").default');
    const nested = await generateStoreTypes(stores, join(root, "src"));
    expect(nested).toContain('typeof import("../app/stores/profile").default');
  });
});
