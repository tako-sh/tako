import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { TakoTerminal } from "../helpers/terminal";

let tempDir: string;
let takoHome: string;

beforeEach(async () => {
  tempDir = await mkdtemp(join(tmpdir(), "tako-cli-test-"));
  takoHome = join(tempDir, ".tako-home");
  await writeFile(
    join(tempDir, "tako.toml"),
    'name = "test-app"\nruntime = "bun"\n\n[envs.production]\nroute = "prod.example.com"\n',
  );
});

afterEach(async () => {
  await rm(tempDir, { recursive: true, force: true });
});

describe("secret expiry prompt", () => {
  test(
    "accepts blank input as no expiration",
    async () => {
      const term = TakoTerminal.spawn({
        args: ["secrets", "set", "API_KEY", "--env", "production"],
        cwd: tempDir,
        env: { HOME: tempDir, TAKO_HOME: takoHome },
      });

      await term.waitForText("Enter value for API_KEY", { timeout: 5000 });
      term.write("secret-value\r");

      await term.waitForText("Expires on", { timeout: 5000 });
      await term.waitForText("No expiration if left blank", { timeout: 5000 });
      term.press("\r");

      if (process.platform === "darwin") {
        await term.waitForText("Use iCloud Keychain?", { timeout: 5000 });
        term.press("\r");
      }

      await term.waitForText("Set API_KEY in production", { timeout: 5000 });
      expect(await term.waitForExit({ timeout: 5000 })).toBe(0);

      const secrets = JSON.parse(
        await readFile(join(tempDir, ".tako", "secrets.json"), "utf8"),
      );
      expect(secrets.production.app.API_KEY.expires_on).toBeUndefined();
    },
    15_000,
  );
});
