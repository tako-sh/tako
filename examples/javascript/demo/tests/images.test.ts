import { describe, expect, test } from "bun:test";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const demoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const imagesModuleUrl = pathToFileURL(path.join(demoRoot, "src/lib/images.ts")).href;

describe("demo image URLs", () => {
  test("use the optimizer route in dev mode", () => {
    const script = `
      import.meta.env.DEV = true;
      const { demoImageUrl } = await import(${JSON.stringify(imagesModuleUrl)});
      const actual = demoImageUrl("/images/europa-dock.jpg", { width: 640 });
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
