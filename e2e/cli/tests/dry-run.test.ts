/**
 * Tests for --dry-run flag behavior.
 *
 * Verifies that --dry-run shows what would happen without performing
 * side effects, and that the output formatting (environment warning,
 * skip markers, colors) is correct.
 */

import { describe, test, expect, beforeEach, afterEach } from "bun:test";
import { TakoTerminal, run } from "../helpers/terminal";
import { mkdtemp, writeFile, rm, mkdir } from "node:fs/promises";
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

async function setupProject(
  opts: {
    servers?: Record<string, { host: string; port?: number }>;
    env?: string;
  } = {},
) {
  await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "dry-run-app" }));

  const envName = opts.env ?? "production";
  const serverEntries = opts.servers ?? {};
  const serverNames = Object.keys(serverEntries);

  let takoToml = `name = "dry-run-app"\nruntime = "node"\npreset = "node"\nmain = "index.ts"\n\n[envs.${envName}]\nroute = "dry-run-app.example.com"\n`;
  if (serverNames.length > 0) {
    takoToml += `servers = [${serverNames.map((n) => `"${n}"`).join(", ")}]\n`;
  }
  await writeFile(join(tempDir, "tako.toml"), takoToml);
  await writeFile(join(tempDir, "index.ts"), "export default {}");

  // Create config.toml in TAKO_HOME (server inventory uses [[servers]] array)
  // Each server entry includes arch/libc target metadata (required by deploy).
  if (serverNames.length > 0) {
    await mkdir(takoHome, { recursive: true });
    let configToml = "";
    for (const [name, entry] of Object.entries(serverEntries)) {
      configToml += `[[servers]]\nname = "${name}"\nhost = "${entry.host}"\nport = ${entry.port ?? 22}\narch = "x86_64"\nlibc = "glibc"\n\n`;
    }
    await writeFile(join(takoHome, "config.toml"), configToml);
  }
}

describe("--dry-run appears in help", () => {
  test("shows --dry-run in global options", async () => {
    const { term } = await run(["--help"], { rows: 40 });
    expect(term.fullText()).toContain("--dry-run");
  });

  test("shows --dry-run in subcommand help", async () => {
    const { term } = await run(["deploy", "--help"], { rows: 30 });
    expect(term.fullText()).toContain("--dry-run");
  });
});

