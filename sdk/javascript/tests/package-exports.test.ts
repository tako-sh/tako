import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
describe("package exports", () => {
  test("declares the vite export from dist output", () => {
    const packageJson = JSON.parse(
      readFileSync(resolve(import.meta.dirname, "..", "package.json"), "utf8"),
    );

    expect(packageJson.exports["./vite"]).toEqual({
      types: "./dist/vite.d.mts",
      import: "./dist/vite.mjs",
    });
  });

  test("declares the Next.js export from dist output", () => {
    const packageJson = JSON.parse(
      readFileSync(resolve(import.meta.dirname, "..", "package.json"), "utf8"),
    );

    expect(packageJson.exports["./nextjs"]).toEqual({
      types: "./dist/nextjs.d.mts",
      import: "./dist/nextjs.mjs",
    });
  });

  test("declares the browser-safe /client export", () => {
    const packageJson = JSON.parse(
      readFileSync(resolve(import.meta.dirname, "..", "package.json"), "utf8"),
    );

    expect(packageJson.exports["./client"]).toEqual({
      types: "./dist/client.d.ts",
      import: "./dist/client.js",
    });
  });

  test("declares the browser-safe /runtime export", () => {
    const packageJson = JSON.parse(
      readFileSync(resolve(import.meta.dirname, "..", "package.json"), "utf8"),
    );

    expect(packageJson.exports["./runtime"]).toEqual({
      types: "./dist/runtime.d.mts",
      import: "./dist/runtime.mjs",
    });
  });

  test("declares the server-only export", () => {
    const packageJson = JSON.parse(
      readFileSync(resolve(import.meta.dirname, "..", "package.json"), "utf8"),
    );

    expect(packageJson.exports["./server"]).toEqual({
      types: "./dist/server.d.mts",
      import: "./dist/server.mjs",
    });
  });

  test("declares the /react export", () => {
    const packageJson = JSON.parse(
      readFileSync(resolve(import.meta.dirname, "..", "package.json"), "utf8"),
    );

    expect(packageJson.exports["./react"]).toEqual({
      types: "./dist/react.d.ts",
      import: "./dist/react.js",
    });
  });

  test("declares react as an optional peer dependency", () => {
    const packageJson = JSON.parse(
      readFileSync(resolve(import.meta.dirname, "..", "package.json"), "utf8"),
    );

    expect(packageJson.peerDependencies?.react).toBe(">=18");
    expect(packageJson.peerDependenciesMeta?.react?.optional).toBe(true);
  });
});
