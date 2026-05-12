/**
 * Tests for the interactive init wizard — driving through prompts
 * with keystrokes in a real PTY.
 */

import { describe, test, expect, beforeEach, afterEach } from "bun:test";
import { TakoTerminal } from "../helpers/terminal";
import { mkdtemp, writeFile, rm, readFile } from "node:fs/promises";
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

function spawnInit(extraEnv: Record<string, string> = {}) {
  return TakoTerminal.spawn({
    args: ["init"],
    cwd: tempDir,
    env: { HOME: tempDir, TAKO_HOME: takoHome, PATH: "", ...extraEnv },
  });
}

describe("init wizard interaction", () => {
  test("shows Application name prompt with detected default", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "my-cool-app" }));

    const term = spawnInit();
    await term.waitForText("Application name", { timeout: 5000 });

    const screen = term.screenText();
    expect(screen).toContain("my-cool-app");

    await term.close();
  });

  test("accepts defaults with Enter and advances to runtime selector", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "test-app" }));

    const term = spawnInit();
    await term.waitForText("Application name", { timeout: 5000 });

    // Press Enter to accept the default name
    term.press("\r");

    // Should advance to runtime selection (arrow-key menu)
    await term.waitForText("runtime", { timeout: 5000 });
    const screen = term.screenText();
    expect(screen).toContain("runtime");

    await term.close();
  });

  test("wizard prompts have colored labels", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "test-app" }));

    const term = spawnInit();
    await term.waitForText("Application name", { timeout: 5000 });

    const row = findRowContaining(term, "Application name");
    expect(row).not.toBeNull();

    if (row !== null) {
      let hasColor = false;
      for (let x = 0; x < 80; x++) {
        const cell = term.cell(row, x);
        if (cell && cell.isFgRGB) {
          hasColor = true;
          break;
        }
      }
      expect(hasColor).toBe(true);
    }

    await term.close();
  });

  test("application name prompt keeps warning above input and preserves the value on its own line", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "prompt-layout-app" }));

    const term = spawnInit();
    await term.waitForText("Application name", { timeout: 5000 });
    await term.waitForText("Name cannot be changed after the first deployment.", {
      timeout: 5000,
    });

    let labelRow = findRowContaining(term, "Application name");
    let warningRow = findRowContaining(term, "Name cannot be changed after the first deployment.");
    let valueRow = findRowContaining(term, "prompt-layout-app");

    expect(labelRow).not.toBeNull();
    expect(warningRow).toBe(labelRow! + 1);
    expect(valueRow).toBe(warningRow! + 1);
    expect(term.row(valueRow!)).toContain("› prompt-layout-app");

    const activeArrowCol = findCharInRow(term, valueRow!, "›");
    expect(activeArrowCol).toBe(0);
    if (activeArrowCol !== null) {
      const cell = term.cell(valueRow!, activeArrowCol);
      expect(cell).not.toBeNull();
      expect(cell!.isFgRGB).toBe(true);
      expect(cell!.isDim).toBe(false);
    }

    term.press("\r");
    await term.waitForText("runtime", { timeout: 5000 });

    labelRow = findRowContaining(term, "Application name");
    warningRow = findRowContaining(term, "Name cannot be changed after the first deployment.");
    valueRow = findRowContaining(term, "prompt-layout-app");

    expect(labelRow).not.toBeNull();
    expect(warningRow).toBe(labelRow! + 1);
    expect(valueRow).toBe(warningRow! + 1);
    expect(term.row(valueRow!)).toContain("› prompt-layout-app");
    expect(term.screenText()).not.toContain("Application name   prompt-layout-app");

    const doneArrowCol = findCharInRow(term, valueRow!, "›");
    expect(doneArrowCol).toBe(0);
    if (doneArrowCol !== null) {
      const cell = term.cell(valueRow!, doneArrowCol);
      expect(cell).not.toBeNull();
      expect(cell!.isDim).toBe(true);
    }

    await term.close();
  });

  test("can navigate through full wizard and create tako.toml", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "wizard-test" }));

    const term = spawnInit();

    // Step 1: Application name — accept default
    await term.waitForText("Application name", { timeout: 5000 });
    term.press("\r");

    // Step 2: Runtime selector — accept default (Enter on highlighted)
    await term.waitForText("runtime", { timeout: 5000 });
    term.press("\r");

    // Step 3: JavaScript app root — accept default
    await term.waitForText("App root", { timeout: 5000 });
    term.press("\r");

    // Step 4: Build preset — accept default when preset discovery is available.
    await term.waitFor(
      (screen) => screen.includes("Choose a build preset:") || screen.includes("Production route"),
      { timeout: 5000, label: "waitFor preset options or production route" },
    );
    if (term.screenText().includes("Choose a build preset:")) {
      term.press("\r");
    }

    // Step 5: Production route — accept the default
    await term.waitForText("Production route", { timeout: 5000 });
    term.press("\r");

    // Step 6: Confirmation — "Looks good?"
    await term.waitForText("Looks good", { timeout: 5000 });
    term.press("\r");

    // Should complete and show success
    await term.waitForText("Created tako.toml", { timeout: 5000 });

    const exitCode = await term.waitForExit({ timeout: 5000 });
    expect(exitCode).toBe(0);

    const toml = await readFile(join(tempDir, "tako.toml"), "utf-8");
    expect(toml).toContain('name = "wizard-test"');
    expect(toml).not.toMatch(/^app_root\s*=/m);
    expect(toml).toContain("[envs.production]");
    expect(toml).toContain("route =");
  });

  test("Esc moves the active init step backward while keeping later steps muted below it", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "stack-test" }));

    const term = spawnInit();

    await term.waitForText("Application name", { timeout: 5000 });
    term.press("\r");

    await term.waitForText("Choose a runtime:", { timeout: 5000 });
    term.press("\r");

    await term.waitForText("App root", { timeout: 5000 });
    term.press("\r");

    // Handle optional build preset step
    await term.waitFor(
      (screen) => screen.includes("Choose a build preset:") || screen.includes("Production route"),
      { timeout: 5000, label: "waitFor preset options or production route" },
    );
    const hasPresetStep = term.screenText().includes("Choose a build preset:");
    if (hasPresetStep) {
      term.press("\r");
    }

    await term.waitForText("Production route", { timeout: 5000 });
    expect(countRowsContaining(term, "Application name")).toBe(1);

    // Esc back: route -> preset (if present) or app root
    term.press("\x1b");
    await term.waitFor(
      (screen) =>
        (screen.includes("◆ Choose a build preset:") || screen.includes("◆ App root")) &&
        screen.includes("◇ Production route") &&
        !screen.includes("◆ Production route"),
      { timeout: 5000, label: "waitFor previous step to reactivate" },
    );

    expect(countRowsContaining(term, "Application name")).toBe(1);

    // Esc back again: if we were on preset, go to app root.
    if (hasPresetStep) {
      term.press("\x1b");
      await term.waitFor(
        (screen) => screen.includes("◆ App root") && !screen.includes("◆ Choose a build preset:"),
        { timeout: 5000, label: "waitFor app root to reactivate" },
      );
      expect(countRowsContaining(term, "Application name")).toBe(1);
    }

    await term.waitFor(
      (screen) => screen.includes("◆ App root") && !screen.includes("◆ Choose a build preset:"),
      { timeout: 5000, label: "waitFor app root to reactivate" },
    );
    expect(countRowsContaining(term, "Application name")).toBe(1);

    term.press("\x1b");
    await term.waitFor(
      (screen) =>
        screen.includes("◆ Choose a runtime:") &&
        screen.includes("◇ App root") &&
        !screen.includes("◆ App root"),
      { timeout: 5000, label: "waitFor runtime to reactivate" },
    );

    term.press("\x1b");
    await term.waitFor(
      (screen) =>
        screen.includes("◆ Application name") &&
        screen.includes("◇ Choose a runtime:") &&
        screen.includes("◇ App root") &&
        screen.includes("◇ Production route") &&
        !screen.includes("◆ Choose a runtime:"),
      { timeout: 5000, label: "waitFor application name to reactivate" },
    );

    expect(countRowsContaining(term, "Application name")).toBe(1);
    await term.close();
  });

  test("success checkmark is green after wizard completes", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "color-test" }));

    const term = spawnInit();

    // Navigate through wizard
    await term.waitForText("Application name", { timeout: 5000 });
    term.press("\r");
    await term.waitForText("runtime", { timeout: 5000 });
    term.press("\r");
    await term.waitForText("App root", { timeout: 5000 });
    term.press("\r");
    await term.waitFor(
      (screen) => screen.includes("Choose a build preset:") || screen.includes("Production route"),
      { timeout: 5000, label: "waitFor preset options or production route" },
    );
    if (term.screenText().includes("Choose a build preset:")) {
      term.press("\r");
    }
    await term.waitForText("Production route", { timeout: 5000 });
    term.write("test.example.com\r");
    await term.waitForText("Looks good", { timeout: 5000 });
    term.press("\r");

    await term.waitForText("Created tako.toml", { timeout: 5000 });
    await term.waitForExit({ timeout: 5000 });

    // Find the ✓ and verify it's green
    const checkRow = findRowContaining(term, "Created tako.toml");
    expect(checkRow).not.toBeNull();

    if (checkRow !== null) {
      const checkCol = findCharInRow(term, checkRow, "✓") ?? findCharInRow(term, checkRow, "✔");
      expect(checkCol).not.toBeNull();

      if (checkCol !== null) {
        const rgb = term.fgRgb(checkRow, checkCol);
        expect(rgb).not.toBeNull();
        // BRAND_GREEN = (155, 217, 179)
        expect(rgb![0]).toBeGreaterThan(100);
        expect(rgb![1]).toBeGreaterThan(180);
        expect(rgb![2]).toBeGreaterThan(130);
      }
    }
  });

  test("Ctrl+C exits the wizard", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "ctrl-c-test" }));

    const term = spawnInit();
    await term.waitForText("Application name", { timeout: 5000 });

    term.press("\x03");
    const exitCode = await term.waitForExit({ timeout: 5000 });

    // Should exit (either 0 for graceful or 130 for SIGINT)
    expect([0, 130]).toContain(exitCode);
  });

  test("Ctrl+C collapses an active text prompt to a cancelled summary", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "ctrl-c-test" }));

    const term = spawnInit();
    await term.waitForText("Application name", { timeout: 5000 });
    await term.waitForText("Name cannot be changed after the first deployment.", {
      timeout: 5000,
    });

    term.press("\x03");
    await term.waitForText("Operation cancelled", { timeout: 5000 });

    const labelRow = findRowContaining(term, "Application name");
    const cancelledRow = findRowContaining(term, "Operation cancelled");
    const screen = term.screenText();

    expect(labelRow).not.toBeNull();
    expect(cancelledRow).toBeGreaterThan(labelRow!);
    expect(screen).not.toContain("› ctrl-c-test");
    expect(term.rawOutput()).toContain("\x1b[9m");

    const exitCode = await term.waitForExit({ timeout: 5000 });
    expect([0, 130]).toContain(exitCode);
  });
});

// ── Helpers ─────────────────────────────────────────────────────────

function findRowContaining(term: TakoTerminal, text: string): number | null {
  const fullText = term.fullText();
  const lines = fullText.split("\n");
  for (let y = 0; y < lines.length; y++) {
    if (lines[y].includes(text)) return y;
  }
  return null;
}

function countRowsContaining(term: TakoTerminal, text: string): number {
  return term
    .screenText()
    .split("\n")
    .filter((line) => line.includes(text)).length;
}

function findCharInRow(term: TakoTerminal, row: number, char: string): number | null {
  for (let x = 0; x < 80; x++) {
    const c = term.cell(row, x);
    if (c && c.char === char) return x;
  }
  return null;
}
