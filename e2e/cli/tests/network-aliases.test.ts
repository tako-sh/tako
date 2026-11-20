import { describe, test, expect, beforeEach, afterEach } from "bun:test";
import { run } from "../helpers/terminal";
import { mkdtemp, realpath, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

let tempDir: string;
let takoHome: string;
beforeEach(async () => {
  // Network persistence rejects symlink ancestors, including macOS /var.
  tempDir = await realpath(await mkdtemp(join(tmpdir(), "tako-cli-test-")));
  takoHome = join(tempDir, ".tako-home");
});
afterEach(async () => {
  await rm(tempDir, { recursive: true, force: true });
});
const options = () => ({ cwd: tempDir, env: { HOME: tempDir, TAKO_HOME: takoHome } });

async function aliases(): Promise<string[]> {
  const index = JSON.parse(await readFile(join(takoHome, "networks", "identities.json"), "utf8"));
  return index.map((pointer: { alias: string }) => pointer.alias).sort();
}

describe("network aliases", () => {
  test("a taken alias needs --alias without a terminal, and rename changes it", async () => {
    expect((await run(["--ci", "network", "create", "fleet"], options())).exitCode).toBe(0);
    const taken = await run(["--ci", "network", "create", "fleet"], options());
    expect(taken.exitCode).not.toBe(0);
    expect(taken.screen).toContain("--alias");
    const second = await run(
      ["--ci", "network", "create", "fleet", "--alias", "fleet-eu"],
      options(),
    );
    expect(second.exitCode).toBe(0);
    expect(await aliases()).toEqual(["fleet", "fleet-eu"]);

    const renamed = await run(["--ci", "network", "rename", "fleet-eu", "europe"], options());
    expect(renamed.exitCode).toBe(0);
    expect(await aliases()).toEqual(["europe", "fleet"]);
    const clash = await run(["--ci", "network", "rename", "europe", "fleet"], options());
    expect(clash.exitCode).not.toBe(0);
  });

  test("there is no default network to choose", async () => {
    const result = await run(["--ci", "network", "use", "fleet"], options());
    expect(result.exitCode).not.toBe(0);
  });

  test("a project environment without a record needs --network when several networks exist", async () => {
    for (const alias of ["fleet", "fleet-eu"]) {
      const created = await run(
        ["--ci", "network", "create", "fleet", "--alias", alias],
        options(),
      );
      expect(created.exitCode).toBe(0);
    }
    await writeFile(
      join(tempDir, "tako.toml"),
      'name = "shop"\n[envs.production]\nroute = "shop.example.com"\n',
    );
    const result = await run(["--ci", "--dry-run", "deploy", "--env", "production"], options());
    expect(result.exitCode).not.toBe(0);
    expect(result.screen).toContain("several networks");
    expect(result.screen).toContain("fleet, fleet-eu");
  });

  test("tako.toml selects the configured network and reports missing membership", async () => {
    await writeFile(
      join(tempDir, "tako.toml"),
      'name = "shop"\n[envs.production]\nroute = "shop.example.com"\nnetwork = "0123456789abcdef0123456789abcdef"\n',
    );
    const result = await run(["--ci", "--dry-run", "deploy"], options());
    expect(result.exitCode).not.toBe(0);
    expect(result.screen).toContain("hasn't joined");
    expect(result.screen).toContain("0123456789abcdef0123456789abcdef");
  });
});
