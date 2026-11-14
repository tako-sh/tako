import { describe, test, expect, beforeEach, afterEach } from "bun:test";
import { TakoTerminal, run } from "../helpers/terminal";
import { mkdtemp, realpath, readFile, rm, writeFile, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

let tempDir: string;
let takoHome: string;
beforeEach(async () => {
  // Network persistence rejects symlink ancestors, including macOS /var.
  tempDir = await realpath(await mkdtemp(join(tmpdir(), "tako-cli-test-")));
  takoHome = join(tempDir, ".tako");
});
afterEach(async () => {
  await rm(tempDir, { recursive: true, force: true });
});
const options = () => ({ cwd: tempDir, env: { HOME: tempDir, TAKO_HOME: takoHome } });

describe("native server enrollment", () => {
  test("requires a network instead of detecting an external VPN", async () => {
    const result = await run(
      ["--ci", "network", "server", "add", "admin@edge.internal"],
      options(),
    );
    expect(result.exitCode).not.toBe(0);
    expect(result.screen).toContain("No network configured");
  });

  test("rejects removed provider flags", async () => {
    for (const provider of ["tailscale", "netbird"]) {
      const result = await run(
        ["network", "server", "add", "edge.internal", "--vpn", provider],
        options(),
      );
      expect(result.exitCode).not.toBe(0);
      expect(result.screen).toContain("unexpected argument '--vpn'");
    }
  });

  test("dry run selects the sole network without creating an inventory entry", async () => {
    expect((await run(["network", "create", "fleet"], options())).exitCode).toBe(0);
    const before = await readFile(join(takoHome, "networks", "identities.json"), "utf8");
    const result = await run(
      [
        "--dry-run",
        "network",
        "server",
        "add",
        "admin@unreachable.invalid",
        "--name",
        "edge",
        "--http-port",
        "8080",
        "--https-port",
        "8443",
      ],
      options(),
    );
    expect(result.exitCode).toBe(0);
    expect(result.screen).toContain("fleet");
    expect(result.screen).toContain("edge");
    expect(await readFile(join(takoHome, "networks", "identities.json"), "utf8")).toBe(before);
  });

  test("legacy inventory does not grant network membership", async () => {
    await mkdir(takoHome);
    await writeFile(
      join(takoHome, "config.toml"),
      "[[servers]]\nname = 'edge'\nhost = 'edge.internal'\nvpn = 'netbird'\n",
    );
    const result = await run(["network", "server", "list"], options());
    expect(result.exitCode).toBe(0);
    expect(result.screen).toContain("No servers configured");
  });

  test("Ctrl+C cancels the SSH target prompt without enrolling a server", async () => {
    expect((await run(["network", "create", "fleet"], options())).exitCode).toBe(0);
    const before = await readFile(join(takoHome, "networks", "identities.json"), "utf8");
    const term = TakoTerminal.spawn({ args: ["network", "server", "add"], ...options() });
    try {
      await term.waitForText("Admin SSH target (admin@host)");
      term.write("admin@edge");
      await term.waitForText("admin@edge");
      term.press("\x03");
      await term.waitForText("Operation cancelled");
      expect([0, 1, 130]).toContain(await term.waitForExit());
      expect(await readFile(join(takoHome, "networks", "identities.json"), "utf8")).toBe(before);
    } finally {
      term.kill();
    }
  });
});
