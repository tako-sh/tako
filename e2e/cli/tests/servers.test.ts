import { describe, test, expect, beforeEach, afterEach } from "bun:test";
import { TakoTerminal, run } from "../helpers/terminal";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

let tempDir: string;
let takoHome: string;

beforeEach(async () => {
  tempDir = await mkdtemp(join(tmpdir(), "tako-cli-test-"));
  takoHome = join(tempDir, ".tako");
});

afterEach(async () => {
  await rm(tempDir, { recursive: true, force: true });
});

function spawnServerAdd() {
  return TakoTerminal.spawn({
    args: ["servers", "add", "--no-test"],
    cwd: tempDir,
    env: { HOME: tempDir, TAKO_HOME: takoHome },
  });
}

describe("server add wizard", () => {
  test("servers add stores and lists custom public ports", async () => {
    const { exitCode, screen } = await run(
      [
        "servers",
        "add",
        "127.0.0.1",
        "--name",
        "edge",
        "--no-test",
        "--vpn",
        "tailscale",
        "--http-port",
        "8080",
        "--https-port",
        "8443",
      ],
      {
        cwd: tempDir,
        env: { HOME: tempDir, TAKO_HOME: takoHome },
      },
    );

    expect(exitCode).toBe(0);
    expect(screen).toContain("Added server");

    const config = await readFile(join(takoHome, "config.toml"), "utf8");
    expect(config).toContain('name = "edge"');
    expect(config).toContain("http_port = 8080");
    expect(config).toContain("https_port = 8443");

    const ls = await run(["servers", "ls"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });
    expect(ls.exitCode).toBe(0);
    expect(ls.screen).toContain("Public ports");
    expect(ls.screen).toContain("HTTP 8080, HTTPS 8443");
  });

  test("Ctrl+C preserves an optional prompt and its input", async () => {
    const term = spawnServerAdd();

    await term.waitForText("Server IP or hostname", { timeout: 5000 });
    term.write("127.0.0.1\r");

    await term.waitForText("SSH port", { timeout: 5000 });
    term.press("\r");

    await term.waitForText("SSH key", { timeout: 5000 });
    term.press("\r");

    await term.waitForText("VPN provider", { timeout: 5000 });
    term.press("\r");

    await term.waitForText("Server name", { timeout: 5000 });
    term.write("prod\r");

    await term.waitForText("Description", { timeout: 5000 });
    await term.waitForText("optional", { timeout: 5000 });
    term.write("staging server");
    await term.waitForText("staging server", { timeout: 5000 });

    term.press("\x03");
    await term.waitForText("Operation cancelled", { timeout: 5000 });

    const labelRow = findRowContaining(term, "Description");
    const cancelledRow = findRowContaining(term, "Operation cancelled");
    const screen = term.screenText();

    expect(labelRow).not.toBeNull();
    expect(cancelledRow).not.toBeNull();
    expect(cancelledRow).toBeGreaterThan(labelRow!);
    expect(screen).toContain("optional");
    expect(screen).toContain("› staging server");

    const exitCode = await term.waitForExit({ timeout: 5000 });
    expect([0, 130]).toContain(exitCode);
  });
});

describe("server VPN selection", () => {
  async function fakeAgents() {
    const bin = join(tempDir, "bin");
    await mkdir(bin);
    await writeFile(
      join(bin, "netbird"),
      `#!/bin/sh\nprintf '%s\\n' '{"management":{"connected":true},"netbirdIp":"100.80.0.1/16"}'\n`,
    );
    await writeFile(
      join(bin, "tailscale"),
      `#!/bin/sh\nprintf '%s\\n' '{"BackendState":"Running","TailscaleIPs":["100.64.0.1"]}'\n`,
    );
    for (const name of ["netbird", "tailscale"]) await chmod(join(bin, name), 0o755);
    return { HOME: tempDir, TAKO_HOME: takoHome, PATH: `${bin}:${process.env.PATH}` };
  }

  test("CI requires a choice when both providers are connected", async () => {
    const env = await fakeAgents();
    const result = await run(["--ci", "servers", "add", "edge.internal", "--no-test"], {
      cwd: tempDir,
      env,
    });
    expect(result.exitCode).not.toBe(0);
    expect(result.screen).toContain("Both Tailscale and NetBird are connected");
  });

  test("saved provider is reused with both agents connected", async () => {
    const env = await fakeAgents();
    const first = await run(
      ["--ci", "servers", "add", "edge.internal", "--vpn", "netbird", "--no-test"],
      { cwd: tempDir, env },
    );
    expect(first.exitCode).toBe(0);
    const again = await run(["--ci", "servers", "add", "edge.internal", "--no-test"], {
      cwd: tempDir,
      env,
    });
    expect(again.exitCode).toBe(0);
    expect(await readFile(join(takoHome, "config.toml"), "utf8")).toContain('vpn = "netbird"');
    const changed = await run(
      ["--ci", "servers", "add", "edge.internal", "--vpn", "tailscale", "--no-test"],
      { cwd: tempDir, env },
    );
    expect(changed.exitCode).not.toBe(0);
    expect(await readFile(join(takoHome, "config.toml"), "utf8")).toContain('vpn = "netbird"');
  });

  test("stores an explicitly selected NetBird provider", async () => {
    const result = await run(["servers", "add", "edge.internal", "--vpn", "netbird", "--no-test"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });
    expect(result.exitCode).toBe(0);
    expect(await readFile(join(takoHome, "config.toml"), "utf8")).toContain('vpn = "netbird"');
    const listed = await run(["servers", "list", "--json"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });
    expect(listed.exitCode).toBe(0);
    const server = JSON.parse(listed.term.rawOutput()).servers[0];
    expect(server.vpn).toBe("netbird");
    expect(server).not.toHaveProperty("network");
  });

  test("connected NetBird appears first, marked detected and preselected", async () => {
    const bin = join(tempDir, "bin");
    await mkdir(bin);
    await writeFile(
      join(bin, "netbird"),
      `#!/bin/sh\nprintf '%s\\n' '{"management":{"connected":true},"netbirdIp":"100.80.0.1/16"}'\n`,
    );
    await writeFile(join(bin, "tailscale"), "#!/bin/sh\nexit 1\n");
    for (const name of ["netbird", "tailscale"]) await chmod(join(bin, name), 0o755);
    const term = TakoTerminal.spawn({
      args: ["servers", "add", "edge.internal", "--no-test"],
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome, PATH: `${bin}:${process.env.PATH}` },
    });
    try {
      await term.waitForText("NetBird (detected)");
      const screen = term.screenText();
      expect(screen).toContain("VPN provider");
      expect(screen.indexOf("NetBird (detected)")).toBeLessThan(screen.indexOf("Tailscale"));
      term.press("\r");
      expect(await term.waitForExit()).toBe(0);
      expect(await readFile(join(takoHome, "config.toml"), "utf8")).toContain('vpn = "netbird"');
    } finally {
      term.kill();
    }
  });
});

function findRowContaining(term: TakoTerminal, text: string): number | null {
  const fullText = term.fullText();
  const lines = fullText.split("\n");
  for (let y = 0; y < lines.length; y++) {
    if (lines[y].includes(text)) return y;
  }
  return null;
}
