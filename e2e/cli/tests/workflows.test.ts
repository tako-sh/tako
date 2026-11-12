import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { run } from "../helpers/terminal";

let project: string;
let env: Record<string, string>;

beforeEach(async () => {
  project = await mkdtemp(join(tmpdir(), "tako-workflows-"));
  const takoHome = join(project, ".tako-home");
  await mkdir(takoHome);
  await writeFile(join(takoHome, "config.toml"), "");
  await writeFile(join(project, "package.json"), JSON.stringify({ name: "demo" }));
  await writeFile(
    join(project, "tako.toml"),
    'name = "demo"\nruntime = "bun"\nmain = "index.ts"\n\n[envs.production]\nroute = "demo.example.com"\n',
  );
  env = { HOME: project, TAKO_HOME: takoHome };
});

afterEach(async () => {
  await rm(project, { recursive: true, force: true });
});

describe("tako workflows", () => {
  test("help lists the run commands", async () => {
    const { screen, exitCode } = await run(["workflows", "--help"], { cols: 200 });
    expect(exitCode).toBe(0);
    expect(screen).toContain("Inspect, retry, and cancel workflow runs of a deployed environment");
    for (const command of ["list", "show", "retry", "cancel"]) {
      expect(screen).toContain(command);
    }
  });

  test("list help documents filters", async () => {
    const { screen, exitCode } = await run(["workflows", "list", "--help"], { cols: 200 });
    expect(exitCode).toBe(0);
    expect(screen).toContain("--status <STATUS>");
    expect(screen).toContain("pending, running, succeeded, cancelled, dead");
    expect(screen).toContain("--name <NAME>");
    expect(screen).toContain("1-200 (default 50)");
    expect(screen).toContain("--server <SERVER>");
  });

  test("cancel help offers --yes", async () => {
    const { screen, exitCode } = await run(["workflows", "cancel", "--help"], { cols: 200 });
    expect(exitCode).toBe(0);
    expect(screen).toContain("<RUN_ID>");
    expect(screen).toContain("--yes");
  });

  test("rejects an unknown status and an out-of-range limit", async () => {
    const status = await run(["workflows", "list", "--status", "failed"], { cols: 200 });
    expect(status.exitCode).toBe(2);
    expect(status.screen).toContain("invalid value 'failed'");
    expect(status.screen).toContain("pending, running, succeeded, cancelled, dead");

    const limit = await run(["workflows", "list", "--limit", "500"], { cols: 200 });
    expect(limit.exitCode).toBe(2);
    expect(limit.screen).toContain("1..=200");
  });

  test("run commands require a run id", async () => {
    const { screen, exitCode } = await run(["workflows", "retry"], { cols: 200 });
    expect(exitCode).toBe(2);
    expect(screen).toContain("<RUN_ID>");
  });

  test("JSON failure without servers is a structured error", async () => {
    const { screen, exitCode } = await run(["--json", "workflows", "list", "--env", "production"], {
      cwd: project,
      env,
      cols: 400,
    });
    expect(exitCode).toBe(1);
    expect(screen).toMatch(/"ok":\s*false/);
    expect(screen).toContain("No servers have been added");
  });

  test("refuses the development environment", async () => {
    const { screen, exitCode } = await run(
      ["--json", "workflows", "cancel", "run_1", "--env", "development"],
      { cwd: project, env, cols: 400 },
    );
    expect(exitCode).toBe(1);
    expect(screen).toMatch(/"ok":\s*false/);
    expect(screen).toContain("Workflow run commands only work against deployed environments.");
  });
});
