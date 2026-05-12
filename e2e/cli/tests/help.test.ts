import { describe, test, expect } from "bun:test";
import { run } from "../helpers/terminal";

describe("tako --help", () => {
  test("prints usage and command list", async () => {
    const { screen, exitCode } = await run(["--help"]);

    expect(exitCode).toBe(0);
    expect(screen).toContain("Usage: tako");
    expect(screen).toContain("Commands:");
    expect(screen).toContain("deploy");
    expect(screen).toContain("init");
    expect(screen).toContain("dev");
    expect(screen).toContain("servers");
    expect(screen).toContain("secrets");
    expect(screen).toContain("gen");
    expect(screen).toContain("Options:");
    expect(screen).toContain("--verbose");
    expect(screen).toContain("--ci");
  });

  test("subcommand help works", async () => {
    const { screen, exitCode } = await run(["init", "--help"]);

    expect(exitCode).toBe(0);
    expect(screen).toContain("Initialize a new tako project");
    expect(screen).toContain("Usage: tako init");
  });
});

describe("tako --version", () => {
  test("prints version number", async () => {
    const { screen, exitCode } = await run(["--version"]);

    expect(exitCode).toBe(0);
    // Version is semver-like (e.g. "0.0.0") optionally followed by -<sha7>
    expect(screen).toMatch(/\d+\.\d+\.\d+/);
  });
});

describe("tako (no args)", () => {
  test("prints help when invoked with no arguments", async () => {
    const { screen, exitCode } = await run([]);

    expect(exitCode).toBe(0);
    expect(screen).toContain("Usage: tako");
    expect(screen).toContain("Commands:");
  });
});
