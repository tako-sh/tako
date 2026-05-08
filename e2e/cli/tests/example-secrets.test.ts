import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { cp, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";

const TAKO_BIN =
  process.env.TAKO_BIN ?? resolve(import.meta.dirname, "..", "..", "..", "target", "debug", "tako");
const REPO_ROOT = resolve(import.meta.dirname, "..", "..", "..");
const EXAMPLE_DIRS = ["basic", "gin", "echo", "chi"] as const;
const SECRET_NAMES = ["API_KEY", "DATABASE_URL", "EXAMPLE_SECRET"] as const;

let tempDir: string;
let takoHome: string;

beforeEach(async () => {
  tempDir = await mkdtemp(join(tmpdir(), "tako-example-secrets-"));
  takoHome = join(tempDir, ".tako-home");
});

afterEach(async () => {
  await rm(tempDir, { recursive: true, force: true });
});

function runTako(args: string[], cwd: string, stdin = "") {
  const proc = Bun.spawnSync([TAKO_BIN, ...args], {
    cwd,
    env: {
      ...(process.env as Record<string, string>),
      HOME: tempDir,
      TAKO_HOME: takoHome,
    },
    stdin: Buffer.from(stdin),
    stdout: "pipe",
    stderr: "pipe",
  });

  return {
    exitCode: proc.exitCode,
    stdout: new TextDecoder().decode(proc.stdout),
    stderr: new TextDecoder().decode(proc.stderr),
  };
}

describe("Go example secrets", () => {
  test("current example secret files import with the documented passphrase", async () => {
    for (const name of EXAMPLE_DIRS) {
      const source = join(REPO_ROOT, "examples", "go", name);
      const projectDir = join(tempDir, basename(source));
      await cp(source, projectDir, { recursive: true });

      for (const envName of ["development", "production"]) {
        const result = runTako(
          [
            "--config",
            join(projectDir, "tako.toml"),
            "secrets",
            "key",
            "import",
            "--passphrase",
            "--env",
            envName,
          ],
          projectDir,
          "tako-example\n",
        );
        expect(result.exitCode, `${name}/${envName}: ${result.stdout}${result.stderr}`).toBe(0);
      }

      const list = runTako(
        ["--config", join(projectDir, "tako.toml"), "secrets", "list"],
        projectDir,
      );
      expect(list.exitCode, `${name}: ${list.stdout}${list.stderr}`).toBe(0);
      for (const secretName of SECRET_NAMES) {
        expect(list.stdout + list.stderr).toContain(secretName);
      }
      expect(list.stdout + list.stderr).toContain("DEVELOPMENT");
      expect(list.stdout + list.stderr).toContain("PRODUCTION");
    }
  }, 120_000);
});
