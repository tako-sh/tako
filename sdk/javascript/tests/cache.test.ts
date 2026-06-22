import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { tako } from "../src/index";

const previousDataDir = process.env["TAKO_DATA_DIR"];
const tempDirs: string[] = [];

async function withDataDir(): Promise<void> {
  const dir = await mkdtemp(join(tmpdir(), "tako-cache-test-"));
  tempDirs.push(dir);
  process.env["TAKO_DATA_DIR"] = join(dir, "data", "app");
}

afterEach(async () => {
  if (previousDataDir === undefined) {
    delete process.env["TAKO_DATA_DIR"];
  } else {
    process.env["TAKO_DATA_DIR"] = previousDataDir;
  }

  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

describe("tako.cache", () => {
  test("get returns undefined when a key is missing", async () => {
    await withDataDir();

    const value = await tako.cache.get("profile:u_missing");

    expect(value).toBeUndefined();
  });

  test("put and get serialize and deserialize cached objects", async () => {
    await withDataDir();
    const profile = {
      id: "u_123",
      name: "Ada",
      flags: ["founder", "admin"],
      metadata: { visits: 3, disabled: false, note: null },
    };

    await tako.cache.put("profile:u_123", profile, { ttl: 60_000 });
    const cached = await tako.cache.get<typeof profile>("profile:u_123");

    expect(cached).toEqual(profile);
    expect(cached).not.toBe(profile);
  });

  test("get treats expired values as missing", async () => {
    await withDataDir();

    await tako.cache.put("weather:tokyo", "sunny", { ttl: 1 });
    await new Promise((resolve) => setTimeout(resolve, 5));

    expect(await tako.cache.get("weather:tokyo")).toBeUndefined();
  });

  test("put overwrites an existing value", async () => {
    await withDataDir();

    await tako.cache.put("settings:global", { version: 1 }, { ttl: 60_000 });
    await tako.cache.put("settings:global", { version: 2 }, { ttl: 60_000 });

    expect(await tako.cache.get("settings:global")).toEqual({ version: 2 });
  });

  test("delete removes a cached value", async () => {
    await withDataDir();

    await tako.cache.put("article:a_123", { version: 1 }, { ttl: 60_000 });
    await tako.cache.delete("article:a_123");

    expect(await tako.cache.get("article:a_123")).toBeUndefined();
  });
});
