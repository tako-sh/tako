import { describe, expect, test } from "bun:test";
import { existsSync, lstatSync, readFileSync, readdirSync } from "node:fs";
import { resolve } from "node:path";

const sdkRoot = resolve(import.meta.dirname, "..");

function readPackageJson(): {
  exports: Record<string, unknown>;
  files?: string[];
  peerDependencies?: Record<string, string>;
  peerDependenciesMeta?: Record<string, Record<string, unknown>>;
} {
  return JSON.parse(readFileSync(resolve(sdkRoot, "package.json"), "utf8")) as {
    exports: Record<string, unknown>;
    files?: string[];
    peerDependencies?: Record<string, string>;
    peerDependenciesMeta?: Record<string, Record<string, unknown>>;
  };
}

describe("package exports", () => {
  test("declares the vite export from dist output", () => {
    const packageJson = readPackageJson();

    expect(packageJson.exports["./vite"]).toEqual({
      types: "./dist/vite.d.mts",
      import: "./dist/vite.mjs",
    });
  });

  test("declares the Next.js export from dist output", () => {
    const packageJson = readPackageJson();

    expect(packageJson.exports["./nextjs"]).toEqual({
      types: "./dist/nextjs.d.mts",
      import: "./dist/nextjs.mjs",
    });
  });

  test("declares the browser-safe /client export", () => {
    const packageJson = readPackageJson();

    expect(packageJson.exports["./client"]).toEqual({
      types: "./dist/client.d.ts",
      import: "./dist/client.js",
    });
  });

  test("declares the browser-safe /runtime export", () => {
    const packageJson = readPackageJson();

    expect(packageJson.exports["./runtime"]).toEqual({
      types: "./dist/runtime.d.mts",
      import: "./dist/runtime.mjs",
    });
  });

  test("declares the server-only export", () => {
    const packageJson = readPackageJson();

    expect(packageJson.exports["./server"]).toEqual({
      types: "./dist/server.d.mts",
      import: "./dist/server.mjs",
    });
  });

  test("declares the /react export", () => {
    const packageJson = readPackageJson();

    expect(packageJson.exports["./react"]).toEqual({
      types: "./dist/react.d.ts",
      import: "./dist/react.js",
    });
  });

  test("declares react as an optional peer dependency", () => {
    const packageJson = readPackageJson();

    expect(packageJson.peerDependencies?.react).toBe(">=18");
    expect(packageJson.peerDependenciesMeta?.react?.optional).toBe(true);
  });

  test("publishes agent skills from the package root", () => {
    const packageJson = readPackageJson();
    const skillsDir = resolve(sdkRoot, "skills");
    const skillsStat = lstatSync(skillsDir);

    expect(packageJson.files).toContain("skills");
    expect(skillsStat.isDirectory()).toBe(true);
    expect(skillsStat.isSymbolicLink()).toBe(false);
    expect(readdirSync(skillsDir).sort()).toEqual(["tako", "tako-sdk-go", "tako-sdk-js"]);

    for (const slug of ["tako", "tako-sdk-go", "tako-sdk-js"]) {
      expect(existsSync(resolve(skillsDir, slug, "SKILL.md"))).toBe(true);
    }
  });
});
