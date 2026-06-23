import { describe, test, expect, beforeEach, afterEach } from "bun:test";
import { TakoTerminal, run } from "../helpers/terminal";
import { mkdtemp, writeFile, rm, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const BRAND_RED = [232, 163, 160] as const;

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

let tempDir: string;

beforeEach(async () => {
  tempDir = await mkdtemp(join(tmpdir(), "tako-cli-test-"));
});

afterEach(async () => {
  await rm(tempDir, { recursive: true, force: true });
});

describe("tako init --ci", () => {
  test("creates tako.toml in non-interactive mode", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "test-app" }));

    const takoHome = join(tempDir, ".tako");
    const { exitCode } = await run(["--ci", "init"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    expect(exitCode).toBe(0);
    const toml = await readFile(join(tempDir, "tako.toml"), "utf-8");
    expect(toml).toContain('name = "test-app"');
  });

  test("detects bun runtime from bun.lock", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "bun-app" }));
    await writeFile(join(tempDir, "bun.lock"), "");

    const takoHome = join(tempDir, ".tako");
    const { exitCode } = await run(["--ci", "init"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    expect(exitCode).toBe(0);
    const toml = await readFile(join(tempDir, "tako.toml"), "utf-8");
    expect(toml).toMatch(/^runtime = "bun@\d+\.\d+\.\d+"$/m);
  });

  test("--ci produces no ANSI color codes in output", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "test-app" }));

    const takoHome = join(tempDir, ".tako");
    const { term, exitCode } = await run(["--ci", "init"], {
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    expect(exitCode).toBe(0);
    const raw = term.rawOutput();
    // No RGB color sequences
    // eslint-disable-next-line no-control-regex
    expect(raw).not.toMatch(/\x1b\[38;2;\d+;\d+;\d+m/);
  });
});

