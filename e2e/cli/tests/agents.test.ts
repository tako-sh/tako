import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { run, TakoTerminal } from "../helpers/terminal";

let project: string;
beforeEach(async () => {
  project = await mkdtemp(join(tmpdir(), "tako-agents-"));
});
afterEach(async () => {
  await rm(project, { recursive: true, force: true });
});

describe("tako agents", () => {
  test("help explains skills, MCP, and client selection", async () => {
    const { screen, exitCode } = await run(["agents", "--help"]);
    expect(exitCode).toBe(0);
    expect(screen).toContain("skills and MCP");
    expect(screen).toContain("--agent");
    expect(screen).toContain("--yes");
  });

  test("previews project-local files without writing", async () => {
    const { screen, exitCode } = await run(["agents", "--agent", "codex", "--dry-run", "--json"], {
      cwd: project,
      env: { HOME: project },
    });
    expect(exitCode).toBe(0);
    expect(screen).toMatch(/"dry_run":\s*true/);
    expect(screen).toContain(".codex/config.toml");
    expect(await readdir(project)).toEqual([]);
  });

  test("confirmation defaults to yes and decline leaves files untouched", async () => {
    const term = TakoTerminal.spawn({
      args: ["agents", "--agent", "codex"],
      cwd: project,
      env: { HOME: project },
      cols: 120,
      rows: 40,
    });
    try {
      await term.waitForText("Set up codex?");
      expect(term.fullText()).toContain("[Y/n]");
      expect(term.fullText()).toContain(".codex/config.toml");
      term.write("n");
      term.press("\r");
      expect(await term.waitForExit()).toBe(0);
      expect(await readdir(project)).toEqual([]);
    } finally {
      await term.close();
    }
  });

  test("accepting setup installs skills and MCP", async () => {
    const term = TakoTerminal.spawn({
      args: ["agents", "--agent", "codex"],
      cwd: project,
      env: { HOME: project },
      cols: 120,
      rows: 40,
    });
    try {
      await term.waitForText("Set up codex?");
      term.press("\r");
      expect(await term.waitForExit()).toBe(0);
      const config = await readFile(join(project, ".codex/config.toml"), "utf8");
      expect(Bun.TOML.parse(config)).toHaveProperty("mcp_servers.tako.args");
      const skill = await readFile(join(project, ".agents/skills/tako/SKILL.md"), "utf8");
      expect(skill).toContain("# Working with Tako");
      expect(await readFile(join(project, "AGENTS.md"), "utf8")).toContain("tako_docs");
    } finally {
      await term.close();
    }
  });
});

test("init offers agent setup by default and installs it after acceptance", async () => {
  const bin = join(project, "bin");
  await mkdir(bin);
  await mkdir(join(project, ".codex"));
  await writeFile(join(bin, "bun"), '#!/bin/sh\nif [ "$1" = --version ]; then echo 1.4.0; fi\n');
  await chmod(join(bin, "bun"), 0o755);
  await writeFile(join(project, "bun.lock"), "");
  await writeFile(join(project, "package.json"), JSON.stringify({ name: "agent-app" }));
  await writeFile(join(project, "index.ts"), "export default () => new Response('ok');");
  const term = TakoTerminal.spawn({
    args: ["init"],
    cwd: project,
    env: { HOME: project, TAKO_HOME: join(project, ".tako"), PATH: `${bin}:${process.env.PATH}` },
    cols: 120,
    rows: 50,
    timeout: 30_000,
  });
  try {
    for (const prompt of ["Application name", "Choose a runtime:", "App root"]) {
      await term.waitForText(prompt);
      term.press("\r");
    }
    await term.waitFor(
      (screen) =>
        screen.includes("Choose a build preset:") || screen.includes("◆ Production route"),
      { timeout: 15_000 },
    );
    if (term.screenText().includes("Choose a build preset:")) {
      term.press("\r");
    }
    await term.waitForText("◆ Production route");
    term.press("\r");
    await term.waitForText("Looks good?");
    term.press("\r");
    await term.waitForText("Set up coding agents?");
    expect(term.fullText()).toContain("Set up coding agents? [Y/n]");
    term.press("\r");
    expect(await term.waitForExit()).toBe(0);
    expect(await readFile(join(project, ".codex/config.toml"), "utf8")).toContain("mcp_servers");
    expect(await readFile(join(project, ".agents/skills/tako/SKILL.md"), "utf8")).toContain(
      "# Working with Tako",
    );
  } finally {
    await term.close();
  }
}, 30_000);