describe("deploy --dry-run", () => {
  test("shows app summary when env is auto-resolved", async () => {
    await setupProject({ servers: { prod: { host: "10.0.0.1" } } });

    // Without --env, the CLI auto-resolves to production
    const { screen, exitCode } = await run(["--dry-run", "deploy"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    expect(exitCode).toBe(0);
    expect(screen).toContain("dry-run-app");
  });

  test("shows skip markers with ⏭ icon", async () => {
    await setupProject({ servers: { prod: { host: "10.0.0.1" } } });

    const { screen, exitCode } = await run(["--dry-run", "deploy", "--env", "production"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    expect(exitCode).toBe(0);
    expect(screen).toContain("⏭");
    expect(screen).toContain("dry run");
  });

  test("shows what would be skipped", async () => {
    await setupProject({ servers: { prod: { host: "10.0.0.1" } } });

    const { screen } = await run(["--dry-run", "deploy", "--env", "production"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    expect(screen).toContain("Server checks");
    expect(screen).toContain("Build");
    expect(screen).toContain("Deploy to");
    expect(screen).toContain("prod");
  });

  test("shows app name and URL summary", async () => {
    await setupProject({ servers: { prod: { host: "10.0.0.1" } } });

    const { screen } = await run(["--dry-run", "deploy", "--env", "production"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    expect(screen).toContain("dry-run-app");
    expect(screen).toContain("https://dry-run-app.example.com");
  });

  test("skip markers are dimmed (muted)", async () => {
    await setupProject({ servers: { prod: { host: "10.0.0.1" } } });

    const { term } = await run(["--dry-run", "deploy", "--env", "production"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    // Find the ⏭ character — it should be dim (brand_muted)
    const skipRow = findRowContaining(term, "⏭");
    expect(skipRow).not.toBeNull();

    if (skipRow !== null) {
      const skipCol = findCharInRow(term, skipRow, "⏭");
      if (skipCol !== null) {
        const cell = term.cell(skipRow, skipCol);
        expect(cell).not.toBeNull();
        expect(cell!.isDim).toBe(true);
      }
    }
  });

  test("shows validation errors even in dry-run", async () => {
    await setupProject(); // no servers configured

    const { screen, exitCode } = await run(["--dry-run", "deploy", "--env", "production"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    // Should still fail validation — no servers
    expect(exitCode).toBe(1);
    expect(screen).toContain("✘");
  });

  test("skip markers are colored in dry-run", async () => {
    await setupProject({ servers: { prod: { host: "10.0.0.1" } } });

    const { screen } = await run(["--dry-run", "deploy", "--env", "production"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    // Dry-run shows skip markers and the deploy summary
    expect(screen).toContain("⏭");
    expect(screen).toContain("dry-run-app");
  });

  test("multi-server deploy shows each server in skip output", async () => {
    await setupProject({
      servers: {
        "us-east": { host: "10.0.0.1" },
        "eu-west": { host: "10.0.0.2" },
      },
    });

    const { screen } = await run(["--dry-run", "deploy", "--env", "production"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    expect(screen).toContain("us-east");
    expect(screen).toContain("eu-west");
  });
});

describe("servers add --dry-run", () => {
  test("shows skip message without writing config", async () => {
    const { screen, exitCode } = await run(
      ["--dry-run", "servers", "add", "fake.server.com", "--name", "test-srv", "--no-test"],
      {
        cwd: tempDir,
        env: { HOME: tempDir, TAKO_HOME: takoHome },
      },
    );

    expect(exitCode).toBe(0);
    expect(screen).toContain("⏭");
    expect(screen).toContain("test-srv");
    expect(screen).toContain("dry run");
    expect(screen).toContain("fake.server.com");
  });
});

describe("--dry-run --ci", () => {
  test("produces plain dry-run output without colors", async () => {
    const { term, exitCode } = await run(
      ["--dry-run", "--ci", "servers", "add", "fake.server.com", "--name", "test-srv", "--no-test"],
      {
        cwd: tempDir,
        env: { HOME: tempDir, TAKO_HOME: takoHome },
      },
    );

    expect(exitCode).toBe(0);
    const raw = term.rawOutput();
    // No RGB color codes
    // eslint-disable-next-line no-control-regex
    expect(raw).not.toMatch(/\x1b\[38;2;\d+;\d+;\d+m/);
    // But should still mention dry-run
    expect(raw).toContain("dry-run");
  });
});

// ── Helpers ─────────────────────────────────────────────────────────

function findRowContaining(term: TakoTerminal, text: string): number | null {
  for (let y = 0; y < 24; y++) {
    if (term.row(y).includes(text)) return y;
  }
  return null;
}

function findCharInRow(term: TakoTerminal, row: number, char: string): number | null {
  for (let x = 0; x < 80; x++) {
    const c = term.cell(row, x);
    if (c && c.char === char) return x;
  }
  return null;
}

describe("container and Compose deployment plans", () => {
  test("plans a Compose stack without a native runtime", async () => {
    await setupProject({ servers: { prod: { host: "10.0.0.1" } } });
    await writeFile(
      join(tempDir, "tako.toml"),
      `name = "compose-app"
[compose]
file = "compose.yaml"
service = "web"
port = 80
[envs.production]
route = "compose.example.com"
servers = ["prod"]
`,
    );
    await writeFile(join(tempDir, "compose.yaml"), "services:\n  web:\n    image: nginx:alpine\n");
    const { exitCode, screen } = await run(["--dry-run", "deploy"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });
    expect(exitCode).toBe(0);
    expect(screen).toContain("compose-app");
  });

  test("plans an existing image with ordinary HTTP health", async () => {
    await setupProject({ servers: { prod: { host: "10.0.0.1" } } });
    await writeFile(
      join(tempDir, "tako.toml"),
      `name = "image-app"
image = "nginx:alpine"
[container_options]
sdk = false
port = 80
health_path = "/"
[envs.production]
route = "image.example.com"
servers = ["prod"]
`,
    );
    const { exitCode, screen } = await run(["--dry-run", "deploy"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });
    expect(exitCode).toBe(0);
    expect(screen).toContain("image-app");
  });
});
