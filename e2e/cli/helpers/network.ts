import { resolve } from "node:path";

/** Generate authentic offline membership with fresh device keys, never fake inventory. */
export function networkFixture(
  home: string,
  servers: Record<string, { host: string; port?: number }>,
): string {
  const binary =
    process.env.TAKO_NETWORK_FIXTURE_BIN ??
    resolve(import.meta.dirname, "../../../target/debug/examples/cli-fixture");
  const result = Bun.spawnSync([binary, home, JSON.stringify(servers)], {
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) throw new Error(result.stderr.toString());
  return result.stdout.toString().trim();
}
