/**
 * Tests for CLI output formatting: colors, bold, dim, structure.
 *
 * These tests verify that the rendered terminal output matches what
 * users actually see — including ANSI colors, bold/dim attributes,
 * and layout — by running the CLI in a real PTY and inspecting the
 * xterm.js screen buffer.
 */

import { describe, test, expect, beforeEach, afterEach } from "bun:test";
import { TakoTerminal, run } from "../helpers/terminal";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
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

// ── Brand colors from output.rs ─────────────────────────────────────

const _BRAND_GREEN = [155, 217, 179] as const; // success ✓
const BRAND_AMBER = [234, 211, 156] as const; // warning !
const BRAND_RED = [232, 163, 160] as const; // error ✗
const _ACCENT = [125, 196, 228] as const; // accent/secondary

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

/** Check if an RGB color is close enough (within tolerance per channel). */
function colorsClose(
  actual: [number, number, number],
  expected: readonly [number, number, number],
  tolerance = 5,
): boolean {
  return (
    Math.abs(actual[0] - expected[0]) <= tolerance &&
    Math.abs(actual[1] - expected[1]) <= tolerance &&
    Math.abs(actual[2] - expected[2]) <= tolerance
  );
}

// ── Tests ───────────────────────────────────────────────────────────

describe("servers ls (warning output)", () => {
  test("shows warning with colored ! marker", async () => {
    // Create a minimal tako.toml so the command runs
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "test-app" }));
    await writeFile(
      join(tempDir, "tako.toml"),
      'name = "test-app"\nruntime = "node"\n\n[envs.production]\nroute = "test.example.com"\n',
    );

    const { term, screen, exitCode } = await run(["network", "server", "ls"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    expect(exitCode).toBe(0);
    expect(screen).toContain("No servers configured");

    // Find the warning "!" marker
    const warnRow = findRowContaining(term, "No servers configured");
    expect(warnRow).not.toBeNull();

    if (warnRow !== null) {
      const bangCol = findCharInRow(term, warnRow, "!");
      expect(bangCol).not.toBeNull();

      if (bangCol !== null) {
        const rgb = term.fgRgb(warnRow, bangCol);
        expect(rgb).not.toBeNull();
        // Should be amber/warning colored
        expect(colorsClose(rgb!, BRAND_AMBER)).toBe(true);
      }
    }
  });

  test("hint text is rendered in dim", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "test-app" }));
    await writeFile(
      join(tempDir, "tako.toml"),
      'name = "test-app"\nruntime = "node"\n\n[envs.production]\nroute = "test.example.com"\n',
    );

    const { term, screen } = await run(["network", "server", "ls"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    // "Run tako network server add" hint should be present
    expect(screen).toContain("tako network server add");

    // The hint row should have dim or colored text (not default)
    const hintRow = findRowContaining(term, "tako network server add");
    if (hintRow !== null) {
      // Check that at least some cells on this row are non-default fg
      const cell = term.cell(hintRow, 4);
      if (cell) {
        // Hint text uses brand_dim — should be RGB colored
        expect(cell.isFgDefault).toBe(false);
      }
    }
  });
});

describe("--ci vs normal mode", () => {
  test("normal mode has ANSI colors, --ci mode does not", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "test-app" }));
    await writeFile(
      join(tempDir, "tako.toml"),
      'name = "test-app"\nruntime = "node"\n\n[envs.production]\nroute = "test.example.com"\n',
    );

    // Normal mode (PTY = colors enabled)
    const normal = await run(["network", "server", "ls"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    // CI mode (no colors)
    const ci = await run(["--ci", "network", "server", "ls"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    // Both should contain the same text
    expect(normal.screen).toContain("No servers configured");
    expect(ci.screen).toContain("No servers configured");

    // Normal mode raw output should have RGB escape sequences
    // eslint-disable-next-line no-control-regex
    expect(normal.term.rawOutput()).toMatch(/\x1b\[38;2;\d+;\d+;\d+m/);
    // CI mode should NOT have RGB escape sequences
    // eslint-disable-next-line no-control-regex
    expect(ci.term.rawOutput()).not.toMatch(/\x1b\[38;2;\d+;\d+;\d+m/);
  });
});

describe("--verbose mode", () => {
  test("verbose output includes timestamps on stderr", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "test-app" }));
    await writeFile(
      join(tempDir, "tako.toml"),
      'name = "test-app"\nruntime = "node"\n\n[envs.production]\nroute = "test.example.com"\n',
    );

    const { term, screen: _screen } = await run(["-v", "network", "server", "ls"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    // Verbose mode should show timestamps (HH:MM:SS.mmm format)
    const raw = term.rawOutput();
    expect(raw).toMatch(/\d{2}:\d{2}:\d{2}\.\d{3}/);
  });
});

describe("error output", () => {
  test("deploy without servers shows colored error", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "test-app" }));
    await writeFile(
      join(tempDir, "tako.toml"),
      'name = "test-app"\nruntime = "node"\n\n[envs.production]\nroute = "test.example.com"\nservers = ["nonexistent"]\n',
    );

    const { term, exitCode } = await run(["deploy", "--env", "production", "-y"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    expect(exitCode).toBe(1);

    // Find the ✗ error marker
    const _fullText = term.fullText();
    const errorRow = findRowContaining(term, "✗");
    if (errorRow !== null) {
      const errorCol = findCharInRow(term, errorRow, "✗");
      if (errorCol !== null) {
        const rgb = term.fgRgb(errorRow, errorCol);
        expect(rgb).not.toBeNull();
        // Should be red/error colored
        expect(colorsClose(rgb!, BRAND_RED)).toBe(true);
      }
    }
  });
});

describe("terminal width handling", () => {
  test("output respects narrow terminal width", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "test-app" }));
    await writeFile(
      join(tempDir, "tako.toml"),
      'name = "test-app"\nruntime = "node"\n\n[envs.production]\nroute = "test.example.com"\n',
    );

    // Spawn with narrow terminal
    const { screen } = await run(["network", "server", "ls"], {
      cols: 40,
      rows: 10,
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    // Should still contain the key text (may wrap)
    expect(screen).toContain("No servers");
  });
});