describe("tako init (interactive wizard)", () => {
  test("overwrite confirmation keeps the input marker aligned under the label", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "wizard-app" }));
    await writeFile(join(tempDir, "tako.toml"), 'name = "existing"\n');

    const takoHome = join(tempDir, ".tako");
    const term = TakoTerminal.spawn({
      args: ["init"],
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    await term.waitForText("Overwrite?", { timeout: 5000 });

    let labelRow = findRowContaining(
      term,
      "Configuration file tako.toml already exists. Overwrite?",
    );
    let valueRow = labelRow! + 1;

    expect(labelRow).not.toBeNull();
    expect(term.row(labelRow!)).toContain("[y/N]");
    expect(term.row(valueRow)).toContain("›");
    const cursor = term.cursor();
    expect(cursor.x).toBeGreaterThan(0);

    const activeArrowCol = findCharInRow(term, valueRow, "›");
    expect(activeArrowCol).toBe(2);
    if (activeArrowCol !== null) {
      const cell = term.cell(valueRow, activeArrowCol);
      expect(cell).not.toBeNull();
      expect(cell!.isFgRGB).toBe(true);
      expect(cell!.isDim).toBe(false);
    }

    term.press("\r");
    await term.waitForText("New config name", { timeout: 5000 });

    labelRow = findRowContaining(term, "Configuration file tako.toml already exists. Overwrite?");
    valueRow = findRowContaining(term, "no");

    expect(labelRow).not.toBeNull();
    expect(term.row(labelRow!)).not.toContain("[y/N]");
    expect(valueRow).toBe(labelRow! + 1);
    expect(term.row(valueRow!)).toBe("  no");

    const doneArrowCol = findCharInRow(term, valueRow!, "›");
    expect(doneArrowCol).toBeNull();

    await term.close();
  });

  test("overwrite confirmation ctrl c shows plain cancellation below the summary", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "wizard-app" }));
    await writeFile(join(tempDir, "tako.toml"), 'name = "existing"\n');

    const takoHome = join(tempDir, ".tako");
    const term = TakoTerminal.spawn({
      args: ["init"],
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    await term.waitForText("Overwrite?", { timeout: 5000 });
    term.press("\x03");
    await term.waitForText("Operation cancelled", { timeout: 5000 });

    const labelRow = findRowContaining(
      term,
      "Configuration file tako.toml already exists. Overwrite?",
    );
    const cancelledRow = findRowContaining(term, "Operation cancelled");

    expect(labelRow).not.toBeNull();
    expect(cancelledRow).toBe(labelRow! + 2);
    expect(term.row(labelRow!)).not.toContain("[y/N]");
    expect(term.row(labelRow! + 1)).toBe("");
    expect(term.row(cancelledRow!)).toBe("Operation cancelled");
    expect(term.screenText()).not.toContain("› Operation cancelled");

    const cancelledRgb = term.fgRgb(cancelledRow!, 0);
    expect(cancelledRgb).not.toBeNull();
    expect(colorsClose(cancelledRgb!, BRAND_RED)).toBe(true);

    const exitCode = await term.waitForExit({ timeout: 5000 });
    expect([0, 130]).toContain(exitCode);
  });

  test("shows wizard prompts in PTY", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "wizard-app" }));

    const takoHome = join(tempDir, ".tako");
    const term = TakoTerminal.spawn({
      args: ["init"],
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    // The wizard starts — should show the first prompt
    await term.waitForText("Application name", { timeout: 5000 });

    // Screen should have colored output (we're in a real PTY)
    const screen = term.screenText();
    expect(screen).toContain("Application name");

    // Exit the wizard
    term.press("\x03");
    await term.close();
  });

  test("esc reactivates the previous field and keeps the next step visible", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "wizard-app" }));

    const takoHome = join(tempDir, ".tako");
    const term = TakoTerminal.spawn({
      args: ["init"],
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    await term.waitForText("Application name", { timeout: 5000 });
    term.press("\r");
    await term.waitForText("Choose a runtime:", { timeout: 5000 });

    term.press("\x1b");
    await term.waitFor(
      () => {
        const screen = term.screenText().split("\n");
        const appLines = screen.filter((line) => line.includes("Application name"));
        return (
          appLines.length === 1 &&
          screen.some((line) => line.includes("Application name")) &&
          screen.some((line) => line.includes("Runtime"))
        );
      },
      { timeout: 5000, label: "waitForBackNavigationState" },
    );

    const appRow = findRowContaining(term, "Application name");
    const runtimeRow = findRowContaining(term, "Runtime");

    expect(appRow).not.toBeNull();
    expect(runtimeRow).not.toBeNull();
    expect(runtimeRow!).toBeGreaterThan(appRow!);
    expect(term.row(runtimeRow! + 1)).toBe("›");

    const appCell = term.cell(appRow!, 0);
    const runtimeCell = term.cell(runtimeRow!, 0);
    expect(appCell?.char).toBe("◆");
    expect(appCell?.isDim).toBe(false);
    expect(runtimeCell?.char).toBe("◇");
    expect(runtimeCell?.isDim).toBe(true);

    term.press("\x03");
    await term.close();
  });

  test("select prompt keeps a blank line before the next inactive step", async () => {
    await writeFile(join(tempDir, "package.json"), JSON.stringify({ name: "wizard-app" }));

    const takoHome = join(tempDir, ".tako");
    const term = TakoTerminal.spawn({
      args: ["init"],
      cwd: tempDir,
      env: { HOME: tempDir, TAKO_HOME: takoHome },
    });

    await term.waitForText("Application name", { timeout: 5000 });
    term.press("\r");
    await term.waitForText("Choose a runtime:", { timeout: 5000 });
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

    term.press("\x1b");
    await term.waitFor(
      (screen) =>
        screen.includes("Choose a build preset:") ||
        screen.includes("App root") ||
        screen.includes("Choose a runtime:"),
      { timeout: 5000, label: "waitFor back to previous init step" },
    );
    await term.waitFor(
      () => {
        const lines = term.screenText().split("\n");
        const keyHintsRow = lines.findLastIndex((line) => line.includes("esc back"));
        return keyHintsRow >= 0 && lines[keyHintsRow + 2]?.includes("Production route");
      },
      { timeout: 5000, label: "waitForSelectFooterSpacing" },
    );

    const lines = term.screenText().split("\n");
    const keyHintsRow = lines.findLastIndex((line) => line.includes("esc back"));

    expect(keyHintsRow).toBeGreaterThanOrEqual(0);
    expect(lines[keyHintsRow + 1]).toBe("");
    expect(lines[keyHintsRow + 2]).toContain("Production route");

    term.press("\x03");
    await term.close();
  }, 15_000);
});

function findRowContaining(term: TakoTerminal, text: string): number | null {
  const fullText = term.fullText();
  const lines = fullText.split("\n");
  for (let y = 0; y < lines.length; y++) {
    if (lines[y].includes(text)) return y;
  }
  return null;
}

function findCharInRow(term: TakoTerminal, row: number, char: string): number | null {
  for (let x = 0; x < 120; x++) {
    const c = term.cell(row, x);
    if (c && c.char === char) return x;
  }
  return null;
}
