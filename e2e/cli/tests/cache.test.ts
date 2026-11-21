import { expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test("tako run serves the cache across processes and isolates local environments", async () => {
  const root = resolve(import.meta.dir, "../../..");
  const cli = process.env["TAKO_BIN"] ?? join(root, "target/debug/tako");
  const project = await mkdtemp(join(tmpdir(), "cache-run-"));
  try {
    await writeFile(join(project, "tako.toml"), 'name = "cache-run"\nruntime = "bun"\n');
    const run = async (env: string, source: string) => {
      const script = `import {cache} from ${JSON.stringify(join(root, "sdk/javascript/src/cache.ts"))}; ${source}`;
      const child = Bun.spawn([cli, "run", "--env", env, "--", process.execPath, "-e", script], {
        cwd: project,
        env: { ...process.env, TAKO_HOME: join(project, ".home"), TAKO_TELEMETRY: "0" },
        stdout: "pipe",
        stderr: "pipe",
      });
      const [code, output, error] = await Promise.all([
        child.exited,
        new Response(child.stdout).text(),
        new Response(child.stderr).text(),
      ]);
      if (code !== 0) throw Error(error || output);
      return output.trim();
    };
    await run("development", 'await cache.put("key", {v: 1}, {ttl: 120000})');
    expect(await run("development", 'console.log(JSON.stringify(await cache.get("key")))')).toBe(
      '{"v":1}',
    );
    expect(await run("staging", 'console.log(await cache.get("key"))')).toBe("undefined");
    await run("development", 'await cache.delete("key")');
    expect(await run("development", 'console.log(await cache.get("key"))')).toBe("undefined");
  } finally {
    await rm(project, { recursive: true, force: true });
  }
});
