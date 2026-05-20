import { describe, expect, test } from "bun:test";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const demoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const basesModuleUrl = pathToFileURL(path.join(demoRoot, "src/lib/bases.ts")).href;

describe("demo image URLs", () => {
  test("use the optimizer route for base artwork", () => {
    const script = `
      const { imageUrl } = await import("tako.sh");
      const { BASE_PRESETS } = await import(${JSON.stringify(basesModuleUrl)});
      const europa = BASE_PRESETS.find((base) => base.slug === "europa-dock");
      const actual = imageUrl(europa.source, { width: 640 });
      const expected = "/_tako/image?src=%2Fimages%2Feuropa-dock.jpg&w=640";
      if (actual !== expected) {
        console.error(actual);
        process.exit(1);
      }
    `;
    const result = Bun.spawnSync({
      cmd: [process.execPath, "--eval", script],
      cwd: demoRoot,
      stderr: "pipe",
      stdout: "pipe",
    });

    expect(result.exitCode, new TextDecoder().decode(result.stderr)).toBe(0);
  });
});
