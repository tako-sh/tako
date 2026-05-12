import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { join } from "node:path";
import { resolveAppRootDir } from "../../src/tako/app-root";

let previous: string | undefined;

beforeEach(() => {
  previous = process.env.TAKO_APP_ROOT;
});

afterEach(() => {
  if (previous === undefined) {
    delete process.env.TAKO_APP_ROOT;
  } else {
    process.env.TAKO_APP_ROOT = previous;
  }
});

describe("resolveAppRootDir", () => {
  test("defaults JavaScript app roots to src", () => {
    delete process.env.TAKO_APP_ROOT;
    expect(resolveAppRootDir("/project")).toBe(join("/project", "src"));
  });

  test("uses configured root relative to the project directory", () => {
    expect(resolveAppRootDir("/project", "app/server")).toBe(join("/project", "app/server"));
  });

  test("supports root-level JavaScript app files", () => {
    expect(resolveAppRootDir("/project", ".")).toBe("/project");
  });

  test("uses TAKO_APP_ROOT when no explicit option is passed", () => {
    process.env.TAKO_APP_ROOT = "app";
    expect(resolveAppRootDir("/project")).toBe(join("/project", "app"));
  });
});
